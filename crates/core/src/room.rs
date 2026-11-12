//! The control room's graph. Each collector replaces its own nodes whenever what it watches
//! changes; the room merges them by id, the fields of the collector that joined first
//! first, and tells every client only what changed. A collector of notes only adds to the
//! nodes others give: a note outlives the agent it is about. A parent named `name:<agent>`
//! is the agent of that name. Other computers' daemons hear only what this computer's own
//! collectors see, never what came from another, so two that watch each other echo nothing.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use claudo_protocol::{
    Benchmark, DibsMachine, DibsWatch, Node, NodeId, NodeKind, NodeState, RoomEvent, ServerEvent,
};
use tokio::sync::mpsc::UnboundedSender;

use crate::clients::{ClientId, Clients};

#[derive(Default)]
pub struct Room {
    /// Each collector's nodes, in the order collectors joined.
    sources: Vec<Source>,
    shown: BTreeMap<NodeId, Node>,
    clients: Clients,
    /// What this computer's own collectors see, and the other computers' daemons hearing it.
    local: BTreeMap<NodeId, Node>,
    machines: Clients,
    /// The dibs machines, which only this computer's pages hear.
    dibs: Vec<DibsMachine>,
    /// How they are watched.
    dibs_watch: Option<DibsWatch>,
    benchmarks: Vec<Benchmark>,
}

impl Room {
    pub fn connect(&mut self, client: ClientId, events: UnboundedSender<ServerEvent>) {
        self.clients.add(client, events);
        let nodes = self.shown.values().cloned().collect();
        self.clients
            .to(client, ServerEvent::Room(RoomEvent::Snapshot(nodes)));
        if !self.dibs.is_empty() {
            self.clients
                .to(client, ServerEvent::Dibs(self.dibs.clone()));
        }
        if let Some(watch) = &self.dibs_watch {
            self.clients
                .to(client, ServerEvent::DibsWatch(watch.clone()));
        }
        if !self.benchmarks.is_empty() {
            self.clients
                .to(client, ServerEvent::Benchmarks(self.benchmarks.clone()));
        }
    }

    fn set_benchmarks(&mut self, benchmarks: Vec<Benchmark>) {
        if benchmarks != self.benchmarks {
            self.clients
                .send(ServerEvent::Benchmarks(benchmarks.clone()));
            self.benchmarks = benchmarks;
        }
    }

    fn set_dibs_watch(&mut self, watch: DibsWatch) {
        if self.dibs_watch.as_ref() != Some(&watch) {
            self.clients.send(ServerEvent::DibsWatch(watch.clone()));
            self.dibs_watch = Some(watch);
        }
    }

    fn set_dibs(&mut self, machines: Vec<DibsMachine>) {
        if machines != self.dibs {
            self.clients.send(ServerEvent::Dibs(machines.clone()));
            self.dibs = machines;
        }
    }

    /// Another computer's daemon, which hears only what this computer's collectors see.
    pub fn connect_machine(&mut self, client: ClientId, events: UnboundedSender<ServerEvent>) {
        self.machines.add(client, events);
        let nodes = self.local.values().cloned().collect();
        self.machines
            .to(client, ServerEvent::Room(RoomEvent::Snapshot(nodes)));
    }

    pub fn disconnect(&mut self, client: ClientId) {
        self.clients.remove(client);
        self.machines.remove(client);
    }

    fn replace(&mut self, name: &'static str, from: Gives, nodes: Vec<Node>) {
        match self.sources.iter_mut().find(|source| source.name == name) {
            Some(source) => source.nodes = nodes,
            None => self.sources.push(Source { name, from, nodes }),
        }
        let every: Vec<&Source> = self.sources.iter().collect();
        let next = merged(&every, false);
        tell(&self.clients, &self.shown, &next);
        self.shown = next;
        let own: Vec<&Source> = every
            .into_iter()
            .filter(|source| source.from != Gives::Machine)
            .collect();
        // A job of an agent another computer runs reaches that computer through this one.
        let local = merged(&own, true);
        tell(&self.machines, &self.local, &local);
        self.local = local;
    }
}

/// Sends `clients` what changed from `before` to `after`.
fn tell(clients: &Clients, before: &BTreeMap<NodeId, Node>, after: &BTreeMap<NodeId, Node>) {
    for id in before.keys().filter(|id| !after.contains_key(*id)) {
        clients.send(ServerEvent::Room(RoomEvent::Removed(id.clone())));
    }
    for (id, node) in after {
        if before.get(id) != Some(node) {
            clients.send(ServerEvent::Room(RoomEvent::Upserted(Box::new(
                node.clone(),
            ))));
        }
    }
}

