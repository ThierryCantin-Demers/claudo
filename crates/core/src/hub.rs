//! One backend shared by several frontends, each a client of it: the app's own window,
//! claudo on another computer, each tab of the daemon's web page. A thread owns the backend
//! and takes their commands in turn, so what runs behind them runs once. Beside it, the
//! control room that collectors feed, which every client hears too.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use claudo_protocol::{Command, ServerEvent};
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

    /// A new client, which hears at once what runs, and leaves when dropped.
    pub fn connect(&self, events: UnboundedSender<ServerEvent>) -> Client {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.room.lock().unwrap().connect(id, events.clone());
        let _ = self.inputs.send(Input::Connect(id, events));
        Client {
            id,
            inputs: self.inputs.clone(),
            room: self.room.clone(),
        }
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
