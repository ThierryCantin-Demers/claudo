//! The control room: the agents waiting on a person as tiles, then a map of every agent the
//! daemon's collectors see, one island per repository with what each agent started around it.

use std::collections::BTreeMap;
use std::f32::consts::{PI, TAU};
use std::time::Duration;

use claudo_protocol::{Node, NodeId, NodeKind, NodeState, Provider, RoomEvent};
use egui::epaint::QuadraticBezierShape;
use egui::{Align2, Color32, FontId, Pos2, RichText, Sense, Stroke, Vec2, pos2, vec2};

#[derive(Default)]
pub struct Room {
    nodes: BTreeMap<NodeId, Node>,
    /// The state each node was last drawn in, and since when in `Context::time`. A state a
    /// node already had when the page first saw it has no start.
    since: BTreeMap<NodeId, (Option<NodeState>, Option<f64>)>,
    primed: bool,
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

    fn stamp(&mut self, now: f64) {
        self.since.retain(|id, _| self.nodes.contains_key(id));
        for (id, node) in &self.nodes {
            match self.since.get_mut(id) {
                Some((state, at)) if *state != node.state => {
                    *state = node.state;
                    *at = Some(now);
                }
                Some(_) => {}
                None => {
                    let at = self.primed.then_some(now);
                    self.since.insert(id.clone(), (node.state, at));
                }
            }
        }
        self.primed |= !self.nodes.is_empty();
    }

    /// How long a node has been in its state, when the page saw it change.
    fn held(&self, id: &str, now: f64) -> Option<f64> {
        self.since.get(id)?.1.map(|at| now - at)
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

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        let now = ui.ctx().time();
        self.stamp(now);
        let moving = self
            .nodes
            .values()
            .any(|node| matches!(node.state, Some(NodeState::Working | NodeState::Blocked)));
        if moving {
            ui.ctx().request_repaint_after(Duration::from_millis(33));
        }
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show(ui, |ui| {
                ui.add_space(12.0);
                self.summary(ui);
                ui.add_space(12.0);
                self.tiles(ui, now);
                ui.add_space(20.0);
                legend(ui);
                ui.add_space(4.0);
                self.map(ui, now);
            });
    }

    fn summary(&self, ui: &mut egui::Ui) {
        let count = |state| {
            self.nodes
                .values()
                .filter(|node| node.state == Some(state))
                .count()
        };
        let agents = self
            .nodes
            .values()
            .filter(|node| matches!(node.kind, Some(NodeKind::Agent(_))))
            .count();
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("{agents} agents"))
                    .size(20.0)
                    .strong(),
            );
            for (state, word) in [
                (NodeState::Working, "working"),
                (NodeState::Blocked, "need you"),
                (NodeState::Failed, "failed"),
            ] {
                let n = count(state);
                if n > 0 {
                    ui.add_space(10.0);
                    dot(ui, color(ui.visuals(), Some(state)));
                    ui.label(RichText::new(format!("{n} {word}")).size(15.0));
                }
            }
        });
    }

    fn tiles(&self, ui: &mut egui::Ui, now: f64) {
        let mut waiting = self.waiting().peekable();
        if waiting.peek().is_none() {
            let calm = color(ui.visuals(), Some(NodeState::Working));
            egui::Frame::new()
                .fill(calm.gamma_multiply(0.08))
                .stroke(Stroke::new(1.0, calm.gamma_multiply(0.4)))
                .corner_radius(12.0)
                .inner_margin(14.0)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        dot(ui, calm);
                        ui.label(RichText::new("Nobody is waiting on you").size(15.0));
                    });
                });
            return;
        }
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(12.0, 12.0);
            for node in waiting {
                tile(ui, node, self.held(&node.id, now), now);
            }
        });
    }

    fn map(&self, ui: &mut egui::Ui, now: f64) {
        let width = ui.available_width();
        let clusters = self.clusters();
        let islands: Vec<Island> = clusters.iter().map(Island::of).collect();
        let (centers, height) = pack(&islands, width);
        let (response, painter) =
            ui.allocate_painter(vec2(width, height.max(200.0)), Sense::hover());
        let origin = response.rect.min;
        let ctx = ui.ctx().clone();
        // Positions glide to where a change in the room moves them; a new width lays the map
        // out again at once.
        let glide = |key: &str, at: Pos2| {
            let id = egui::Id::new(("room-at", key, width as u32));
            origin
                + vec2(
                    ctx.animate_value_with_time(id.with(0), at.x, 0.5),
                    ctx.animate_value_with_time(id.with(1), at.y, 0.5),
                )
        };
        let visuals = ui.visuals().clone();

        let mut drawn: Vec<(&Node, Pos2, f32)> = Vec::new();
        for (island, center) in islands.iter().zip(&centers) {
            let key = island.repo.unwrap_or("");
            let middle = glide(&format!("island:{key}"), *center);
            shore(&painter, &visuals, island, middle);
            let at: BTreeMap<&str, Pos2> = island
                .placed
                .iter()
                .map(|p| (p.node.id.as_str(), glide(&p.node.id, *center + p.at)))
                .collect();
            for placed in &island.placed {
                let Some(parent) = placed.parent.and_then(|parent| at.get(parent)) else {
                    continue;
                };
                edge(
                    &painter,
                    &visuals,
                    placed.node,
                    *parent,
                    at[placed.node.id.as_str()],
                    now,
                );
            }
            for placed in island.placed.iter().rev() {
                let here = at[placed.node.id.as_str()];
                let radius = if placed.depth == 0 { AGENT } else { CHILD };
                body(&painter, &visuals, placed.node, here, radius, now);
                if placed.depth == 0 {
                    caption(&painter, &visuals, placed.node, here);
                }
                drawn.push((placed.node, here, radius));
            }
        }

        let pointer = response.hover_pos();
        let hovered = pointer.and_then(|pointer| {
            drawn
                .iter()
                .filter(|(_, at, radius)| at.distance(pointer) <= radius + 6.0)
                .min_by(|a, b| a.1.distance(pointer).total_cmp(&b.1.distance(pointer)))
        });
        if let Some(&(node, at, radius)) = hovered {
            painter.circle_stroke(
                at,
                radius + 4.0,
                Stroke::new(1.5, visuals.strong_text_color()),
            );
            let held = self.held(&node.id, now);
            response.on_hover_ui_at_pointer(|ui| about(ui, node, held));
        }
    }
}

