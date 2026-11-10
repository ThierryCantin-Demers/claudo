//! The control room's graph. Each collector replaces its own nodes whenever what it watches
//! changes; the room merges them by id, the fields of the collector that joined first
//! first, and tells every client only what changed.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use claudo_protocol::{Node, NodeId, RoomEvent, ServerEvent};
use tokio::sync::mpsc::UnboundedSender;

use crate::clients::{ClientId, Clients};

#[derive(Default)]
pub struct Room {
    /// Each collector's nodes, in the order collectors joined.
    sources: Vec<(&'static str, Vec<Node>)>,
    shown: BTreeMap<NodeId, Node>,
    clients: Clients,
}

impl Room {
    pub fn connect(&mut self, client: ClientId, events: UnboundedSender<ServerEvent>) {
        self.clients.add(client, events);
        let nodes = self.shown.values().cloned().collect();
        self.clients
            .to(client, ServerEvent::Room(RoomEvent::Snapshot(nodes)));
    }

    pub fn disconnect(&mut self, client: ClientId) {
        self.clients.remove(client);
    }

    fn replace(&mut self, source: &'static str, nodes: Vec<Node>) {
        match self.sources.iter_mut().find(|(name, _)| *name == source) {
            Some((_, known)) => *known = nodes,
            None => self.sources.push((source, nodes)),
        }
        let next = merged(&self.sources);
        for id in self.shown.keys().filter(|id| !next.contains_key(*id)) {
            self.clients
                .send(ServerEvent::Room(RoomEvent::Removed(id.clone())));
        }
        for (id, node) in &next {
            if self.shown.get(id) != Some(node) {
                self.clients
                    .send(ServerEvent::Room(RoomEvent::Upserted(node.clone())));
            }
        }
        self.shown = next;
    }
}

fn merged(sources: &[(&'static str, Vec<Node>)]) -> BTreeMap<NodeId, Node> {
    let mut nodes: BTreeMap<NodeId, Node> = BTreeMap::new();
    for node in sources.iter().flat_map(|(_, nodes)| nodes) {
        match nodes.get_mut(&node.id) {
            Some(known) => known.fill(node.clone()),
            None => {
                nodes.insert(node.id.clone(), node.clone());
            }
        }
    }
    nodes
}

/// What one collector gives the room.
#[derive(Clone)]
pub struct Feed {
    room: Arc<Mutex<Room>>,
    source: &'static str,
}

impl Feed {
    pub fn new(room: Arc<Mutex<Room>>, source: &'static str) -> Self {
        Self { room, source }
    }

    /// Everything this collector sees now, in place of what it saw before.
    pub fn replace(&self, nodes: Vec<Node>) {
        self.room.lock().unwrap().replace(self.source, nodes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claudo_protocol::NodeState;
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    fn node(id: &str) -> Node {
        Node {
            id: id.to_string(),
            ..Node::default()
        }
    }

    fn heard(events: &mut UnboundedReceiver<ServerEvent>) -> Vec<String> {
        std::iter::from_fn(|| events.try_recv().ok())
            .map(|event| match event {
                ServerEvent::Room(RoomEvent::Snapshot(nodes)) => {
                    format!("snapshot {}", nodes.len())
                }
                ServerEvent::Room(RoomEvent::Upserted(node)) => format!("upserted {}", node.id),
                ServerEvent::Room(RoomEvent::Removed(id)) => format!("removed {id}"),
                _ => unreachable!(),
            })
            .collect()
    }

    #[test]
    fn clients_hear_what_changed_and_arrivals_the_whole_room() {
        let room = Arc::new(Mutex::new(Room::default()));
        let herdr = Feed::new(room.clone(), "herdr");
        let (first, mut first_events) = unbounded_channel();
        room.lock().unwrap().connect(1, first);
        herdr.replace(vec![node("a"), node("b")]);
        herdr.replace(vec![
            node("a"),
            Node {
                state: Some(NodeState::Working),
                ..node("c")
            },
        ]);
        let (second, mut second_events) = unbounded_channel();
        room.lock().unwrap().connect(2, second);
        assert_eq!(
            heard(&mut first_events),
            [
                "snapshot 0",
                "upserted a",
                "upserted b",
                "removed b",
                "upserted c"
            ]
        );
        assert_eq!(heard(&mut second_events), ["snapshot 2"]);
    }

    #[test]
    fn collectors_describing_one_node_add_up() {
        let room = Arc::new(Mutex::new(Room::default()));
        Feed::new(room.clone(), "herdr").replace(vec![Node {
            name: Some("pegasus".into()),
            state: Some(NodeState::Blocked),
            ..node("a")
        }]);
        Feed::new(room.clone(), "board").replace(vec![Node {
            doing: Some("waiting on review".into()),
            state: Some(NodeState::Idle),
            ..node("a")
        }]);
        let shown = &room.lock().unwrap().shown["a"];
        assert_eq!(shown.name.as_deref(), Some("pegasus"));
        assert_eq!(shown.doing.as_deref(), Some("waiting on review"));
        assert_eq!(
            shown.state,
            Some(NodeState::Blocked),
            "the first collector's wins"
        );
    }
}
