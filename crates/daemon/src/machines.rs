//! The rooms of the daemons on other computers, each reached over ssh through its
//! `--bridge` and reached again whenever it is lost, shown beside this computer's. An action
//! on one of their agents goes to the daemon that has it, and its failure back to the client
//! that asked.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::process::Command as Process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use claudo_core::hub::Hub;
use claudo_protocol::{Command, Node, NodeId, RoomEvent, ServerEvent};
use claudo_transport::Commands;
use tokio::sync::mpsc::UnboundedSender;

/// Runs the other computer's bridge wherever `cargo xtask install-daemon` put it; sh reads
/// it the same under any login shell.
const BRIDGE: &str = r#"sh -c 'exec "${CARGO_HOME:-$HOME/.cargo}/bin/claudo-daemon" --bridge'"#;

#[derive(Default)]
pub struct Machines(Vec<Machine>);

struct Machine {
    commands: Commands,
    /// The nodes its room shows now.
    shows: Arc<Mutex<HashSet<NodeId>>>,
    /// Who asked for each action relayed, by its `request`, to hear if it fails.
    asked: Arc<Mutex<HashMap<u64, UnboundedSender<ServerEvent>>>>,
    next: AtomicU64,
}

impl Machines {
    /// The daemons on `hosts`, as ssh names them.
    pub fn spawn(hosts: impl IntoIterator<Item = String>, hub: &Hub) -> Self {
        Machines(
            hosts
                .into_iter()
                .map(|host| Machine::spawn(host, hub))
                .collect(),
        )
    }

    /// Sends an action on another computer's node to it, saying whether one has the node.
    pub fn act(&self, command: &Command, from: &UnboundedSender<ServerEvent>) -> bool {
        let Command::Act { node, action, .. } = command else {
            return false;
        };
        let Some(machine) = self
            .0
            .iter()
            .find(|machine| machine.shows.lock().unwrap().contains(node))
        else {
            return false;
        };
        // Only a failure answers, so an action that worked leaves its asker here until the
        // tab closes.
        let request = machine.next.fetch_add(1, Ordering::Relaxed) + 1;
        let mut asked = machine.asked.lock().unwrap();
        asked.retain(|_, asker| !asker.is_closed());
        asked.insert(request, from.clone());
        drop(asked);
        machine.commands.send(Command::Act {
            node: node.clone(),
            action: action.clone(),
            request,
        });
        true
    }
}

impl Machine {
    fn spawn(host: String, hub: &Hub) -> Self {
        let feed = hub.machine(Box::leak(format!("machine:{host}").into_boxed_str()));
        let (commands, mut events) = claudo_transport::connect_with(move || {
            let mut ssh = Process::new("ssh");
            ssh.args([
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "ServerAliveInterval=10",
                "-o",
                "ServerAliveCountMax=3",
                &host,
                BRIDGE,
            ]);
            ssh
        });
        let shows: Arc<Mutex<HashSet<NodeId>>> = Arc::default();
        let asked: Arc<Mutex<HashMap<u64, UnboundedSender<ServerEvent>>>> = Arc::default();
        let (shown, answer) = (shows.clone(), asked.clone());
        std::thread::spawn(move || {
            let mut nodes: BTreeMap<NodeId, Node> = BTreeMap::new();
            while let Some(event) = events.blocking_recv() {
                match event {
                    ServerEvent::Room(RoomEvent::Snapshot(all)) => {
                        nodes = all
                            .into_iter()
                            .map(|node| (node.id.clone(), node))
                            .collect();
                    }
                    ServerEvent::Room(RoomEvent::Upserted(node)) => {
                        nodes.insert(node.id.clone(), *node);
                    }
                    ServerEvent::Room(RoomEvent::Removed(id)) => {
                        nodes.remove(&id);
                    }
                    ServerEvent::Room(RoomEvent::Failed {
                        node,
                        detail,
                        request,
                    }) => {
                        if let Some(asker) = answer.lock().unwrap().remove(&request) {
                            let failed = RoomEvent::Failed {
                                node,
                                detail,
                                request: 0,
                            };
                            let _ = asker.send(ServerEvent::Room(failed));
                        }
                        continue;
                    }
                    ServerEvent::Connection {
                        connected: false, ..
                    } => nodes.clear(),
                    _ => continue,
                }
                *shown.lock().unwrap() = nodes.keys().cloned().collect();
                feed.replace(nodes.values().cloned().collect());
            }
        });
        Machine {
            commands,
            shows,
            asked,
            next: AtomicU64::new(0),
        }
    }
}
