//! Carries the protocol between a frontend and the backend. The frontend only
//! ever sees the `Transport` trait: it sends `Command`s and drains
//! `ServerEvent`s, oblivious to whether the backend is in-process or across a
//! WebSocket.
//!
//! - `InProc` (native): backend runs on a thread in the same process.
//! - `WsClient` (wasm): talks to the daemon's WebSocket server.

use claudo_protocol::{Command, ServerEvent};

pub trait Transport {
    fn send(&mut self, cmd: Command);
    /// Non-blocking drain of any events that have arrived since the last call.
    fn poll(&mut self) -> Vec<ServerEvent>;
}

#[cfg(not(target_arch = "wasm32"))]
mod inproc {
    use super::*;
    use std::sync::mpsc::{Sender, channel};

    use claudo_core::backend::Backend;
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    /// Bundled-desktop transport: the backend runs on a worker thread and talks
    /// to the UI over in-memory channels (no socket, no port).
    pub struct InProc {
        commands: Sender<Command>,
        events: UnboundedReceiver<ServerEvent>,
    }

    impl InProc {
        pub fn spawn() -> Self {
            let (cmd_tx, cmd_rx) = channel::<Command>();
            let (evt_tx, evt_rx) = unbounded_channel::<ServerEvent>();
            std::thread::spawn(move || {
                let backend = Backend::new(evt_tx);
                while let Ok(cmd) = cmd_rx.recv() {
                    backend.handle(cmd);
                }
            });
            Self {
                commands: cmd_tx,
                events: evt_rx,
            }
        }
    }

    impl Transport for InProc {
        fn send(&mut self, cmd: Command) {
            let _ = self.commands.send(cmd);
        }

        fn poll(&mut self) -> Vec<ServerEvent> {
            let mut out = Vec::new();
            while let Ok(ev) = self.events.try_recv() {
                out.push(ev);
            }
            out
        }
    }

    /// Command sender for the event-driven (async) integration used by the
    /// libcosmic frontend. Cloneable and cheap to pass around.
    #[derive(Clone, Debug)]
    pub struct Commands(Sender<Command>);

    impl Commands {
        pub fn send(&self, cmd: Command) {
            let _ = self.0.send(cmd);
        }
    }

    /// Async event source: `recv().await` yields backend events as they arrive.
    pub struct Events(UnboundedReceiver<ServerEvent>);

    impl Events {
        pub async fn recv(&mut self) -> Option<ServerEvent> {
            self.0.recv().await
        }

        /// Non-blocking: take one already-queued event, if any. Lets a consumer
        /// coalesce a burst of events into a single render.
        pub fn try_recv(&mut self) -> Option<ServerEvent> {
            self.0.try_recv().ok()
        }
    }

    /// Spawn the backend and return an async command/event pair. Unlike
    /// `InProc` (poll-based), this suits iced's stream-based subscriptions.
    pub fn spawn_backend() -> (Commands, Events) {
        let (cmd_tx, cmd_rx) = channel::<Command>();
        let (evt_tx, evt_rx) = unbounded_channel::<ServerEvent>();
        std::thread::spawn(move || {
            let backend = Backend::new(evt_tx);
            while let Ok(cmd) = cmd_rx.recv() {
                backend.handle(cmd);
            }
        });
        (Commands(cmd_tx), Events(evt_rx))
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub use inproc::{Commands, Events, InProc, spawn_backend};

/// Connect a frontend to its backend. Native bundles the backend in-process.
#[cfg(not(target_arch = "wasm32"))]
pub fn connect() -> Box<dyn Transport> {
    Box::new(InProc::spawn())
}

#[cfg(target_arch = "wasm32")]
mod ws {
    use super::*;
    use ewebsock::{WsEvent, WsMessage, WsReceiver, WsSender};

    /// Web transport: JSON-over-WebSocket to the daemon. Commands sent before the
    /// socket opens are buffered and flushed on `Opened`.
    pub struct WsClient {
        sender: WsSender,
        receiver: WsReceiver,
        open: bool,
        pending: Vec<Command>,
    }

    impl WsClient {
        pub fn connect(url: &str) -> Self {
            let (sender, receiver) =
                ewebsock::connect(url, ewebsock::Options::default()).expect("open websocket");
            Self {
                sender,
                receiver,
                open: false,
                pending: Vec::new(),
            }
        }

        fn ship(sender: &mut WsSender, cmd: &Command) {
            if let Ok(text) = serde_json::to_string(cmd) {
                sender.send(WsMessage::Text(text));
            }
        }
    }

    impl Transport for WsClient {
        fn send(&mut self, cmd: Command) {
            if self.open {
                Self::ship(&mut self.sender, &cmd);
            } else {
                self.pending.push(cmd);
            }
        }

        fn poll(&mut self) -> Vec<ServerEvent> {
            let mut out = Vec::new();
            while let Some(event) = self.receiver.try_recv() {
                match event {
                    WsEvent::Opened => {
                        self.open = true;
                        for cmd in std::mem::take(&mut self.pending) {
                            Self::ship(&mut self.sender, &cmd);
                        }
                    }
                    WsEvent::Message(WsMessage::Text(text)) => {
                        if let Ok(ev) = serde_json::from_str::<ServerEvent>(&text) {
                            out.push(ev);
                        }
                    }
                    WsEvent::Message(_) => {}
                    WsEvent::Error(detail) => out.push(ServerEvent::Error { detail }),
                    WsEvent::Closed => out.push(ServerEvent::Error {
                        detail: "connection to daemon closed".to_string(),
                    }),
                }
            }
            out
        }
    }
}

#[cfg(target_arch = "wasm32")]
pub use ws::WsClient;

/// Connect a frontend to its backend. Wasm dials the daemon's WebSocket, reading
/// host + `?token=…` from the page's own URL.
#[cfg(target_arch = "wasm32")]
pub fn connect() -> Box<dyn Transport> {
    let location = web_sys::window().expect("window").location();
    let scheme = if location.protocol().map(|p| p == "https:").unwrap_or(false) {
        "wss"
    } else {
        "ws"
    };
    let host = location.host().unwrap_or_default();
    let search = location.search().unwrap_or_default(); // "?token=…"
    let url = format!("{scheme}://{host}/ws{search}");
    Box::new(ws::WsClient::connect(&url))
}