struct Source {
    name: &'static str,
    from: Gives,
    nodes: Vec<Node>,
}

/// What a collector gives.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Gives {
    /// Nodes of its own.
    Nodes,
    /// Only what it adds to the nodes others give.
    Notes,
    /// Another computer's room.
    Machine,
}

/// With `passed_on`, a `name:` parent no agent here has stays, for whoever hears it to find.
fn merged(sources: &[&Source], passed_on: bool) -> BTreeMap<NodeId, Node> {
    let mut nodes: BTreeMap<NodeId, Node> = BTreeMap::new();
    for node in sources
        .iter()
        .filter(|source| source.from != Gives::Notes)
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
        .filter(|source| source.from == Gives::Notes)
        .flat_map(|source| &source.nodes)
    {
        if let Some(known) = nodes.get_mut(&node.id) {
            known.fill(node.clone());
        }
    }
    // A collector that knows more than one place for a node gives them as `a|b`, the most
    // precise first, such as the subagent that started a run and then its agent: it goes
    // under the first the room shows, or the last when it shows none. Passed on, the room
    // that shows it chooses.
    let shown: HashSet<NodeId> = nodes.keys().cloned().collect();
    for node in nodes.values_mut().filter(|_| !passed_on) {
        let Some(parent) = node.parent.as_deref().filter(|parent| parent.contains('|')) else {
            continue;
        };
        let places: Vec<&str> = parent.split('|').collect();
        let chosen = places
            .iter()
            .find(|place| place.starts_with("name:") || shown.contains(**place))
            .or(places.last())
            .map(|place| place.to_string());
        node.parent = chosen;
    }
    // A collector that knows agents by name alone gives a parent as `name:<agent>`; one no
    // agent here has the name of is someone else's.
    let names: HashMap<String, NodeId> = nodes
        .values()
        .filter(|node| matches!(node.kind, Some(NodeKind::Agent(_))))
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
        // Something an agent started names itself after it: `builder~codex-review`.
        let agent = name.split_once('~').map_or(name, |(agent, _)| agent);
        match names.get(name).or_else(|| names.get(agent)) {
            Some(id) => {
                node.parent = Some(id.clone());
                true
            }
            None => passed_on,
        }
    });
    // herdr calls an agent that ended its turn done, though its note may ask for an answer:
    // until the person writes to it, when the note has gone stale unless written again. Both
    // times are whole seconds, so a message in the note's own second may have come first.
    for node in nodes.values_mut() {
        let asks = node.note.as_ref().is_some_and(|note| {
            let answered = node
                .answered
                .zip(note.at)
                .is_some_and(|(answered, at)| answered > at);
            note.waiting && !answered
        });
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
    from: Gives,
}

impl Feed {
    pub fn new(room: Arc<Mutex<Room>>, source: &'static str) -> Self {
        Self {
            room,
            source,
            from: Gives::Nodes,
        }
    }

    /// A collector whose nodes only add to the ones other collectors give.
    pub fn notes(room: Arc<Mutex<Room>>, source: &'static str) -> Self {
        Self {
            room,
            source,
            from: Gives::Notes,
        }
    }

    /// Another computer's room, which this one shows but never passes on.
    pub fn machine(room: Arc<Mutex<Room>>, source: &'static str) -> Self {
        Self {
            room,
            source,
            from: Gives::Machine,
        }
    }

    /// The dibs machines as the last look found them.
    pub fn dibs(&self, machines: Vec<DibsMachine>) {
        self.room.lock().unwrap().set_dibs(machines);
    }

    /// The team's benchmarks as the last look found them.
    pub fn benchmarks(&self, benchmarks: Vec<Benchmark>) {
        self.room.lock().unwrap().set_benchmarks(benchmarks);
    }

    /// How the dibs machines are watched.
    pub fn dibs_watch(&self, watch: DibsWatch) {
        self.room.lock().unwrap().set_dibs_watch(watch);
    }

