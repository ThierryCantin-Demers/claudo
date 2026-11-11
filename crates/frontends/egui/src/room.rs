//! The control room: the agents waiting on a person as tiles, then a map of every agent the
//! daemon's collectors see, one island per repository with what each agent started around it.

use std::collections::BTreeMap;
use std::f32::consts::PI;
use std::time::Duration;

use claudo_protocol::{
    Action, Checks, Command, Node, NodeId, NodeKind, NodeState, Provider, RoomEvent,
};
use egui::epaint::QuadraticBezierShape;
use egui::{Align2, Color32, FontId, Pos2, RichText, Sense, Stroke, Vec2, pos2, vec2};
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};

#[derive(Default)]
pub struct Room {
    nodes: BTreeMap<NodeId, Node>,
    /// The state each node was last drawn in, and since when in `Context::time`. A state a
    /// node already had when the page first saw it has no start.
    since: BTreeMap<NodeId, (Option<NodeState>, Option<f64>)>,
    primed: bool,
    /// Answers being typed to the agents waiting on the person.
    drafts: BTreeMap<NodeId, String>,
    /// The last action that failed, and why.
    failed: Option<(NodeId, String)>,
    sent: Vec<Command>,
    /// The node whose details are open beside the map.
    selected: Option<NodeId>,
    /// What the last click on the map picked, and when. A double-click right after acts on
    /// it wherever it lands, since the details the first click opened move the map.
    clicked: Option<(NodeId, f64)>,
    markdown: CommonMarkCache,
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
                self.nodes.insert(node.id.clone(), *node);
            }
            RoomEvent::Removed(id) => {
                self.nodes.remove(&id);
            }
            RoomEvent::Failed { node, detail, .. } => self.failed = Some((node, detail)),
        }
    }

    /// What the person asked of the agents since the last call, for the backend.
    pub fn take_sent(&mut self) -> Vec<Command> {
        std::mem::take(&mut self.sent)
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
        let shown = |node: &&Node| {
            !finished(node)
                || node
                    .parent
                    .as_ref()
                    .is_none_or(|p| !self.nodes.contains_key(p))
        };
        for node in self.nodes.values().filter(shown) {
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
        self.selected = self
            .selected
            .take()
            .filter(|id| self.nodes.contains_key(id));
        if let Some(id) = self.selected.clone() {
            // Leaves the map at least half its tile, a narrow one too.
            let room = ui.available_width() / 2.0;
            egui::Panel::right("room-details")
                .default_size(room.min(440.0))
                .size_range(room.min(200.0)..=room)
                .resizable(true)
                .show(ui, |ui| self.details(ui, &id, now));
        }
        let double = ui.input(|input| {
            input
                .pointer
                .button_double_clicked(egui::PointerButton::Primary)
        });
        let screen = ui.available_height();
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show(ui, |ui| {
                let top = ui.cursor().top();
                // Measured before anything is drawn: what is drawn wider widens the page after.
                let width = ui.available_width();
                ui.add_space(8.0);
                ui.horizontal_wrapped(|ui| {
                    self.summary(ui);
                    ui.add_space(24.0);
                    legend(ui);
                });
                self.pull_requests(ui);
                self.failure(ui);
                ui.add_space(8.0);
                self.tiles(ui, width, now);
                let left = screen - (ui.cursor().top() - top);
                let picked = self.map(ui, width, left, now);
                if double {
                    let agent = self.clicked.take().filter(|(id, at)| {
                        let node = self.nodes.get(id);
                        now - at < DOUBLE_CLICK
                            && node
                                .is_some_and(|node| matches!(node.kind, Some(NodeKind::Agent(_))))
                    });
                    if let Some((node, _)) = agent {
                        self.sent.push(Command::Act {
                            node,
                            action: Action::Focus,
                            request: 0,
                        });
                    }
                } else {
                    match picked {
                        Some(Picked::Node(id)) => {
                            self.clicked = Some((id.clone(), now));
                            self.selected = (self.selected.as_ref() != Some(&id)).then_some(id);
                        }
                        Some(Picked::Nothing) => self.clicked = None,
                        None => {}
                    }
                }
            });
    }

    /// Every open pull request an agent here has, with the agent's name.
    fn pull_requests(&self, ui: &mut egui::Ui) {
        let mut open: Vec<(&Node, Shown)> = self
            .nodes
            .values()
            .flat_map(|node| open_prs(node).into_iter().map(move |pr| (node, pr)))
            .collect();
        if open.is_empty() {
            return;
        }
        open.sort_by(|a, b| a.1.about.cmp(&b.1.about));
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            ui.weak("Open PRs:");
            for (node, pr) in open {
                ui.add_space(8.0);
                let repo = pr.about.split(':').next().unwrap_or(pr.number).to_string();
                ui.hyperlink_to(repo, pr.link).on_hover_text(&pr.about);
                ci(ui, pr.checks);
                ui.weak(node.name.as_deref().unwrap_or("unnamed"));
            }
        });
    }

    fn failure(&mut self, ui: &mut egui::Ui) {
        let Some((node, detail)) = &self.failed else {
            return;
        };
        let name = self
            .nodes
            .get(node)
            .and_then(|node| node.name.as_deref())
            .unwrap_or(node);
        let mut dismissed = false;
        ui.horizontal_wrapped(|ui| {
            let text = format!("Could not reach {name}: {detail}");
            ui.colored_label(ui.visuals().error_fg_color, text);
            dismissed = ui.small_button("Dismiss").clicked();
        });
        if dismissed {
            self.failed = None;
        }
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
        if self.waiting().next().is_none() {
            ui.add_space(10.0);
            dot(ui, color(ui.visuals(), Some(NodeState::Working)));
            ui.label(RichText::new("nobody is waiting on you").size(15.0));
        }
    }

    /// The selected node: what the hover says, a box to answer an agent, its digest when it
    /// is a finished review, and the runs an agent started with theirs.
    fn details(&mut self, ui: &mut egui::Ui, id: &str, now: f64) {
        let Some(node) = self.nodes.get(id).cloned() else {
            return;
        };
        let mut runs: Vec<Node> = self
            .nodes
            .values()
            .filter(|run| run.parent.as_deref() == Some(id))
            .filter(|run| matches!(run.kind, Some(NodeKind::Run(_))))
            .cloned()
            .collect();
        // Run folders begin with when they started.
        runs.sort_by(|a, b| b.id.cmp(&a.id));
        let agent = self
            .nodes
            .get(node.parent.as_deref().unwrap_or_default())
            .filter(|parent| matches!(parent.kind, Some(NodeKind::Agent(_))))
            .map(|parent| (parent.id.clone(), parent.name.clone()));
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("Close").clicked() {
                    self.selected = None;
                }
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    dot(ui, color(ui.visuals(), node.state));
                    ui.label(RichText::new(kind(&node)).size(15.0).weak());
                });
            });
        });
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show(ui, |ui| {
                about(ui, &node, self.held(id, now));
                if let Some((parent, name)) = agent {
                    let name = name.unwrap_or_else(|| "its agent".to_string());
                    if ui.link(format!("Started by {name}")).clicked() {
                        self.selected = Some(parent);
                    }
                }
                for pr in open_prs(&node) {
                    ui.horizontal(|ui| {
                        ui.hyperlink_to(format!("PR #{}", pr.number), pr.link)
                            .on_hover_text(&pr.about);
                        ci(ui, pr.checks);
                    });
                }
                if matches!(node.kind, Some(NodeKind::Agent(_))) {
                    ui.add_space(6.0);
                    let draft = self.drafts.entry(node.id.clone()).or_default();
                    if let Some(action) =
                        answer(ui, "details", &node, draft, ui.available_width() - 60.0)
                    {
                        self.sent.push(Command::Act {
                            node: node.id.clone(),
                            action,
                            request: 0,
                        });
                    }
                }
                if let Some(digest) = &node.digest {
                    ui.separator();
                    CommonMarkViewer::new().show(ui, &mut self.markdown, digest);
                }
                for run in &runs {
                    ui.separator();
                    ui.horizontal(|ui| {
                        dot(ui, color(ui.visuals(), run.state));
                        let name = run.name.as_deref().unwrap_or("unnamed");
                        ui.label(RichText::new(name).strong());
                        ui.weak(kind(run));
                    });
                    if let Some(doing) = &run.doing {
                        ui.weak(doing);
                    }
                    if let Some(digest) = &run.digest {
                        egui::CollapsingHeader::new("Digest")
                            .id_salt(("digest", &run.id))
                            .default_open(true)
                            .show(ui, |ui| {
                                CommonMarkViewer::new().show(ui, &mut self.markdown, digest);
                            });
                    }
                }
            });
    }

    fn tiles(&mut self, ui: &mut egui::Ui, width: f32, now: f64) {
        let waiting: Vec<(Node, Option<f64>)> = self
            .waiting()
            .map(|node| (node.clone(), self.held(&node.id, now)))
            .collect();
        let selected = self.selected.clone();
        self.drafts.retain(|id, _| {
            selected.as_ref() == Some(id) || waiting.iter().any(|(node, _)| node.id == *id)
        });
        let outer = TILE_WIDTH + 2.0 * TILE_MARGIN + TILE_GAP;
        let across = (((width + TILE_GAP) / outer).floor() as usize).max(1);
        for row in waiting.chunks(across) {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = TILE_GAP;
                for (node, held) in row {
                    let draft = self.drafts.entry(node.id.clone()).or_default();
                    if let Some(action) = tile(ui, node, *held, now, draft) {
                        self.sent.push(Command::Act {
                            node: node.id.clone(),
                            action,
                            request: 0,
                        });
                    }
                }
            });
            ui.add_space(TILE_GAP);
        }
    }

    /// `screen` is the height left on screen, which the map fits into while it stays readable.
    /// Clicking a node opens its details, and double-clicking an agent brings its terminal
    /// forward.
    fn map(&self, ui: &mut egui::Ui, width: f32, screen: f32, now: f64) -> Option<Picked> {
        let clusters = self.clusters();
        let scale = fit(&clusters, width, screen);
        let map = Map::lay_out(&clusters, width / scale);
        let (response, painter) =
            ui.allocate_painter(vec2(width, map.height * scale), Sense::click());
        let origin = response.rect.min;
        let ctx = ui.ctx().clone();
        // Positions glide to where a change in the room moves them; a new window size lays
        // the map out again at once.
        let glide = |key: &str, at: Vec2| {
            let id = egui::Id::new(("room-at", key, width as u32, screen as u32));
            let at = at * scale;
            origin
                + vec2(
                    ctx.animate_value_with_time(id.with(0), at.x, 0.5),
                    ctx.animate_value_with_time(id.with(1), at.y, 0.5),
                )
        };
        let visuals = ui.visuals().clone();

        let mut drawn: Vec<(&Node, Pos2, f32)> = Vec::new();
        for island in &map.islands {
            let key = island.repo.unwrap_or("");
            let corner = glide(&format!("island:{key}"), island.top.to_vec2());
            shore(&painter, &visuals, island, corner, scale);
            let at: BTreeMap<&str, Pos2> = island
                .placed
                .iter()
                .map(|p| {
                    let here = glide(&p.node.id, island.top.to_vec2() + p.at);
                    (p.node.id.as_str(), here)
                })
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
                    scale,
                );
            }
            for placed in island.placed.iter().rev() {
                let here = at[placed.node.id.as_str()];
                let radius = scale * if placed.depth == 0 { AGENT } else { CHILD };
                match placed.depth {
                    0 => body(&painter, &visuals, placed.node, here, radius, now, scale),
                    _ => {
                        let siblings = island
                            .placed
                            .iter()
                            .filter(|other| other.parent == placed.parent)
                            .count();
                        let label = placed.depth == 1 && siblings <= LABELLED;
                        started_body(
                            &painter,
                            &visuals,
                            placed.node,
                            here,
                            radius,
                            now,
                            scale,
                            label,
                        );
                    }
                }
                if self.selected.as_ref() == Some(&placed.node.id) {
                    let ring = Stroke::new(2.5, visuals.selection.stroke.color);
                    painter.circle_stroke(here, radius + 6.0 * scale, ring);
                }
                if placed.depth == 0 {
                    let children = island
                        .placed
                        .iter()
                        .filter(|child| child.parent == Some(placed.node.id.as_str()))
                        .map(|child| child.node);
                    let started = started(children);
                    caption(&painter, &visuals, placed.node, &started, here, scale);
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
        let Some(&(node, at, radius)) = hovered else {
            return response.clicked().then_some(Picked::Nothing);
        };
        painter.circle_stroke(
            at,
            radius + 4.0,
            Stroke::new(1.5, visuals.strong_text_color()),
        );
        let agent = matches!(node.kind, Some(NodeKind::Agent(_)));
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        let picked = response.clicked().then(|| Picked::Node(node.id.clone()));
        let held = self.held(&node.id, now);
        response.on_hover_ui_at_pointer(|ui| {
            about(ui, node, held);
            ui.weak(match agent {
                true => "Click for details, double-click to bring its terminal forward",
                false => "Click for details",
            });
        });
        picked
    }
}

