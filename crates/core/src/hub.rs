//! One backend shared by several frontends, each a client of it: the app's own window,
//! claudo on another computer, each tab of the daemon's web page. A thread owns the backend
//! and takes their commands in turn, so what runs behind them runs once. Beside it, the
//! control room that collectors feed, which every client hears too.

use std::io::{BufRead, BufReader, BufWriter, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use claudo_protocol::{Command, PROTOCOL_VERSION, ServerEvent};
use tokio::sync::mpsc::UnboundedSender;

use crate::backend::Backend;
use crate::clients::{ClientId, LOCAL};
use crate::room::{Feed, Room};

/// What reaches the thread that owns the backend.
pub enum Input {
    Command(ClientId, Command),
    Connect(ClientId, UnboundedSender<ServerEvent>),
    Disconnect(ClientId),
}

/// Hands each input to `backend`, until every sender is gone.
pub fn run(backend: Backend, inputs: Receiver<Input>) {
    for input in inputs {
        match input {
            Input::Command(client, command) => backend.handle_from(client, command),
            Input::Connect(client, events) => backend.connect(client, events),
            Input::Disconnect(client) => backend.disconnect(client),
        }
    }
}

/// A backend no frontend was made with, which every client joins alike.
#[derive(Clone)]
pub struct Hub {
    inputs: Sender<Input>,
    next: Arc<AtomicU64>,
    room: Arc<Mutex<Room>>,
}

impl Hub {
    pub fn spawn() -> Self {
        let (inputs, received) = channel();
        std::thread::spawn(move || run(Backend::headless(), received));
        Self {
            inputs,
            next: Arc::new(AtomicU64::new(LOCAL + 1)),
            room: Arc::default(),
        }
    }

    /// What the collector named `source` gives the room.
    pub fn feed(&self, source: &'static str) -> Feed {
        Feed::new(self.room.clone(), source)
    }

    /// What the collector named `source` adds to the nodes the others give.
    pub fn notes(&self, source: &'static str) -> Feed {
        Feed::notes(self.room.clone(), source)
    }

    /// Another computer's room, named `source`.
    pub fn machine(&self, source: &'static str) -> Feed {
        Feed::machine(self.room.clone(), source)
    }

    /// A new client, which hears at once what runs, and leaves when dropped.
    pub fn connect(&self, events: UnboundedSender<ServerEvent>) -> Client {
        self.join(events, false)
    }

    fn join(&self, events: UnboundedSender<ServerEvent>, machine: bool) -> Client {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let _ = events.send(ServerEvent::Hello {
            version: PROTOCOL_VERSION,
        });
        let mut room = self.room.lock().unwrap();
        if machine {
            room.connect_machine(id, events.clone());
        } else {
            room.connect(id, events.clone());
        }
        drop(room);
        let _ = self.inputs.send(Input::Connect(id, events));
        Client {
            id,
            inputs: self.inputs.clone(),
            room: self.room.clone(),
        }
    }

    /// Serves other computers' daemons on `path`, a socket only this user can open, which
    /// they reach over ssh: one JSON line per command in and per event out. Each hears the
    /// room as this computer's own collectors see it. Leaves a daemon already serving there.
    pub fn serve(&self, path: &Path) {
        if UnixStream::connect(path).is_ok() {
            return;
        }
        let _ = std::fs::remove_file(path);
        let listener = match UnixListener::bind(path) {
            Ok(listener) => listener,
            Err(error) => {
                eprintln!(
                    "cannot serve other computers on {}: {error}",
                    path.display()
                );
                return;
            }
        };
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        let hub = self.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                hub.machine_client(stream);
            }
        });
    }

    fn machine_client(&self, stream: UnixStream) {
        let Ok(writer) = stream.try_clone() else {
            return;
        };
        let (events, mut sent) = tokio::sync::mpsc::unbounded_channel();
        let client = self.join(events, true);
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
                if let Ok(command) = serde_json::from_str(&line) {
                    client.send(command);
                }
            }
        });
    }
}

pub struct Client {
    id: ClientId,
    inputs: Sender<Input>,
    room: Arc<Mutex<Room>>,
}

impl Client {
    pub fn send(&self, command: Command) {
        let _ = self.inputs.send(Input::Command(self.id, command));
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.room.lock().unwrap().disconnect(self.id);
        let _ = self.inputs.send(Input::Disconnect(self.id));
    }
}
