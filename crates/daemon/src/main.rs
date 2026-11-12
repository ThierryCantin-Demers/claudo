//! Headless backend host: a `127.0.0.1` WebSocket server wrapping the same
//! `Backend` the native app uses in-process, plus static serving of the web
//! client bundle. Every WS connection is a client of one shared backend.
//!
//! Other computers' daemons, named in `CLAUDO_MACHINES` as ssh names them, show their
//! agents beside this one's: each serves its room on a socket only its user can open, and
//! `claudo-daemon --bridge`, run over ssh, joins that socket to ssh's stdin and stdout.
//!
//! The whole room, as the page hears it, is served the same way on a second socket, which
//! `claudo-daemon --room-bridge` joins to stdin and stdout: for the COSMIC app, on this
//! computer or over ssh.
//!
//! `claudo-daemon --team` serves something else: the dibs machines named in
//! `CLAUDO_TEAM_MACHINES` (all, without it) to anyone on the network, who can only look. It
//! runs no other collector and no backend, takes no command, and needs no token, so nothing
//! of this user's agents or conversations reaches it.
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
    match std::env::args().nth(1).as_deref() {
        Some("--bridge") => return claudo_transport::bridge_at(&machine_socket()),
        Some("--room-bridge") => return claudo_transport::bridge_at(&room_socket()),
        _ => {}
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("start tokio");
    match std::env::args().nth(1).as_deref() {
        Some("--team") => runtime.block_on(serve_team()),
        _ => runtime.block_on(serve()),
    }
    ExitCode::SUCCESS
}

/// The shared machines, on every address of this computer, for anyone to look at.
async fn serve_team() {
    let port: u16 = std::env::var("CLAUDO_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8790);
    let machines: Vec<String> = std::env::var("CLAUDO_TEAM_MACHINES")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect();
    let static_dir = static_dir();
    let hub = Hub::room_only();
    claudo_core::dibs::spawn_team(hub.feed("dibs"), machines);
    let app = Router::new()
        .route("/ws", get(team_socket))
        .fallback_service(
            ServeDir::new(&static_dir)
                .precompressed_br()
                .precompressed_gzip(),
        )
        .layer(axum::middleware::from_fn(caching))
        .with_state(hub);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("bind the team page's port");
    println!("claudo team page on port {port}, for anyone on the network to look at");
    println!("  serving bundle from: {}", static_dir.display());
    axum::serve(listener, app).await.expect("serve");
}

async fn team_socket(ws: WebSocketUpgrade, State(hub): State<Hub>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| async move {
        let (mut sink, mut stream) = socket.split();
        let (events, mut sent) = unbounded_channel::<ServerEvent>();
        let _ = events.send(ServerEvent::LookOnly);
        let _client = hub.connect(events);
        let forward = tokio::spawn(async move {
            while let Some(event) = sent.recv().await {
                let Ok(json) = serde_json::to_string(&event) else {
                    break;
                };
                if sink.send(Message::Text(json.into())).await.is_err() {
                    break;
                }
            }
        });
        // Whatever the page sends is dropped: nobody on the network acts as this user.
        while let Some(Ok(message)) = stream.next().await {
            if matches!(message, Message::Close(_)) {
                break;
            }
        }
        forward.abort();
    })
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
    claudo_core::transcripts::spawn(hub.notes("transcripts"), hub.feed("subagents"));
    hub.serve(&machine_socket());
    let others = std::env::var("CLAUDO_MACHINES").unwrap_or_default();
    let others = others
        .split(',')
        .map(str::trim)
        .filter(|host| !host.is_empty())
        .map(str::to_string);
    let machines = Arc::new(Machines::spawn(others, &hub));
    serve_room(&room_socket(), hub.clone(), machines.clone());
    let state = AppState {
        token: Arc::new(token.clone()),
        hub,
        machines,
    };

    let app = Router::new()
        .route("/ws", get(ws_handler))
        .fallback_service(
            ServeDir::new(&static_dir)
                .precompressed_br()
                .precompressed_gzip(),
        )
        .layer(axum::middleware::from_fn(caching))
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

/// Where this computer's daemon serves its whole room to apps, as the page hears it.
fn room_socket() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("claudo-room.sock")
}

/// Serves the room on `path`, a socket only this user can open: one JSON line per command
/// in and per event out, each client heard as a page is. Leaves a daemon already serving.
fn serve_room(path: &Path, hub: Hub, machines: Arc<Machines>) {
    use std::io::{BufRead, BufReader, BufWriter};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::{UnixListener, UnixStream};

    if UnixStream::connect(path).is_ok() {
        return;
    }
    let _ = fs::remove_file(path);
    let listener = match UnixListener::bind(path) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("cannot serve the room on {}: {error}", path.display());
            return;
        }
    };
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let Ok(writer) = stream.try_clone() else {
                continue;
            };
            let (events, mut sent) = unbounded_channel::<ServerEvent>();
            let asker = events.clone();
            let client = hub.connect(events);
            let machines = machines.clone();
            std::thread::spawn(move || {
                let mut writer = BufWriter::new(writer);
                while let Some(event) = sent.blocking_recv() {
                    let Ok(line) = serde_json::to_string(&event) else {
                        continue;
                    };
                    if writeln!(writer, "{line}")
                        .and_then(|()| writer.flush())
                        .is_err()
                    {
                        break;
                    }
                }
            });
            std::thread::spawn(move || {
                for line in BufReader::new(stream).lines().map_while(Result::ok) {
                    if let Ok(command) = serde_json::from_str::<Command>(&line)
                        && !machines.act(&command, &asker)
                    {
                        client.send(command);
                    }
                }
            });
        }
    });
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
/// The font never changes, so the browser keeps it; everything else it checks on every load,
/// which costs a "not modified" when nothing did and brings a new build at once when one did.
async fn caching(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let kept = request.uri().path().ends_with(".ttf");
    let mut response = next.run(request).await;
    // A miss, as while an install replaces the bundle, must not be kept for a year.
    let rule = match kept && response.status().is_success() {
        true => "public, max-age=31536000, immutable",
        false => "no-cache",
    };
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static(rule),
    );
    response
}

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