/// What a click on the map landed on.
enum Picked {
    Node(NodeId),
    Nothing,
}

/// How soon after a click on an agent a double-click brings its terminal forward, in seconds:
/// longer than egui's own double-click delay.
const DOUBLE_CLICK: f64 = 0.6;

/// A run that ended well, or what it fanned out to. The map leaves it out under its agent,
/// where it reads as still going; a finished review stays for its digest, which its agent's
/// details show, or its own once its agent has left.
fn finished(node: &Node) -> bool {
    matches!(node.kind, Some(NodeKind::Run(_) | NodeKind::Subagent))
        && node.state == Some(NodeState::Done)
}

const TILE_WIDTH: f32 = 250.0;
const TILE_MARGIN: f32 = 14.0;
const TILE_GAP: f32 = 12.0;

const AGENT: f32 = 17.0;
const CHILD: f32 = 7.5;
/// How far an agent's caption reaches to either side.
const CAPTION_REACH: f32 = 88.0;
/// Under an agent: its name on up to two rows, what it is doing, its badges, and what it
/// started.
const CAPTION: f32 = 84.0;
/// An agent that started at most this many has each one named on the map.
const LABELLED: usize = 3;
const CELL_WIDTH: f32 = 2.0 * CAPTION_REACH + 12.0;
/// Above an agent that started nothing.
const HEADROOM: f32 = 10.0;
/// What an agent started sits on arcs above it, this many to an arc.
const FAN: usize = 7;
const FAN_NEAR: f32 = 34.0;
const FAN_STEP: f32 = 18.0;
/// An island's name above its agents, and the margin around them.
const TITLE: f32 = 28.0;
const PAD: f32 = 10.0;
const ISLAND_GAP: f32 = 16.0;
/// The smallest the map is drawn to fit the screen; below it names are too small to read.
const MIN_SCALE: f32 = 0.7;

