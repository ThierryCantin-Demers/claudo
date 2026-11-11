//! Headless backend host: a `127.0.0.1` WebSocket server wrapping the same
//! `Backend` the native app uses in-process, plus static serving of the web
//! client bundle. Every WS connection is a client of one shared backend.
//!
//! Other computers' daemons, named in `CLAUDO_MACHINES` as ssh names them, show their
//! agents beside this one's: each serves its room on a socket only its user can open, and
//! `claudo-daemon --bridge`, run over ssh, joins that socket to ssh's stdin and stdout.
//!
//! Auth: a token, made on the first start and kept in claudo's config so the page's
//! link survives restarts, must be presented as `?token=…` on the WS upgrade. The
//! token is part of the URL we print, so the served page can read it from its own
//! location and reconnect.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::net::SocketAddr;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use claudo_core::hub::Hub;
use claudo_protocol::{Command, ServerEvent};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc::unbounded_channel;
use tower_http::services::ServeDir;

use crate::machines::Machines;

mod machines;

#[derive(Clone)]
struct AppState {
    token: Arc<String>,
    hub: Hub,
    machines: Arc<Machines>,
}

fn main() -> ExitCode {
    if std::env::args().nth(1).as_deref() == Some("--bridge") {
        return claudo_transport::bridge_at(&machine_socket());
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("start tokio");
    runtime.block_on(serve());
    ExitCode::SUCCESS
}

async fn serve() {
    let port: u16 = std::env::var("CLAUDO_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8787);
    let token = std::env::var("CLAUDO_TOKEN").unwrap_or_else(|_| kept_token());
    let static_dir = static_dir();

    let hub = Hub::spawn();
    claudo_core::herdr::spawn(hub.feed("herdr"));
    claudo_core::runs::spawn(hub.feed("runs"));
    claudo_core::board::spawn(hub.notes("board"));
    // dibs says the same from every computer, so one whose daemon another shows can skip it.
    if std::env::var("CLAUDO_DIBS").as_deref() != Ok("off") {
        claudo_core::dibs::spawn(hub.feed("dibs"));
    }
    claudo_core::transcripts::spawn(hub.notes("transcripts"));
    hub.serve(&machine_socket());
    let others = std::env::var("CLAUDO_MACHINES").unwrap_or_default();
    let others = others
        .split(',')
        .map(str::trim)
        .filter(|host| !host.is_empty())
        .map(str::to_string);
    let machines = Arc::new(Machines::spawn(others, &hub));
    let state = AppState {
        token: Arc::new(token.clone()),
        hub,
        machines,
    };

    let app = Router::new()
        .route("/ws", get(ws_handler))
        .fallback_service(ServeDir::new(&static_dir))
        .with_state(state);

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("bind 127.0.0.1");

    println!("claudo daemon listening on http://127.0.0.1:{port}");
    println!("  web client:  http://127.0.0.1:{port}/?token={token}");
    println!("  websocket:   ws://127.0.0.1:{port}/ws?token={token}");
    println!("  serving bundle from: {}", static_dir.display());

    axum::serve(listener, app).await.expect("serve");
}

/// Where this computer's daemon serves its room to other computers' daemons.
fn machine_socket() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("claudo-daemon.sock")
}

/// The token in claudo's config, made there on the first start.
fn kept_token() -> String {
    let Some(path) =
        home_dir("XDG_CONFIG_HOME", ".config").map(|dir| dir.join("claudo/daemon-token"))
    else {
        return uuid::Uuid::new_v4().to_string();
    };
    if let Ok(kept) = fs::read_to_string(&path)
        && !kept.trim().is_empty()
    {
        return kept.trim().to_string();
    }
    let token = uuid::Uuid::new_v4().to_string();
    let written = path
        .parent()
        .map_or(Ok(()), fs::create_dir_all)
        .and_then(|()| {
            fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&path)
        })
        .and_then(|mut file| file.write_all(token.as_bytes()));
    if let Err(error) = written {
        eprintln!("cannot keep the token in {}: {error}", path.display());
    }
    token
}

/// `CLAUDO_STATIC_DIR`, else the bundle `cargo xtask web` builds when run from the repo,
/// else the one `cargo xtask install-daemon` installs.
fn static_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("CLAUDO_STATIC_DIR") {
        return dir.into();
    }
    let built = PathBuf::from("crates/frontends/egui/dist");
    if built.is_dir() {
        return built;
    }
    home_dir("XDG_DATA_HOME", ".local/share")
        .map(|dir| dir.join("claudo/web"))
        .unwrap_or(built)
}

/// An XDG directory: its variable, else `fallback` under the home directory.
fn home_dir(variable: &str, fallback: &str) -> Option<PathBuf> {
    std::env::var_os(variable)
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(fallback)))
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    Query(params): Query<HashMap<String, String>>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    if params.get("token").map(String::as_str) != Some(state.token.as_str()) {
        return (StatusCode::UNAUTHORIZED, "invalid token").into_response();
    }
    ws.on_upgrade(move |socket| handle_socket(socket, state.hub, state.machines))
}

async fn handle_socket(socket: WebSocket, hub: Hub, machines: Arc<Machines>) {
    let (mut sink, mut stream) = socket.split();
    let (events_tx, mut events_rx) = unbounded_channel::<ServerEvent>();
    let asker = events_tx.clone();
    let client = hub.connect(events_tx);

    // Forward backend events to the socket.
    let send_task = tokio::spawn(async move {
        while let Some(event) = events_rx.recv().await {
            let Ok(json) = serde_json::to_string(&event) else {
                break;
            };
            if sink.send(Message::Text(json.into())).await.is_err() {
                break;
            }
        }
    });

    // Read commands from the socket and hand them to the backend.
    while let Some(Ok(msg)) = stream.next().await {
        match msg {
            Message::Text(text) => {
                if let Ok(cmd) = serde_json::from_str::<Command>(&text)
                    && !machines.act(&cmd, &asker)
                {
                    client.send(cmd);
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    send_task.abort();
}
