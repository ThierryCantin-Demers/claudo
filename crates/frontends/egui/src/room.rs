//! The control room: every agent the daemon's collectors see, the ones waiting on a person
//! first, then one card per repository with what each agent started under it.

use std::collections::BTreeMap;

use claudo_protocol::{Node, NodeId, NodeKind, NodeState, Provider, RoomEvent};

#[derive(Default)]
pub struct Room {
    nodes: BTreeMap<NodeId, Node>,
}

/// A repository's agents, each with what it started.
#[derive(Debug, PartialEq)]
struct Cluster<'a> {
    repo: Option<&'a str>,
    trees: Vec<Tree<'a>>,
}

#[derive(Debug, PartialEq)]
struct Tree<'a> {
    node: &'a Node,
    children: Vec<Tree<'a>>,
}

impl Room {
    pub fn apply(&mut self, event: RoomEvent) {
        match event {
            RoomEvent::Snapshot(nodes) => {
                self.nodes = nodes
                    .into_iter()
                    .map(|node| (node.id.clone(), node))
                    .collect();
            }
            RoomEvent::Upserted(node) => {
                self.nodes.insert(node.id.clone(), node);
            }
            RoomEvent::Removed(id) => {
                self.nodes.remove(&id);
            }
        }
    }

    fn waiting(&self) -> impl Iterator<Item = &Node> {
        self.nodes
            .values()
            .filter(|node| node.state == Some(NodeState::Blocked))
    }

    /// Repositories by name, then the agents in none. A node whose parent is gone is a root.
    fn clusters(&self) -> Vec<Cluster<'_>> {
        let mut children: BTreeMap<&str, Vec<&Node>> = BTreeMap::new();
        let mut roots: BTreeMap<Option<&str>, Vec<&Node>> = BTreeMap::new();
        for node in self.nodes.values() {
            match node.parent.as_deref() {
                Some(parent) if self.nodes.contains_key(parent) => {
                    children.entry(parent).or_default().push(node)
                }
                _ => roots.entry(node.repo.as_deref()).or_default().push(node),
            }
        }
        fn tree<'a>(node: &'a Node, children: &BTreeMap<&str, Vec<&'a Node>>) -> Tree<'a> {
            let below = children.get(node.id.as_str()).into_iter().flatten();
            Tree {
                node,
                children: below.map(|child| tree(child, children)).collect(),
            }
        }
        let mut clusters: Vec<Cluster> = roots
            .into_iter()
            .map(|(repo, nodes)| Cluster {
                repo,
                trees: nodes
                    .into_iter()
                    .map(|node| tree(node, &children))
                    .collect(),
            })
            .collect();
        clusters.sort_by_key(|cluster| (cluster.repo.is_none(), cluster.repo.map(short)));
        clusters
    }

    pub fn ui(&self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show(ui, |ui| {
                ui.add_space(8.0);
                ui.heading("Waiting on you");
                let mut waiting = self.waiting().peekable();
                if waiting.peek().is_none() {
                    ui.weak("No agent is waiting on you.");
                }
                for node in waiting {
                    row(ui, node);
                }
                ui.add_space(16.0);
                let clusters = self.clusters();
                let fit = (ui.available_width() / CARD_WIDTH).floor() as usize;
                let columns = fit.clamp(1, clusters.len().max(1));
                ui.columns(columns, |columns| {
                    for (index, cluster) in clusters.iter().enumerate() {
                        card(&mut columns[index % columns.len()], cluster);
                    }
                });
            });
    }
}

/// The narrowest a card is drawn, which decides how many columns fit.
const CARD_WIDTH: f32 = 340.0;

fn card(ui: &mut egui::Ui, cluster: &Cluster) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.vertical(|ui| {
            let title = ui.strong(cluster.repo.map_or("Elsewhere", short));
            if let Some(repo) = cluster.repo {
                title.on_hover_text(repo);
            }
            for tree in &cluster.trees {
                branch(ui, tree);
            }
        });
    });
    ui.add_space(8.0);
}