#[derive(Debug)]
struct Placed<'a> {
    node: &'a Node,
    /// From the island's top left corner.
    at: Vec2,
    parent: Option<&'a str>,
    depth: usize,
}

#[derive(Debug)]
struct Island<'a> {
    repo: Option<&'a str>,
    top: Pos2,
    size: Vec2,
    placed: Vec<Placed<'a>>,
}

struct Map<'a> {
    islands: Vec<Island<'a>>,
    height: f32,
}

impl<'a> Map<'a> {
    fn lay_out(clusters: &[Cluster<'a>], width: f32) -> Self {
        let mut islands: Vec<Island> = clusters
            .iter()
            .map(|cluster| Island::of(cluster, width))
            .collect();
        let height = pack(&mut islands, width);
        Map { islands, height }
    }
}

impl<'a> Island<'a> {
    /// The agents in a grid, as square as fits `width`, each with what it started above it.
    fn of(cluster: &Cluster<'a>, width: f32) -> Self {
        let n = cluster.trees.len().max(1);
        let most = (((width - 2.0 * PAD - ISLAND_GAP) / CELL_WIDTH).floor() as usize).max(1);
        let cols = n.min((n as f32).sqrt().ceil().max(3.0) as usize).min(most);
        let rows = n.div_ceil(cols);
        let fan = cluster
            .trees
            .iter()
            .map(|tree| tree.children.len())
            .max()
            .unwrap_or(0);
        let above = match fan {
            0 => HEADROOM,
            _ => FAN_NEAR + (fan.div_ceil(FAN) - 1) as f32 * FAN_STEP + CHILD * 2.0,
        };
        let cell = above + AGENT * 2.0 + CAPTION;
        let mut placed = Vec::new();
        for (i, tree) in cluster.trees.iter().enumerate() {
            let at = vec2(
                PAD + (i % cols) as f32 * CELL_WIDTH + CELL_WIDTH / 2.0,
                TITLE + (i / cols) as f32 * cell + above + AGENT,
            );
            place(tree, at, None, None, 0, &mut placed);
        }
        Island {
            repo: cluster.repo,
            top: Pos2::ZERO,
            size: vec2(
                2.0 * PAD + cols as f32 * CELL_WIDTH,
                TITLE + rows as f32 * cell + PAD,
            ),
            placed,
        }
    }
}

fn place<'a>(
    tree: &Tree<'a>,
    at: Vec2,
    parent: Option<&'a str>,
    outward: Option<f32>,
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
    for (j, child) in tree.children.iter().enumerate() {
        let (angle, distance) = match outward {
            // An agent's caption is under it, so what it started fans out above.
            None => {
                let arc = j / FAN;
                let on_arc = (k - arc * FAN).min(FAN);
                let margin = 0.3;
                let angle = if on_arc == 1 {
                    -PI / 2.0
                } else {
                    -PI + margin + (PI - 2.0 * margin) * (j % FAN) as f32 / (on_arc - 1) as f32
                };
                (angle, FAN_NEAR + arc as f32 * FAN_STEP)
            }
            Some(angle) => (angle + (j as f32 - (k - 1) as f32 / 2.0) * 0.6, CHILD * 2.6),
        };
        let here = at + Vec2::angled(angle) * distance;
        place(
            child,
            here,
            Some(&tree.node.id),
            Some(angle),
            depth + 1,
            placed,
        );
    }
}