const AGENT: f32 = 17.0;
const CHILD: f32 = 7.0;
const CHILD_DISTANCE: f32 = 54.0;
/// How far an agent's caption reaches to either side.
const CAPTION_REACH: f32 = 74.0;
/// The space between neighbours on an island's ring, so their captions never meet.
const ROOT_GAP: f32 = 2.0 * CAPTION_REACH + 16.0;
/// Room at the top of an island for its name.
const SHORE: f32 = 34.0;
const ISLAND_GAP: f32 = 24.0;

#[derive(Debug)]
struct Placed<'a> {
    node: &'a Node,
    /// From the island's middle.
    at: Vec2,
    parent: Option<&'a str>,
    depth: usize,
}

#[derive(Debug)]
struct Island<'a> {
    repo: Option<&'a str>,
    radius: f32,
    placed: Vec<Placed<'a>>,
}

impl<'a> Island<'a> {
    /// The agents on a ring, each fanning what it started outward, away from the middle.
    fn of(cluster: &Cluster<'a>) -> Self {
        let n = cluster.trees.len();
        let ring = if n <= 1 {
            0.0
        } else {
            ROOT_GAP / (2.0 * (PI / n as f32).sin())
        };
        let mut placed = Vec::new();
        for (i, tree) in cluster.trees.iter().enumerate() {
            let angle = -PI / 2.0 + i as f32 * TAU / n as f32;
            let outward = (n > 1).then_some(angle);
            place(
                tree,
                Vec2::angled(angle) * ring,
                outward,
                None,
                0,
                &mut placed,
            );
        }
        let reach = placed
            .iter()
            .map(|p| {
                p.at.length()
                    + if p.depth == 0 {
                        CAPTION_REACH
                    } else {
                        CHILD * 2.0
                    }
            })
            .fold(CAPTION_REACH, f32::max);
        Island {
            repo: cluster.repo,
            radius: reach + SHORE,
            placed,
        }
    }
}

