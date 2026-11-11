//! The control room's graph. Each collector replaces its own nodes whenever what it watches
//! changes; the room merges them by id, the fields of the collector that joined first
//! first, and tells every client only what changed. A collector of notes only adds to the
//! nodes others give: a note outlives the agent it is about. A parent named `name:<agent>`
//! is the agent of that name.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use claudo_protocol::{Node, NodeId, NodeState, RoomEvent, ServerEvent};
use tokio::sync::mpsc::UnboundedSender;

use crate::clients::{ClientId, Clients};

#[derive(Default)]
pub struct Room {
    /// Each collector's nodes, in the order collectors joined.
    sources: Vec<Source>,
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

    fn replace(&mut self, name: &'static str, adds: bool, nodes: Vec<Node>) {
        match self.sources.iter_mut().find(|source| source.name == name) {
            Some(source) => source.nodes = nodes,
            None => self.sources.push(Source { name, adds, nodes }),
        }
        let next = merged(&self.sources);
        for id in self.shown.keys().filter(|id| !next.contains_key(*id)) {
            self.clients
                .send(ServerEvent::Room(RoomEvent::Removed(id.clone())));
        }
        for (id, node) in &next {
            if self.shown.get(id) != Some(node) {
                self.clients
                    .send(ServerEvent::Room(RoomEvent::Upserted(Box::new(
                        node.clone(),
                    ))));
            }
        }
        self.shown = next;
    }
}

struct Source {
    name: &'static str,
    /// Whether its nodes join the room, rather than only add to the ones others give.
    adds: bool,
    nodes: Vec<Node>,
}

fn merged(sources: &[Source]) -> BTreeMap<NodeId, Node> {
    let mut nodes: BTreeMap<NodeId, Node> = BTreeMap::new();
    for node in sources
        .iter()
        .filter(|source| source.adds)
        .flat_map(|source| &source.nodes)
    {
        match nodes.get_mut(&node.id) {
            Some(known) => known.fill(node.clone()),
            None => {
                nodes.insert(node.id.clone(), node.clone());
            }
        }
    }
    for node in sources
        .iter()
        .filter(|source| !source.adds)
        .flat_map(|source| &source.nodes)
    {
        if let Some(known) = nodes.get_mut(&node.id) {
            known.fill(node.clone());
        }
    }
    // A collector that knows agents by name alone gives a parent as `name:<agent>`; one no
    // agent here has the name of is someone else's.
    let names: HashMap<String, NodeId> = nodes
        .values()
        .filter_map(|node| Some((node.name.clone()?, node.id.clone())))
        .collect();
    nodes.retain(|_, node| {
        let Some(name) = node
            .parent
            .as_deref()
            .and_then(|parent| parent.strip_prefix("name:"))
        else {
            return true;
        };
        node.parent = names.get(name).cloned();
        node.parent.is_some()
    });
    // herdr calls an agent that ended its turn done, though its note may ask for an answer.
    for node in nodes.values_mut() {
        let asks = node.note.as_ref().is_some_and(|note| note.waiting);
        if asks && node.state != Some(NodeState::Working) {
            node.state = Some(NodeState::Blocked);
        }
    }
    nodes
}

/// What one collector gives the room.
#[derive(Clone)]
pub struct Feed {
    room: Arc<Mutex<Room>>,
    source: &'static str,
    adds: bool,
}

impl Feed {
    pub fn new(room: Arc<Mutex<Room>>, source: &'static str) -> Self {
        Self {
            room,
            source,
            adds: true,
        }
    }

    /// A collector whose nodes only add to the ones other collectors give.
    pub fn notes(room: Arc<Mutex<Room>>, source: &'static str) -> Self {
        Self {
            room,
            source,
            adds: false,
        }
    }

    /// Everything this collector sees now, in place of what it saw before.
    pub fn replace(&self, nodes: Vec<Node>) {
        self.room
            .lock()
            .unwrap()
            .replace(self.source, self.adds, nodes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claudo_protocol::Note;
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
            name: Some("builder".into()),
            state: Some(NodeState::Blocked),
            ..node("a")
        }]);
        Feed::new(room.clone(), "board").replace(vec![Node {
            doing: Some("waiting on review".into()),
            state: Some(NodeState::Idle),
            ..node("a")
        }]);
        let shown = &room.lock().unwrap().shown["a"];
        assert_eq!(shown.name.as_deref(), Some("builder"));
        assert_eq!(shown.doing.as_deref(), Some("waiting on review"));
        assert_eq!(
            shown.state,
            Some(NodeState::Blocked),
            "the first collector's wins"
        );
    }

    fn noted(id: &str, waiting: bool) -> Node {
        Node {
            note: Some(Note {
                waiting,
                text: "which signature?".into(),
                pr: None,
            }),
            ..node(id)
        }
    }

    #[test]
    fn notes_join_only_the_nodes_others_give() {
        let room = Arc::new(Mutex::new(Room::default()));
        let board = Feed::notes(room.clone(), "board");
        board.replace(vec![noted("a", false), noted("gone", false)]);
        Feed::new(room.clone(), "herdr").replace(vec![node("a")]);
        let shown = &room.lock().unwrap().shown;
        assert_eq!(shown.keys().collect::<Vec<_>>(), ["a"]);
        assert!(
            shown["a"].note.is_some(),
            "a note from before its agent joined"
        );
    }

    #[test]
    fn an_agent_whose_note_asks_for_an_answer_waits_until_it_works_again() {
        let room = Arc::new(Mutex::new(Room::default()));
        Feed::new(room.clone(), "herdr").replace(vec![
            Node {
                state: Some(NodeState::Done),
                ..node("asks")
            },
            Node {
                state: Some(NodeState::Working),
                ..node("answered")
            },
            Node {
                state: Some(NodeState::Done),
                ..node("finished")
            },
        ]);
        Feed::notes(room.clone(), "board").replace(vec![
            noted("asks", true),
            noted("answered", true),
            noted("finished", false),
        ]);
        let shown = &room.lock().unwrap().shown;
        assert_eq!(shown["asks"].state, Some(NodeState::Blocked));
        assert_eq!(shown["answered"].state, Some(NodeState::Working));
        assert_eq!(shown["finished"].state, Some(NodeState::Done));
    }

    #[test]
    fn a_job_known_by_its_agents_name_hangs_under_that_agent() {
        let room = Arc::new(Mutex::new(Room::default()));
        Feed::new(room.clone(), "herdr").replace(vec![Node {
            name: Some("builder".into()),
            ..node("session:s1")
        }]);
        Feed::new(room.clone(), "dibs").replace(vec![
            Node {
                parent: Some("name:builder".into()),
                ..node("dibs:host-a:1")
            },
            Node {
                parent: Some("name:someone-else".into()),
                ..node("dibs:host-a:2")
            },
        ]);
        let shown = &room.lock().unwrap().shown;
        assert_eq!(shown["dibs:host-a:1"].parent.as_deref(), Some("session:s1"));
        assert!(!shown.contains_key("dibs:host-a:2"));
    }
}