/// The largest scale, down to `MIN_SCALE`, at which the map laid out across the width it
/// would then have fits `screen` in height.
fn fit(clusters: &[Cluster], width: f32, screen: f32) -> f32 {
    let height = |scale: f32| Map::lay_out(clusters, width / scale).height * scale;
    if height(1.0) <= screen {
        return 1.0;
    }
    if height(MIN_SCALE) > screen {
        return MIN_SCALE;
    }
    let (mut low, mut high) = (MIN_SCALE, 1.0);
    for _ in 0..12 {
        let middle = (low + high) / 2.0;
        if height(middle) <= screen {
            low = middle;
        } else {
            high = middle;
        }
    }
    low
}

/// Places islands in rows across `width`, each row centred, and says the height it takes.
fn pack(islands: &mut [Island], width: f32) -> f32 {
    let mut rows: Vec<Vec<usize>> = vec![Vec::new()];
    let mut used = 0.0;
    for (i, island) in islands.iter().enumerate() {
        let size = island.size.x + ISLAND_GAP;
        let row = rows.last_mut().expect("rows starts with one");
        if !row.is_empty() && used + size > width {
            rows.push(vec![i]);
            used = size;
        } else {
            row.push(i);
            used += size;
        }
    }
    let mut top = ISLAND_GAP / 2.0;
    for row in rows.iter().filter(|row| !row.is_empty()) {
        let wide: f32 = row.iter().map(|&i| islands[i].size.x + ISLAND_GAP).sum();
        let tall = row.iter().map(|&i| islands[i].size.y).fold(0.0, f32::max);
        let mut left = ((width - wide) / 2.0).max(0.0) + ISLAND_GAP / 2.0;
        for &i in row {
            islands[i].top = pos2(left, top);
            left += islands[i].size.x + ISLAND_GAP;
        }
        top += tall + ISLAND_GAP;
    }
    top - ISLAND_GAP / 2.0
}

fn shore(
    painter: &egui::Painter,
    visuals: &egui::Visuals,
    island: &Island,
    corner: Pos2,
    scale: f32,
) {
    let rect = egui::Rect::from_min_size(corner, island.size * scale);
    let round = 16.0 * scale;
    let sea = hue(island.repo.unwrap_or(""), visuals.dark_mode);
    for (grow, alpha) in [(8.0, 0.05), (4.0, 0.08)] {
        let grow = grow * scale;
        painter.rect_filled(rect.expand(grow), round + grow, sea.gamma_multiply(alpha));
    }
    painter.rect_filled(rect, round, visuals.extreme_bg_color);
    painter.rect_filled(rect, round, sea.gamma_multiply(0.12));
    painter.rect_stroke(
        rect,
        round,
        Stroke::new(1.5, sea.gamma_multiply(0.6)),
        egui::StrokeKind::Inside,
    );
    let name = island.repo.map_or("elsewhere", short);
    painter.text(
        corner + vec2(PAD + 4.0, 8.0) * scale,
        Align2::LEFT_TOP,
        name,
        FontId::proportional(14.0 * scale),
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
    scale: f32,
) {
    let _ = visuals;
    let tint = made_by(node);
    let bend = (to - from).rot90().normalized() * 10.0 * scale;
    let curve = QuadraticBezierShape::from_points_stroke(
        [from, from + (to - from) / 2.0 + bend, to],
        false,
        Color32::TRANSPARENT,
        Stroke::new(1.5, tint.gamma_multiply(0.45)),
    );
    if node.state == Some(NodeState::Working) {
        let t = ((now * 0.6 + phase(&node.id)) % 1.0) as f32;
        painter.circle_filled(curve.sample(t), 2.5 * scale, tint);
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
    scale: f32,
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
                radius + (5.0 + 4.0 * breath) * scale,
                tint.gamma_multiply(0.15 + 0.2 * breath),
            );
        }
        _ => {}
    }
    let rim = Stroke::new(2.0, visuals.extreme_bg_color);
    let fill = tint.gamma_multiply(0.9);
    let codex = matches!(node.kind, Some(NodeKind::Agent(Provider::Codex)))
        || node.provider == Some(Provider::Codex);
    if codex {
        let square = egui::Rect::from_center_size(at, Vec2::splat(radius * 1.8));
        painter.rect_filled(square, radius * 0.4, fill);
        painter.rect_stroke(square, radius * 0.4, rim, egui::StrokeKind::Outside);
    } else if dibs_job(node) {
        let reach = radius * 1.25;
        let corners = [
            vec2(0.0, -reach),
            vec2(reach, 0.0),
            vec2(0.0, reach),
            vec2(-reach, 0.0),
        ];
        let diamond = corners.map(|corner| at + corner).to_vec();
        painter.add(egui::Shape::convex_polygon(diamond, fill, rim));
    } else {
        painter.circle_filled(at, radius, fill);
        painter.circle_stroke(at, radius, rim);
    }
    let mark = match node.state {
        Some(NodeState::Blocked) => "!",
        Some(NodeState::Failed) => "x",
        _ => return,
    };
    if radius >= AGENT * scale {
        painter.text(
            at,
            Align2::CENTER_CENTER,
            mark,
            FontId::proportional(16.0 * scale),
            visuals.extreme_bg_color,
        );
    }
}