fn place<'a>(
    tree: &Tree<'a>,
    at: Vec2,
    outward: Option<f32>,
    parent: Option<&'a str>,
    depth: usize,
    placed: &mut Vec<Placed<'a>>,
) {
    placed.push(Placed {
        node: tree.node,
        at,
        parent,
        depth,
    });
    let k = tree.children.len();
    if k == 0 {
        return;
    }
    // A lone agent sits in the middle with its caption under it, so its fan opens upward.
    let middle = outward.unwrap_or(-PI / 2.0);
    let widest = if outward.is_none() {
        1.6 * PI
    } else {
        0.9 * PI
    };
    let span = ((k - 1) as f32 * 0.5).min(widest);
    for (j, child) in tree.children.iter().enumerate() {
        let angle = if k == 1 {
            middle
        } else {
            middle - span / 2.0 + span * j as f32 / (k - 1) as f32
        };
        // Below an agent is its caption, so what points down goes past it.
        let distance = if depth == 0 {
            CHILD_DISTANCE + 34.0 * angle.sin().max(0.0)
        } else {
            CHILD_DISTANCE * 0.7
        };
        let here = at + Vec2::angled(angle) * distance;
        place(
            child,
            here,
            Some(angle),
            Some(&tree.node.id),
            depth + 1,
            placed,
        );
    }
}

/// Islands in rows across `width`, each row centred: their middles, and the height it takes.
fn pack(islands: &[Island], width: f32) -> (Vec<Pos2>, f32) {
    let mut rows: Vec<Vec<usize>> = vec![Vec::new()];
    let mut used = 0.0;
    for (i, island) in islands.iter().enumerate() {
        let size = island.radius * 2.0 + ISLAND_GAP;
        let row = rows.last_mut().expect("rows starts with one");
        if !row.is_empty() && used + size > width {
            rows.push(vec![i]);
            used = size;
        } else {
            row.push(i);
            used += size;
        }
    }
    let mut centers = vec![Pos2::ZERO; islands.len()];
    let mut top = ISLAND_GAP / 2.0;
    for row in rows.iter().filter(|row| !row.is_empty()) {
        let size = |i: usize| islands[i].radius * 2.0 + ISLAND_GAP;
        let tall = row.iter().map(|&i| size(i)).fold(0.0, f32::max);
        let mut left = ((width - row.iter().map(|&i| size(i)).sum::<f32>()) / 2.0).max(0.0);
        for &i in row {
            centers[i] = pos2(left + size(i) / 2.0, top + tall / 2.0);
            left += size(i);
        }
        top += tall;
    }
    (centers, top + ISLAND_GAP / 2.0)
}

fn shore(painter: &egui::Painter, visuals: &egui::Visuals, island: &Island, middle: Pos2) {
    let sea = hue(island.repo.unwrap_or(""), visuals.dark_mode);
    for (grow, alpha) in [(16.0, 0.05), (8.0, 0.08)] {
        painter.circle_filled(middle, island.radius + grow, sea.gamma_multiply(alpha));
    }
    painter.circle_filled(middle, island.radius, visuals.extreme_bg_color);
    painter.circle_filled(middle, island.radius, sea.gamma_multiply(0.12));
    painter.circle_stroke(
        middle,
        island.radius,
        Stroke::new(1.5, sea.gamma_multiply(0.6)),
    );
    let name = island.repo.map_or("elsewhere", short);
    painter.text(
        middle - vec2(0.0, island.radius - 12.0),
        Align2::CENTER_TOP,
        name,
        FontId::proportional(14.0),
        visuals.strong_text_color(),
    );
}

fn edge(
    painter: &egui::Painter,
    visuals: &egui::Visuals,
    node: &Node,
    from: Pos2,
    to: Pos2,
    now: f64,
) {
    let tint = color(visuals, node.state);
    let bend = (to - from).rot90().normalized() * 10.0;
    let curve = QuadraticBezierShape::from_points_stroke(
        [from, from + (to - from) / 2.0 + bend, to],
        false,
        Color32::TRANSPARENT,
        Stroke::new(1.5, tint.gamma_multiply(0.45)),
    );
    if node.state == Some(NodeState::Working) {
        let t = ((now * 0.6 + phase(&node.id)) % 1.0) as f32;
        painter.circle_filled(curve.sample(t), 2.5, tint);
    }
    painter.add(curve);
}

