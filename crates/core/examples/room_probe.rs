//! Prints the control room as the collectors fill it: the nodes it starts with, then each
//! change for as many seconds as the first argument says (30 by default). Reads herdr's
//! socket, the `agent-runs` folders, the board, `dibs --status` and the transcripts; runs
//! no turn.
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
    claudo_core::runs::spawn(Feed::new(room.clone(), "runs"));
    claudo_core::board::spawn(Feed::notes(room.clone(), "board"));
    claudo_core::dibs::spawn(Feed::new(room.clone(), "dibs"));
    claudo_core::transcripts::spawn(Feed::notes(room.clone(), "transcripts"));
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
    let mut badges = Vec::new();
    if let Some(tokens) = node.context {
        badges.push(format!("context {tokens}"));
    }
    if let Some(pr) = node.note.as_ref().and_then(|note| note.pr.as_deref()) {
        badges.push(format!("pr {pr}"));
    }
    if let Some(parent) = &node.parent {
        badges.push(format!("under {parent}"));
    }
    format!(
        "{:<28} {:<10} {:<20} {:?} {} [{}]",
        node.name.as_deref().unwrap_or("(unnamed)"),
        format!(
            "{:?}",
            node.state.unwrap_or(claudo_protocol::NodeState::Unknown)
        ),
        repo.unwrap_or("-"),
        node.kind,
        node.doing.as_deref().unwrap_or(""),
        badges.join(", "),
    )
}