fn caption(
    painter: &egui::Painter,
    visuals: &egui::Visuals,
    node: &Node,
    started: &[(String, Color32)],
    at: Pos2,
    scale: f32,
) {
    let top = at + vec2(0.0, (AGENT + 6.0) * scale);
    let name = node.name.as_deref().unwrap_or("unnamed");
    let name = lines(
        painter,
        name,
        13.0 * scale,
        visuals.strong_text_color(),
        2,
        scale,
    );
    let below = top + vec2(0.0, name.size().y + 2.0 * scale);
    painter.galley(top, name, visuals.strong_text_color());
    if let Some(doing) = &node.doing {
        let doing = lines(
            painter,
            doing,
            11.0 * scale,
            visuals.weak_text_color(),
            1,
            scale,
        );
        painter.galley(below, doing, visuals.weak_text_color());
    }
    let badges = badges(node, visuals);
    if !badges.is_empty() {
        let badges = row(painter, &badges, 10.0 * scale, scale);
        painter.galley(
            below + vec2(0.0, 15.0 * scale),
            badges,
            visuals.text_color(),
        );
    }
    if !started.is_empty() {
        let dot = ("  ·  ".to_string(), visuals.weak_text_color());
        let parts: Vec<(String, Color32)> = started
            .iter()
            .enumerate()
            .flat_map(|(i, part)| {
                (i > 0)
                    .then(|| dot.clone())
                    .into_iter()
                    .chain([part.clone()])
            })
            .collect();
        let started = row(painter, &parts, 10.5 * scale, scale);
        painter.galley(
            below + vec2(0.0, 29.0 * scale),
            started,
            visuals.text_color(),
        );
    }
}

/// Coloured parts on one row centred under an agent, its end cut when it runs over.
fn row(
    painter: &egui::Painter,
    parts: &[(String, Color32)],
    size: f32,
    scale: f32,
) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::default();
    for (text, tint) in parts {
        job.append(
            text,
            0.0,
            egui::TextFormat::simple(FontId::proportional(size), *tint),
        );
    }
    job.wrap.max_width = 2.0 * CAPTION_REACH * scale;
    job.wrap.max_rows = 1;
    job.halign = egui::Align::Center;
    painter.layout_job(job)
}

/// How a pull request's CI stands, in words and in its colour.
fn checks(checks: Checks) -> (&'static str, Color32) {
    match checks {
        Checks::Passing => ("CI passing", Color32::from_rgb(80, 190, 120)),
        Checks::Failing => ("CI failing", Color32::from_rgb(230, 85, 75)),
        Checks::Running => ("CI running", Color32::from_rgb(230, 170, 60)),
    }
}

fn ci(ui: &mut egui::Ui, state: Option<Checks>) {
    if let Some((words, tint)) = state.map(checks) {
        ui.label(RichText::new(words).color(tint).small());
    }
}

/// What an agent carries: its context, and its open pull requests, each in the colour of
/// its CI.
fn badges(node: &Node, visuals: &egui::Visuals) -> Vec<(String, Color32)> {
    let text = visuals.text_color();
    let mut parts = Vec::new();
    if let Some(tokens) = node.context {
        parts.push((format!("{} context", thousands(tokens)), text));
    }
    let prs = open_prs(node);
    if !prs.is_empty() {
        if !parts.is_empty() {
            parts.push(("  ·  ".to_string(), visuals.weak_text_color()));
        }
        parts.push(("PR".to_string(), text));
        for pr in prs {
            let tint = pr.checks.map_or(text, |state| checks(state).1);
            parts.push((format!(" #{}", pr.number), tint));
        }
    }
    parts
}

/// What an agent started, counted by kind and by whether it runs, each in its kind's
/// colour: "1 subagent running", "2 Codex reviews waiting".
fn started<'a>(children: impl Iterator<Item = &'a Node>) -> Vec<(String, Color32)> {
    let mut counted: BTreeMap<(String, &str), (usize, Color32)> = BTreeMap::new();
    for child in children {
        let what = match &child.kind {
            Some(NodeKind::Subagent) if child.provider != Some(Provider::Codex) => {
                "subagent".to_string()
            }
            _ => kind(child),
        };
        let how = match child.state {
            Some(NodeState::Working) => "running",
            Some(NodeState::Failed) => "failed",
            Some(NodeState::Done) => "done",
            _ => "waiting",
        };
        counted.entry((what, how)).or_insert((0, made_by(child))).0 += 1;
    }
    counted
        .into_iter()
        .map(|((what, how), (count, tint))| match count {
            1 => (format!("1 {what} {how}"), tint),
            _ => (format!("{count} {what}s {how}"), tint),
        })
        .collect()
}