fn body(
    painter: &egui::Painter,
    visuals: &egui::Visuals,
    node: &Node,
    at: Pos2,
    radius: f32,
    now: f64,
) {
    let tint = color(visuals, node.state);
    match node.state {
        Some(NodeState::Working) => {
            let p = ((now * 0.7 + phase(&node.id)) % 1.0) as f32;
            let ring = Stroke::new(2.0 * (1.0 - p), tint.gamma_multiply(1.0 - p));
            painter.circle_stroke(at, radius + p * radius, ring);
        }
        Some(NodeState::Blocked) => {
            let breath = (0.5 + 0.5 * (now * 3.0).sin()) as f32;
            painter.circle_filled(
                at,
                radius + 5.0 + 4.0 * breath,
                tint.gamma_multiply(0.15 + 0.2 * breath),
            );
        }
        _ => {}
    }
    let rim = Stroke::new(2.0, visuals.extreme_bg_color);
    let fill = tint.gamma_multiply(0.9);
    if node.kind == Some(NodeKind::Agent(Provider::Codex)) {
        let square = egui::Rect::from_center_size(at, Vec2::splat(radius * 1.8));
        painter.rect_filled(square, radius * 0.4, fill);
        painter.rect_stroke(square, radius * 0.4, rim, egui::StrokeKind::Outside);
    } else {
        painter.circle_filled(at, radius, fill);
        painter.circle_stroke(at, radius, rim);
    }
    let mark = match node.state {
        Some(NodeState::Blocked) => "!",
        Some(NodeState::Failed) => "x",
        _ => return,
    };
    if radius >= AGENT {
        painter.text(
            at,
            Align2::CENTER_CENTER,
            mark,
            FontId::proportional(16.0),
            visuals.extreme_bg_color,
        );
    }
}

fn caption(painter: &egui::Painter, visuals: &egui::Visuals, node: &Node, at: Pos2) {
    let top = at + vec2(0.0, AGENT + 6.0);
    let name = node.name.as_deref().unwrap_or("unnamed");
    painter.text(
        top,
        Align2::CENTER_TOP,
        clip(name, 22),
        FontId::proportional(13.0),
        visuals.strong_text_color(),
    );
    if let Some(doing) = &node.doing {
        painter.text(
            top + vec2(0.0, 17.0),
            Align2::CENTER_TOP,
            clip(doing, 26),
            FontId::proportional(11.0),
            visuals.weak_text_color(),
        );
    }
}

fn tile(ui: &mut egui::Ui, node: &Node, held: Option<f64>, now: f64) {
    let warn = ui.visuals().warn_fg_color;
    let breath = (0.5 + 0.5 * (now * 3.0).sin()) as f32;
    egui::Frame::new()
        .fill(warn.gamma_multiply(0.08 + 0.04 * breath))
        .stroke(Stroke::new(1.5, warn.gamma_multiply(0.6 + 0.4 * breath)))
        .corner_radius(12.0)
        .inner_margin(14.0)
        .show(ui, |ui| {
            ui.set_width(250.0);
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    dot(ui, warn);
                    ui.label(
                        RichText::new(node.name.as_deref().unwrap_or("unnamed"))
                            .size(17.0)
                            .strong(),
                    );
                });
                let mut place = kind(node.kind.as_ref());
                if let Some(repo) = &node.repo {
                    place.push_str(&format!(" in {}", short(repo)));
                }
                ui.label(RichText::new(place).weak());
                if let Some(doing) = &node.doing {
                    ui.add(egui::Label::new(doing.as_str()).wrap());
                }
                if let Some(held) = held {
                    ui.label(RichText::new(format!("waiting {}", ago(held))).color(warn));
                }
            });
        });
}

fn about(ui: &mut egui::Ui, node: &Node, held: Option<f64>) {
    ui.label(RichText::new(node.name.as_deref().unwrap_or("unnamed")).strong());
    let mut line = format!(
        "{} {}",
        kind(node.kind.as_ref()),
        state(node.state).to_lowercase()
    );
    if let Some(held) = held {
        line.push_str(&format!(" for {}", ago(held)));
    }
    ui.label(line);
    if let Some(doing) = &node.doing {
        ui.label(RichText::new(doing).weak());
    }
    if let Some(repo) = &node.repo {
        ui.label(RichText::new(repo).weak().monospace());
    }
    if let Some(machine) = &node.machine {
        ui.label(RichText::new(format!("on {machine}")).weak());
    }
}

fn legend(ui: &mut egui::Ui) {
    ui.horizontal_wrapped(|ui| {
        for shown in [
            NodeState::Working,
            NodeState::Blocked,
            NodeState::Idle,
            NodeState::Done,
            NodeState::Failed,
        ] {
            dot(ui, color(ui.visuals(), Some(shown)));
            ui.weak(state(Some(shown)));
            ui.add_space(6.0);
        }
        ui.add_space(10.0);
        ui.weak("round: Claude, square: Codex, small: what an agent started");
    });
}