    /// Everything this collector sees now, in place of what it saw before.
    pub fn replace(&self, nodes: Vec<Node>) {
        self.room
            .lock()
            .unwrap()
            .replace(self.source, self.from, nodes);
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
                at: Some(100),
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
            kind: Some(NodeKind::Agent(claudo_protocol::Provider::Claude)),
            ..node("session:s1")
        }]);
        Feed::new(room.clone(), "runs").replace(vec![Node {
            name: Some("builder".into()),
            kind: Some(NodeKind::Run("review".into())),
            ..node("run:r1")
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
            Node {
                parent: Some("name:builder~codex-review".into()),
                ..node("dibs:host-a:3")
            },
        ]);
        let shown = &room.lock().unwrap().shown;
        assert_eq!(shown["dibs:host-a:1"].parent.as_deref(), Some("session:s1"));
        assert!(!shown.contains_key("dibs:host-a:2"));
        assert_eq!(
            shown["dibs:host-a:3"].parent.as_deref(),
            Some("session:s1"),
            "a job of a review the agent started"
        );
    }

    #[test]
    fn another_computer_hears_only_what_this_ones_collectors_see() {
        let room = Arc::new(Mutex::new(Room::default()));
        let (machine, mut machine_events) = unbounded_channel();
        room.lock().unwrap().connect_machine(9, machine);
        let (page, mut page_events) = unbounded_channel();
        room.lock().unwrap().connect(1, page);
        Feed::new(room.clone(), "herdr").replace(vec![node("here")]);
        Feed::machine(room.clone(), "remote:host-b").replace(vec![node("there")]);
        assert_eq!(
            heard(&mut page_events),
            ["snapshot 0", "upserted here", "upserted there"]
        );
        assert_eq!(heard(&mut machine_events), ["snapshot 0", "upserted here"]);
    }

    #[test]
    fn a_job_of_another_computers_agent_reaches_it() {
        let room = Arc::new(Mutex::new(Room::default()));
        let (machine, mut machine_events) = unbounded_channel();
        room.lock().unwrap().connect_machine(9, machine);
        Feed::new(room.clone(), "dibs").replace(vec![Node {
            parent: Some("name:elsewhere".into()),
            ..node("dibs:host-a:1")
        }]);
        assert_eq!(
            heard(&mut machine_events),
            ["snapshot 0", "upserted dibs:host-a:1"]
        );
        assert!(
            !room.lock().unwrap().shown.contains_key("dibs:host-a:1"),
            "this computer's page has no such agent"
        );
    }

    #[test]
    fn a_node_goes_under_the_first_place_it_names_that_the_room_shows() {
        let room = Arc::new(Mutex::new(Room::default()));
        let under = |id: &str, parent: &str| Node {
            parent: Some(parent.to_string()),
            ..node(id)
        };
        Feed::new(room.clone(), "herdr").replace(vec![node("session:s1")]);
        Feed::new(room.clone(), "subagents").replace(vec![under("subagent:a1", "session:s1")]);
        Feed::new(room.clone(), "runs").replace(vec![
            under("run:by-subagent", "subagent:a1|session:s1"),
            under("run:subagent-left", "subagent:gone|session:s1"),
        ]);
        let shown = &room.lock().unwrap().shown;
        assert_eq!(
            shown["run:by-subagent"].parent.as_deref(),
            Some("subagent:a1")
        );
        assert_eq!(
            shown["run:subagent-left"].parent.as_deref(),
            Some("session:s1")
        );
    }

    #[test]
    fn another_computer_hears_every_place_a_node_names() {
        let jobs = Source {
            name: "dibs",
            from: Gives::Nodes,
            nodes: vec![
                node("name:builder"),
                Node {
                    parent: Some("subagent:a1|name:builder".into()),
                    ..node("dibs:host-a:1")
                },
            ],
        };
        let passed = merged(&[&jobs], true);
        assert_eq!(
            passed["dibs:host-a:1"].parent.as_deref(),
            Some("subagent:a1|name:builder"),
            "the room there may show the subagent this one does not"
        );
    }

    #[test]
    fn a_waiting_note_goes_stale_once_the_person_writes_to_the_agent() {
        let room = Arc::new(Mutex::new(Room::default()));
        let done = |id: &str, answered: Option<u64>| Node {
            state: Some(NodeState::Done),
            answered,
            ..node(id)
        };
        Feed::new(room.clone(), "herdr").replace(vec![
            done("unanswered", Some(50)),
            done("answered", Some(150)),
            done("same-second", Some(100)),
            done("never", None),
        ]);
        Feed::notes(room.clone(), "board").replace(vec![
            noted("unanswered", true),
            noted("answered", true),
            noted("same-second", true),
            noted("never", true),
        ]);
        let shown = &room.lock().unwrap().shown;
        assert_eq!(shown["unanswered"].state, Some(NodeState::Blocked));
        assert_eq!(shown["answered"].state, Some(NodeState::Done));
        assert_eq!(shown["never"].state, Some(NodeState::Blocked));
        assert_eq!(shown["same-second"].state, Some(NodeState::Blocked));
    }
}