/// The colour of what an agent started, by what runs it: a Claude subagent, a Codex run or
/// reviewer, a dibs job.
fn made_by(node: &Node) -> Color32 {
    if dibs_job(node) {
        Color32::from_rgb(175, 125, 245)
    } else if node.provider == Some(Provider::Codex) {
        Color32::from_rgb(70, 150, 245)
    } else {
        Color32::from_rgb(225, 125, 85)
    }
}

/// Something an agent started: its kind's shape and colour, filled with a turning arc
/// around it while it runs, an outline while it waits, and outlined red once it failed.
#[allow(clippy::too_many_arguments)]
fn started_body(
    painter: &egui::Painter,
    visuals: &egui::Visuals,
    node: &Node,
    at: Pos2,
    radius: f32,
    now: f64,
    scale: f32,
    label: bool,
) {
    let tint = made_by(node);
    let running = node.state == Some(NodeState::Working);
    let (fill, rim) = match node.state {
        Some(NodeState::Working) => (tint, Stroke::new(1.5, visuals.extreme_bg_color)),
        Some(NodeState::Failed) => (
            visuals.extreme_bg_color,
            Stroke::new(2.0, visuals.error_fg_color),
        ),
        _ => (visuals.extreme_bg_color, Stroke::new(2.0, tint)),
    };
    if dibs_job(node) {
        let reach = radius * 1.3;
        let corners = [
            vec2(0.0, -reach),
            vec2(reach, 0.0),
            vec2(0.0, reach),
            vec2(-reach, 0.0),
        ];
        let diamond = corners.map(|corner| at + corner).to_vec();
        painter.add(egui::Shape::convex_polygon(diamond, fill, rim));
    } else if node.provider == Some(Provider::Codex) {
        let square = egui::Rect::from_center_size(at, Vec2::splat(radius * 1.8));
        painter.rect_filled(square, radius * 0.35, fill);
        painter.rect_stroke(square, radius * 0.35, rim, egui::StrokeKind::Outside);
    } else {
        painter.circle_filled(at, radius, fill);
        painter.circle_stroke(at, radius, rim);
    }
    if running {
        let turn = (now * 0.35 + phase(&node.id)) as f32 * 2.0 * PI;
        let reach = radius + 3.5 * scale;
        let arc: Vec<Pos2> = (0..=12)
            .map(|i| turn + i as f32 / 12.0 * PI * 0.7)
            .map(|angle| at + Vec2::angled(angle) * reach)
            .collect();
        let stroke = Stroke::new(1.2 * scale, tint.gamma_multiply(0.6));
        painter.add(egui::Shape::line(arc, stroke));
    }
    if label {
        let word = match &node.kind {
            _ if dibs_job(node) => "dibs",
            Some(NodeKind::Run(kind)) => kind.as_str(),
            _ => "subagent",
        };
        painter.text(
            at - vec2(0.0, radius + 6.0 * scale),
            Align2::CENTER_BOTTOM,
            word,
            FontId::proportional(9.5 * scale),
            tint,
        );
    }
}

/// A pull request as the page shows it.
struct Shown<'a> {
    number: &'a str,
    /// Its repository, number, whether it is a draft, and title.
    about: String,
    link: &'a str,
    checks: Option<Checks>,
}

fn number(link: &str) -> &str {
    link.rsplit('/').next().unwrap_or_default()
}

/// The open pull requests an agent opened, and the one its board note waits on.
fn open_prs(node: &Node) -> Vec<Shown<'_>> {
    let mut shown: Vec<Shown> = node
        .prs
        .iter()
        .map(|pr| {
            let draft = if pr.draft { " (draft)" } else { "" };
            Shown {
                number: number(&pr.url),
                about: format!("{}#{}{draft}: {}", short(&pr.repo), pr.number, pr.title),
                link: &pr.url,
                checks: pr.checks,
            }
        })
        .collect();
    if let Some(link) = node.note.as_ref().and_then(|note| note.pr.as_deref())
        && !shown.iter().any(|pr| pr.link == link)
    {
        shown.push(Shown {
            number: number(link),
            about: link.trim_start_matches("https://github.com/").to_string(),
            link,
            checks: None,
        });
    }
    shown
}

fn dibs_job(node: &Node) -> bool {
    matches!(&node.kind, Some(NodeKind::Run(kind)) if kind == "dibs")
}

fn thousands(tokens: u64) -> String {
    match tokens {
        0..1000 => tokens.to_string(),
        1000..1_000_000 => format!("{}k", tokens / 1000),
        _ => format!("{:.1}M", tokens as f64 / 1e6),
    }
}

/// Text centred on the point it is painted at, wrapped to a caption's width, its last row
/// ending in an ellipsis when it runs over `rows`.
fn lines(
    painter: &egui::Painter,
    text: &str,
    size: f32,
    tint: Color32,
    rows: usize,
    scale: f32,
) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple(
        text.to_string(),
        FontId::proportional(size),
        tint,
        2.0 * CAPTION_REACH * scale,
    );
    job.wrap.max_rows = rows;
    job.halign = egui::Align::Center;
    painter.layout_job(job)
}