fn branch(ui: &mut egui::Ui, tree: &Tree) {
    row(ui, tree.node);
    if tree.children.is_empty() {
        return;
    }
    ui.indent(&tree.node.id, |ui| {
        for child in &tree.children {
            branch(ui, child);
        }
    });
}

fn row(ui: &mut egui::Ui, node: &Node) {
    let response = ui
        .horizontal(|ui| {
            let (dot, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
            ui.painter()
                .circle_filled(dot.center(), 4.0, color(ui, node.state));
            ui.weak(kind(node.kind.as_ref()));
            ui.strong(node.name.as_deref().unwrap_or("unnamed"));
            if let Some(doing) = &node.doing {
                ui.add(egui::Label::new(egui::RichText::new(doing).weak()).truncate());
            }
        })
        .response;
    let mut about = format!("{:?}", node.state.unwrap_or(NodeState::Unknown));
    if let Some(machine) = &node.machine {
        about.push_str(&format!(" on {machine}"));
    }
    response.on_hover_text(about);
}

fn kind(kind: Option<&NodeKind>) -> String {
    match kind {
        Some(NodeKind::Agent(Provider::Claude)) => "Claude".to_string(),
        Some(NodeKind::Agent(Provider::Codex)) => "Codex".to_string(),
        Some(NodeKind::Agent(Provider::Other(name))) => name.clone(),
        Some(NodeKind::Run(kind)) => kind.clone(),
        Some(NodeKind::Subagent) => "subagent".to_string(),
        None => String::new(),
    }
}

fn color(ui: &egui::Ui, state: Option<NodeState>) -> egui::Color32 {
    let visuals = ui.visuals();
    match state {
        Some(NodeState::Working) => egui::Color32::from_rgb(80, 180, 110),
        Some(NodeState::Blocked) => visuals.warn_fg_color,
        Some(NodeState::Failed) => visuals.error_fg_color,
        Some(NodeState::Done) => egui::Color32::from_rgb(90, 140, 220),
        Some(NodeState::Idle) | Some(NodeState::Unknown) | None => visuals.weak_text_color(),
    }
}

/// The last part of a path: a repository's folder name.
fn short(repo: &str) -> &str {
    repo.rsplit('/').next().unwrap_or(repo)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, repo: Option<&str>, parent: Option<&str>) -> Node {
        Node {
            id: id.to_string(),
            repo: repo.map(str::to_string),
            parent: parent.map(str::to_string),
            ..Node::default()
        }
    }

    #[test]
    fn agents_cluster_by_repo_with_what_they_started_under_them() {
        let mut room = Room::default();
        room.apply(RoomEvent::Snapshot(vec![
            node("a", Some("/src/zeta"), None),
            node("b", Some("/src/alpha"), None),
            node("review", Some("/src/alpha"), Some("b")),
            node("c", None, None),
            node("orphan", Some("/src/zeta"), Some("gone")),
        ]));
        let clusters = room.clusters();
        let names: Vec<_> = clusters.iter().map(|cluster| cluster.repo).collect();
        assert_eq!(names, [Some("/src/alpha"), Some("/src/zeta"), None]);
        let alpha = &clusters[0].trees;
        assert_eq!(alpha.len(), 1, "the review sits under its agent");
        assert_eq!(alpha[0].children[0].node.id, "review");
        assert_eq!(
            clusters[1].trees.len(),
            2,
            "a node whose parent left stays shown"
        );
    }

    #[test]
    fn changes_apply_by_id() {
        let mut room = Room::default();
        room.apply(RoomEvent::Snapshot(vec![node("a", None, None)]));
        room.apply(RoomEvent::Upserted(Node {
            state: Some(NodeState::Blocked),
            ..node("a", None, None)
        }));
        room.apply(RoomEvent::Upserted(node("b", None, None)));
        room.apply(RoomEvent::Removed("b".to_string()));
        assert_eq!(
            room.waiting().map(|node| &node.id).collect::<Vec<_>>(),
            ["a"]
        );
        assert_eq!(room.nodes.len(), 1);
    }
}
