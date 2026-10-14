//! Headless backend host: a `127.0.0.1` WebSocket server wrapping the same
//! `Backend` the native app uses in-process, plus static serving of the web
//! client bundle. Each WS connection gets its own backend instance.
//!
//! Auth: a random token is generated on startup and must be presented as
//! `?token=…` on the WS upgrade. The token is part of the URL we print, so the
//! served page can read it from its own location and reconnect.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use claudo_core::backend::Backend;
use claudo_protocol::{Command, ServerEvent};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc::unbounded_channel;
use tower_http::services::ServeDir;

#[derive(Clone)]
struct AppState {
    token: Arc<String>,
}

#[tokio::main]
async fn main() {
    let port: u16 = std::env::var("CLAUDO_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8787);
    // Random per run, unless pinned via env (handy for scripted testing).
    let token = std::env::var("CLAUDO_TOKEN").unwrap_or_else(|_| uuid::Uuid::new_v4().to_string());
    let static_dir: PathBuf = std::env::var_os("CLAUDO_STATIC_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("crates/frontends/egui/dist"));

    let state = AppState {
        token: Arc::new(token.clone()),
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

async fn ws_handler(
    ws: WebSocketUpgrade,
    Query(params): Query<HashMap<String, String>>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    if params.get("token").map(String::as_str) != Some(state.token.as_str()) {
        return (StatusCode::UNAUTHORIZED, "invalid token").into_response();
    }
    ws.on_upgrade(handle_socket)
}

async fn handle_socket(socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    let (events_tx, mut events_rx) = unbounded_channel::<ServerEvent>();
    let backend = Backend::new(events_tx);

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
                if let Ok(cmd) = serde_json::from_str::<Command>(&text) {
                    backend.handle(cmd);
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    send_task.abort();
}