/// What the person asked of the agent from its tile: an answer typed in it, or its terminal.
fn tile(
    ui: &mut egui::Ui,
    node: &Node,
    held: Option<f64>,
    now: f64,
    draft: &mut String,
) -> Option<Action> {
    let mut action = None;
    let warn = ui.visuals().warn_fg_color;
    let breath = (0.5 + 0.5 * (now * 3.0).sin()) as f32;
    egui::Frame::new()
        .fill(warn.gamma_multiply(0.08 + 0.04 * breath))
        .stroke(Stroke::new(1.5, warn.gamma_multiply(0.6 + 0.4 * breath)))
        .corner_radius(12.0)
        .inner_margin(TILE_MARGIN)
        .show(ui, |ui| {
            ui.set_width(TILE_WIDTH);
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    dot(ui, warn);
                    ui.label(
                        RichText::new(node.name.as_deref().unwrap_or("unnamed"))
                            .size(17.0)
                            .strong(),
                    );
                });
                let mut place = kind(node);
                if let Some(repo) = &node.repo {
                    place.push_str(&format!(" in {}", short(repo)));
                }
                ui.label(RichText::new(place).weak());
                let note = node.note.as_ref().map(|note| note.text.as_str());
                let reason = note
                    .filter(|text| !text.is_empty())
                    .or(node.doing.as_deref());
                if let Some(reason) = reason {
                    ui.add(egui::Label::new(reason).wrap());
                }
                if let Some(held) = held {
                    ui.label(RichText::new(format!("waiting {}", ago(held))).color(warn));
                }
                for pr in open_prs(node) {
                    ui.horizontal(|ui| {
                        ui.hyperlink_to(format!("PR #{}", pr.number), pr.link)
                            .on_hover_text(&pr.about);
                        ci(ui, pr.checks);
                    });
                }
                ui.add_space(4.0);
                action = answer(ui, "tile", node, draft, TILE_WIDTH - 60.0);
            });
        });
    action
}

/// A box to write to the agent in, and a link to its terminal. `place` keeps the tile's box
/// and the details' apart when both show one agent.
fn answer(
    ui: &mut egui::Ui,
    place: &str,
    node: &Node,
    draft: &mut String,
    width: f32,
) -> Option<Action> {
    let mut action = None;
    ui.horizontal(|ui| {
        let answer = ui.add(
            egui::TextEdit::singleline(draft)
                .id(egui::Id::new(("answer", place, &node.id)))
                .hint_text("Answer")
                .desired_width(width),
        );
        let entered = answer.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
        let send = ui.button("Send").clicked();
        if (entered || send) && !draft.trim().is_empty() {
            let text = std::mem::take(draft).trim().to_string();
            action = Some(Action::Prompt(text));
        }
    });
    if ui.link("Bring its terminal forward").clicked() {
        action = Some(Action::Focus);
    }
    action
}

fn about(ui: &mut egui::Ui, node: &Node, held: Option<f64>) {
    ui.label(RichText::new(node.name.as_deref().unwrap_or("unnamed")).strong());
    let mut line = format!("{} {}", kind(node), state(node.state).to_lowercase());
    if let Some(held) = held {
        line.push_str(&format!(" for {}", ago(held)));
    }
    ui.label(line);
    if let Some(doing) = &node.doing {
        ui.label(RichText::new(doing).weak());
    }
    if let Some(note) = node.note.as_ref().filter(|note| !note.text.is_empty()) {
        ui.label(format!("noted: {}", note.text));
    }
    if let Some(tokens) = node.context {
        ui.label(format!("{} tokens in context", thousands(tokens)));
    }
    for pr in open_prs(node) {
        let ci = pr.checks.map(|state| format!(", {}", checks(state).0));
        ui.label(format!("PR {}{}", pr.about, ci.unwrap_or_default()));
    }
    if let Some(repo) = &node.repo {
        ui.label(RichText::new(repo).weak().monospace());
    }
    if let Some(machine) = &node.machine {
        ui.label(RichText::new(format!("on {machine}")).weak());
    }
}

fn legend(ui: &mut egui::Ui) {
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
    ui.weak("round: Claude, square: Codex, diamond: dibs job.");
    ui.add_space(6.0);
    for (word, tint) in [
        ("Claude subagent", made_by(&Node::default())),
        (
            "Codex run",
            made_by(&Node {
                provider: Some(Provider::Codex),
                ..Node::default()
            }),
        ),
        (
            "dibs job",
            made_by(&Node {
                kind: Some(NodeKind::Run("dibs".to_string())),
                ..Node::default()
            }),
        ),
    ] {
        ui.label(RichText::new(word).color(tint));
    }
    ui.weak("around an agent: filled and turning while it runs, hollow while it waits.");
    ui.add_space(10.0);
    ui.weak("click one for details");
}

fn dot(ui: &mut egui::Ui, tint: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(12.0, 12.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 5.0, tint);
}

/// What a node is, as a person says it: "Claude", "Codex review", "dibs job".
fn kind(node: &Node) -> String {
    let by = node.provider.as_ref().map(provider);
    match (&node.kind, by) {
        (Some(NodeKind::Agent(agent)), _) => provider(agent),
        (Some(NodeKind::Run(_)), _) if dibs_job(node) => "dibs job".to_string(),
        (Some(NodeKind::Run(kind)), Some(by)) => format!("{by} {kind}"),
        (Some(NodeKind::Run(kind)), None) => kind.clone(),
        (Some(NodeKind::Subagent), Some(by)) => format!("{by} subagent"),
        (Some(NodeKind::Subagent), None) => "subagent".to_string(),
        (None, _) => String::new(),
    }
}