fn dot(ui: &mut egui::Ui, tint: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(12.0, 12.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 5.0, tint);
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

fn state(state: Option<NodeState>) -> &'static str {
    match state {
        Some(NodeState::Working) => "Working",
        Some(NodeState::Blocked) => "Blocked",
        Some(NodeState::Idle) => "Idle",
        Some(NodeState::Done) => "Done",
        Some(NodeState::Failed) => "Failed",
        Some(NodeState::Unknown) | None => "Unknown",
    }
}

fn color(visuals: &egui::Visuals, state: Option<NodeState>) -> Color32 {
    match state {
        Some(NodeState::Working) => Color32::from_rgb(80, 190, 120),
        Some(NodeState::Blocked) => visuals.warn_fg_color,
        Some(NodeState::Failed) => visuals.error_fg_color,
        Some(NodeState::Done) => Color32::from_rgb(90, 140, 230),
        Some(NodeState::Idle) | Some(NodeState::Unknown) | None => visuals.weak_text_color(),
    }
}

/// A repository's own colour, the same on every page.
fn hue(repo: &str, dark: bool) -> Color32 {
    let h = repo
        .bytes()
        .fold(7u32, |h, b| h.wrapping_mul(31).wrapping_add(u32::from(b)));
    let (s, v) = if dark { (0.45, 0.75) } else { (0.5, 0.8) };
    egui::ecolor::Hsva::new((h % 360) as f32 / 360.0, s, v, 1.0).into()
}

/// Where in its pulse a node starts, so neighbours do not beat together.
fn phase(id: &str) -> f64 {
    id.bytes().map(u64::from).sum::<u64>() as f64 % 97.0 / 97.0
}

fn ago(seconds: f64) -> String {
    let seconds = seconds.max(0.0) as u64;
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        _ => format!("{}h {}m", seconds / 3600, seconds % 3600 / 60),
    }
}

fn clip(text: &str, most: usize) -> String {
    if text.chars().count() <= most {
        return text.to_string();
    }
    let mut kept: String = text.chars().take(most - 1).collect();
    kept.push('…');
    kept
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

    #[test]
    fn a_state_is_timed_from_the_change_the_page_saw() {
        let mut room = Room::default();
        room.apply(RoomEvent::Snapshot(vec![node("a", None, None)]));
        room.stamp(10.0);
        assert_eq!(
            room.held("a", 50.0),
            None,
            "already so when the page opened"
        );
        room.apply(RoomEvent::Upserted(Node {
            state: Some(NodeState::Blocked),
            ..node("a", None, None)
        }));
        room.apply(RoomEvent::Upserted(node("b", None, None)));
        room.stamp(20.0);
        room.stamp(30.0);
        assert_eq!(room.held("a", 50.0), Some(30.0));
        assert_eq!(room.held("b", 50.0), Some(30.0), "arrived while watching");
    }

    #[test]
    fn islands_hold_their_nodes_and_rows_fit_the_width() {
        let mut room = Room::default();
        let mut nodes = vec![node("lone", Some("/src/beta"), None)];
        for i in 0..5 {
            nodes.push(node(&format!("agent-{i}"), Some("/src/alpha"), None));
            for j in 0..4 {
                let parent = format!("agent-{i}");
                nodes.push(node(
                    &format!("sub-{i}-{j}"),
                    Some("/src/alpha"),
                    Some(&parent),
                ));
            }
        }
        room.apply(RoomEvent::Snapshot(nodes));
        let islands: Vec<Island> = room.clusters().iter().map(Island::of).collect();
        for island in &islands {
            for placed in &island.placed {
                assert!(placed.at.length() + CHILD <= island.radius, "{placed:?}");
            }
        }
        assert_eq!(
            islands[1].placed[0].at,
            Vec2::ZERO,
            "a lone agent sits in the middle"
        );
        let width = islands[0].radius * 2.0 + ISLAND_GAP + 10.0;
        let (centers, height) = pack(&islands, width);
        assert!(
            centers[1].y > centers[0].y,
            "the second island wraps to a row of its own"
        );
        assert!(
            height
                >= islands
                    .iter()
                    .map(|island| island.radius * 2.0)
                    .sum::<f32>()
        );
    }
}
