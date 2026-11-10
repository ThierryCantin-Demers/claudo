//! Prints the control room as the herdr collector fills it: the agents it starts with, then
//! each change for as many seconds as the first argument says (30 by default). Reads
//! herdr's socket; runs no turn.
//!   cargo run -p claudo-core --example room_probe -- 60

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use claudo_core::room::{Feed, Room};
use claudo_protocol::{Node, RoomEvent, ServerEvent};
use tokio::sync::mpsc::unbounded_channel;

fn main() {
    let seconds = std::env::args()
        .nth(1)
        .and_then(|arg| arg.parse().ok())
        .unwrap_or(30);
    let room = Arc::new(Mutex::new(Room::default()));
    claudo_core::herdr::spawn(Feed::new(room.clone(), "herdr"));
    let (events, mut heard) = unbounded_channel();
    room.lock().unwrap().connect(1, events);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(seconds));
        std::process::exit(0);
    });
    let start = Instant::now();
    while let Some(event) = heard.blocking_recv() {
        let at = start.elapsed().as_secs_f32();
        match event {
            ServerEvent::Room(RoomEvent::Snapshot(nodes)) => {
                println!("{at:5.1}s snapshot of {}", nodes.len())
            }
            ServerEvent::Room(RoomEvent::Upserted(node)) => println!("{at:5.1}s {}", line(&node)),
            ServerEvent::Room(RoomEvent::Removed(id)) => println!("{at:5.1}s removed {id}"),
            _ => {}
        }
    }
}

fn line(node: &Node) -> String {
    let repo = node
        .repo
        .as_deref()
        .and_then(|repo| repo.rsplit('/').next());
    format!(
        "{:<28} {:<10} {:<20} {:?} {}",
        node.name.as_deref().unwrap_or("(unnamed)"),
        format!(
            "{:?}",
            node.state.unwrap_or(claudo_protocol::NodeState::Unknown)
        ),
        repo.unwrap_or("-"),
        node.kind,
        node.doing.as_deref().unwrap_or(""),
    )
}