fn provider(provider: &Provider) -> String {
    match provider {
        Provider::Claude => "Claude".to_string(),
        Provider::Codex => "Codex".to_string(),
        Provider::Other(name) => name.clone(),
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
    fn a_finished_review_leaves_the_map_but_not_the_room() {
        let mut room = Room::default();
        let run = |id: &str, state| Node {
            kind: Some(NodeKind::Run("review".to_string())),
            state: Some(state),
            ..node(id, Some("/src/alpha"), Some("a"))
        };
        room.apply(RoomEvent::Snapshot(vec![
            node("a", Some("/src/alpha"), None),
            run("done", NodeState::Done),
            run("going", NodeState::Working),
        ]));
        let clusters = room.clusters();
        let shown: Vec<_> = clusters[0].trees[0]
            .children
            .iter()
            .map(|child| child.node.id.as_str())
            .collect();
        assert_eq!(shown, ["going"]);
        assert!(room.nodes.contains_key("done"));
        room.apply(RoomEvent::Removed("a".to_string()));
        let roots: Vec<_> = room.clusters()[0]
            .trees
            .iter()
            .map(|tree| tree.node.id.as_str())
            .collect();
        assert_eq!(
            roots,
            ["done", "going"],
            "its agent left, so it is the way to its digest"
        );
    }

    #[test]
    fn changes_apply_by_id() {
        let mut room = Room::default();
        room.apply(RoomEvent::Snapshot(vec![node("a", None, None)]));
        room.apply(RoomEvent::Upserted(Box::new(Node {
            state: Some(NodeState::Blocked),
            ..node("a", None, None)
        })));
        room.apply(RoomEvent::Upserted(Box::new(node("b", None, None))));
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
        room.apply(RoomEvent::Upserted(Box::new(Node {
            state: Some(NodeState::Blocked),
            ..node("a", None, None)
        })));
        room.apply(RoomEvent::Upserted(Box::new(node("b", None, None))));
        room.stamp(20.0);
        room.stamp(30.0);
        assert_eq!(room.held("a", 50.0), Some(30.0));
        assert_eq!(room.held("b", 50.0), Some(30.0), "arrived while watching");
    }

    #[test]
    fn islands_hold_their_nodes_and_never_overlap() {
        let mut room = Room::default();
        let mut nodes = vec![node("lone", Some("/src/beta"), None)];
        for i in 0..5 {
            nodes.push(node(&format!("agent-{i}"), Some("/src/alpha"), None));
            for j in 0..9 {
                let parent = format!("agent-{i}");
                nodes.push(node(
                    &format!("sub-{i}-{j}"),
                    Some("/src/alpha"),
                    Some(&parent),
                ));
            }
        }
        room.apply(RoomEvent::Snapshot(nodes));
        let width = 900.0;
        let map = Map::lay_out(&room.clusters(), width);
        for island in &map.islands {
            let rect = egui::Rect::from_min_size(Pos2::ZERO, island.size);
            for placed in &island.placed {
                let reach = if placed.depth == 0 { AGENT } else { CHILD };
                let node =
                    egui::Rect::from_center_size(placed.at.to_pos2(), Vec2::splat(2.0 * reach));
                assert!(rect.contains_rect(node), "{placed:?} outside {rect:?}");
            }
            assert!(island.top.x >= 0.0 && island.top.x + island.size.x <= width);
        }
        let [alpha, beta] = &map.islands[..] else {
            panic!("two islands");
        };
        let alpha = egui::Rect::from_min_size(alpha.top, alpha.size);
        let beta = egui::Rect::from_min_size(beta.top, beta.size);
        assert!(!alpha.intersects(beta));
        assert!(map.height >= alpha.bottom().max(beta.bottom()));
    }

    #[test]
    fn the_map_shrinks_to_fit_the_screen_while_it_stays_readable() {
        let mut room = Room::default();
        let nodes = (0..20)
            .map(|i| node(&format!("agent-{i}"), Some(&format!("/src/repo-{i}")), None))
            .collect();
        room.apply(RoomEvent::Snapshot(nodes));
        let clusters = room.clusters();
        let width = 1400.0;
        let tall = Map::lay_out(&clusters, width).height;
        assert_eq!(fit(&clusters, width, tall + 1.0), 1.0);
        let scale = fit(&clusters, width, tall * 0.8);
        assert!((MIN_SCALE..1.0).contains(&scale));
        assert!(Map::lay_out(&clusters, width / scale).height * scale <= tall * 0.8);
        assert_eq!(fit(&clusters, width, 10.0), MIN_SCALE);
    }

    #[test]
    fn what_an_agent_started_says_who_runs_it() {
        let review = Node {
            kind: Some(NodeKind::Run("review".into())),
            provider: Some(Provider::Codex),
            ..node("r", None, None)
        };
        let reviewer = Node {
            kind: Some(NodeKind::Subagent),
            provider: Some(Provider::Codex),
            ..node("s", None, None)
        };
        let job = Node {
            kind: Some(NodeKind::Run("dibs".into())),
            ..node("j", None, None)
        };
        assert_eq!(kind(&review), "Codex review");
        assert_eq!(kind(&reviewer), "Codex subagent");
        assert_eq!(kind(&job), "dibs job");
        let waiting = Node {
            state: Some(NodeState::Idle),
            ..review.clone()
        };
        let running = Node {
            state: Some(NodeState::Working),
            ..job.clone()
        };
        let subagent = Node {
            kind: Some(NodeKind::Subagent),
            provider: Some(Provider::Claude),
            state: Some(NodeState::Working),
            ..node("c", None, None)
        };
        let started: Vec<String> = started([&waiting, &waiting, &running, &subagent].into_iter())
            .into_iter()
            .map(|(text, _)| text)
            .collect();
        assert_eq!(
            started,
            [
                "2 Codex reviews waiting",
                "1 dibs job running",
                "1 subagent running"
            ]
        );
    }
}
