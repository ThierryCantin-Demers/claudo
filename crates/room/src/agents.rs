//! The agents the daemon's collectors see: what changed and when, which wait on the person,
//! how they cluster by repository with what each started under it, where each sits on the
//! map, and what is said about each, in words and colour.

use std::collections::BTreeMap;
use std::f32::consts::PI;
use std::ops::{Add, Div, Mul, Sub};

use claudo_protocol::{Checks, Node, NodeId, NodeKind, NodeState, Provider, RoomEvent};

/// A position or size on the map, in the map's own units before it is scaled to the screen.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct V {
    pub x: f32,
    pub y: f32,
}

pub const fn v(x: f32, y: f32) -> V {
    V { x, y }
}

impl V {
    /// One unit long, `angle` radians clockwise from the right, y growing down.
    pub fn angled(angle: f32) -> V {
        v(angle.cos(), angle.sin())
    }
}

impl Add for V {
    type Output = V;
    fn add(self, other: V) -> V {
        v(self.x + other.x, self.y + other.y)
    }
}

impl Sub for V {
    type Output = V;
    fn sub(self, other: V) -> V {
        v(self.x - other.x, self.y - other.y)
    }
}

impl Mul<f32> for V {
    type Output = V;
    fn mul(self, by: f32) -> V {
        v(self.x * by, self.y * by)
    }
}

impl Div<f32> for V {
    type Output = V;
    fn div(self, by: f32) -> V {
        v(self.x / by, self.y / by)
    }
}

/// A colour the frontend picks from its theme, or one of the room's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tint {
    Text,
    Weak,
    Warn,
    Error,
    Rgb([u8; 3]),
}

/// Every node, and since when each has been in its state, by a clock the frontend gives in
/// seconds. A state a node already had when the frontend first saw it has no start.
#[derive(Default)]
pub struct Agents {
    pub nodes: BTreeMap<NodeId, Node>,
    since: BTreeMap<NodeId, (Option<NodeState>, Option<f64>)>,
    primed: bool,
}

/// A repository's agents, each with what it started.
#[derive(Debug, PartialEq)]
pub struct Cluster<'a> {
    pub repo: Option<&'a str>,
    pub trees: Vec<Tree<'a>>,
}

#[derive(Debug, PartialEq)]
pub struct Tree<'a> {
    pub node: &'a Node,
    pub children: Vec<Tree<'a>>,
}

impl Agents {
    /// Applies a change, and gives back an action's failure: which node, and why.
    pub fn apply(&mut self, event: RoomEvent) -> Option<(NodeId, String)> {
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
            RoomEvent::Failed { node, detail, .. } => return Some((node, detail)),
        }
        None
    }

    /// Notes, at `now`, each state that changed since the last call.
    pub fn stamp(&mut self, now: f64) {
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

    /// How long a node has been in its state, when the frontend saw it change.
    pub fn held(&self, id: &str, now: f64) -> Option<f64> {
        self.since.get(id)?.1.map(|at| now - at)
    }

    pub fn waiting(&self) -> impl Iterator<Item = &Node> {
        self.nodes
            .values()
            .filter(|node| node.state == Some(NodeState::Blocked))
    }

    pub fn agents(&self) -> usize {
        self.nodes
            .values()
            .filter(|node| matches!(node.kind, Some(NodeKind::Agent(_))))
            .count()
    }

    pub fn count(&self, state: NodeState) -> usize {
        self.nodes
            .values()
            .filter(|node| node.state == Some(state))
            .count()
    }

    /// Something on the map moves while one works or waits.
    pub fn moving(&self) -> bool {
        self.nodes
            .values()
            .any(|node| matches!(node.state, Some(NodeState::Working | NodeState::Blocked)))
    }

    /// Every open pull request an agent here has, with the agent, by what it is about.
    pub fn pull_requests(&self) -> Vec<(&Node, Shown<'_>)> {
        let mut open: Vec<(&Node, Shown)> = self
            .nodes
            .values()
            .flat_map(|node| open_prs(node).into_iter().map(move |pr| (node, pr)))
            .collect();
        open.sort_by(|a, b| a.1.about.cmp(&b.1.about));
        open
    }

    /// The runs `id` started, the newest first.
    pub fn runs(&self, id: &str) -> Vec<&Node> {
        let mut runs: Vec<&Node> = self
            .nodes
            .values()
            .filter(|run| run.parent.as_deref() == Some(id))
            .filter(|run| matches!(run.kind, Some(NodeKind::Run(_))))
            .collect();
        // Run folders begin with when they started.
        runs.sort_by(|a, b| b.id.cmp(&a.id));
        runs
    }

    /// The agent that started `node`, when it is one the room shows.
    pub fn agent_of(&self, node: &Node) -> Option<&Node> {
        self.nodes
            .get(node.parent.as_deref()?)
            .filter(|parent| matches!(parent.kind, Some(NodeKind::Agent(_))))
    }

    /// Repositories by name, then the agents in none. A node whose parent is gone is a root.
    pub fn clusters(&self) -> Vec<Cluster<'_>> {
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
}

/// A run that ended well, or what it fanned out to. The map leaves it out under its agent,
/// where it reads as still going; a finished review stays for its digest, which its agent's
/// details show, or its own once its agent has left.
pub fn finished(node: &Node) -> bool {
    matches!(node.kind, Some(NodeKind::Run(_) | NodeKind::Subagent))
        && node.state == Some(NodeState::Done)
}

pub const AGENT: f32 = 17.0;
pub const CHILD: f32 = 7.5;
/// How far an agent's caption reaches to either side.
pub const CAPTION_REACH: f32 = 72.0;
/// Under an agent, a row each: its name, what it is doing, its badges, and what it started.
const CAPTION: f32 = 62.0;
/// An agent that started at most this many has each one named on the map.
pub const LABELLED: usize = 3;
const CELL_WIDTH: f32 = 2.0 * CAPTION_REACH + 12.0;
/// Above an agent that started nothing.
const HEADROOM: f32 = 10.0;
/// What an agent started sits on arcs above it, this many to an arc.
const FAN: usize = 7;
const FAN_NEAR: f32 = 34.0;
const FAN_STEP: f32 = 18.0;
/// An island's name above its agents, and the margin around them.
const TITLE: f32 = 24.0;
pub const PAD: f32 = 8.0;
const ISLAND_GAP: f32 = 12.0;
/// The smallest the map is drawn to fit the screen; below it names are too small to read.
pub const MIN_SCALE: f32 = 0.55;

#[derive(Debug)]
pub struct Placed<'a> {
    pub node: &'a Node,
    /// From the island's top left corner.
    pub at: V,
    pub parent: Option<&'a str>,
    pub depth: usize,
}

#[derive(Debug)]
pub struct Island<'a> {
    pub repo: Option<&'a str>,
    pub top: V,
    pub size: V,
    pub placed: Vec<Placed<'a>>,
}

pub struct Map<'a> {
    pub islands: Vec<Island<'a>>,
    pub height: f32,
}

impl<'a> Map<'a> {
    pub fn lay_out(clusters: &[Cluster<'a>], width: f32) -> Self {
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
            let at = v(
                PAD + (i % cols) as f32 * CELL_WIDTH + CELL_WIDTH / 2.0,
                TITLE + (i / cols) as f32 * cell + above + AGENT,
            );
            place(tree, at, None, None, 0, &mut placed);
        }
        Island {
            repo: cluster.repo,
            top: V::default(),
            size: v(
                2.0 * PAD + cols as f32 * CELL_WIDTH,
                TITLE + rows as f32 * cell + PAD,
            ),
            placed,
        }
    }

    /// The nodes `parent` started on this island.
    pub fn children(&self, parent: &str) -> impl Iterator<Item = &Placed<'a>> {
        self.placed
            .iter()
            .filter(move |child| child.parent == Some(parent))
    }
}

fn place<'a>(
    tree: &Tree<'a>,
    at: V,
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
        let here = at + V::angled(angle) * distance;
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
pub fn fit(clusters: &[Cluster], width: f32, screen: f32) -> f32 {
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
            islands[i].top = v(left, top);
            left += islands[i].size.x + ISLAND_GAP;
        }
        top += tall + ISLAND_GAP;
    }
    top - ISLAND_GAP / 2.0
}

/// A pull request as the room shows it.
pub struct Shown<'a> {
    pub number: &'a str,
    /// Its repository, number, whether it is a draft, and title.
    pub about: String,
    pub link: &'a str,
    pub checks: Option<Checks>,
}

fn number(link: &str) -> &str {
    link.rsplit('/').next().unwrap_or_default()
}

/// The open pull requests an agent opened, and the one its board note waits on.
pub fn open_prs(node: &Node) -> Vec<Shown<'_>> {
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

/// How a pull request's CI stands, in words and in its colour.
pub fn checks(checks: Checks) -> (&'static str, [u8; 3]) {
    match checks {
        Checks::Passing => ("CI passing", [80, 190, 120]),
        Checks::Failing => ("CI failing", [230, 85, 75]),
        Checks::Running => ("CI running", [230, 170, 60]),
    }
}

/// What an agent carries: its context, and its open pull requests, each in the colour of
/// its CI.
pub fn badges(node: &Node) -> Vec<(String, Tint)> {
    let mut parts = Vec::new();
    if let Some(tokens) = node.context {
        parts.push((format!("{} context", thousands(tokens)), Tint::Text));
    }
    let prs = open_prs(node);
    if !prs.is_empty() {
        if !parts.is_empty() {
            parts.push(("  ·  ".to_string(), Tint::Weak));
        }
        parts.push(("PR".to_string(), Tint::Text));
        for pr in prs {
            let tint = pr
                .checks
                .map_or(Tint::Text, |state| Tint::Rgb(checks(state).1));
            parts.push((format!(" #{}", pr.number), tint));
        }
    }
    parts
}

/// What an agent started, counted by kind and by whether it runs, each in its kind's
/// colour: "1 subagent running", "2 Codex reviews waiting".
pub fn started<'a>(children: impl Iterator<Item = &'a Node>) -> Vec<(String, [u8; 3])> {
    let mut counted: BTreeMap<(String, &str), (usize, [u8; 3])> = BTreeMap::new();
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
pub fn made_by(node: &Node) -> [u8; 3] {
    if dibs_job(node) {
        [175, 125, 245]
    } else if node.provider == Some(Provider::Codex) {
        [70, 150, 245]
    } else {
        [225, 125, 85]
    }
}

/// A node drawn as a square rather than round: a Codex agent or run.
pub fn codex(node: &Node) -> bool {
    matches!(node.kind, Some(NodeKind::Agent(Provider::Codex)))
        || node.provider == Some(Provider::Codex)
}

pub fn dibs_job(node: &Node) -> bool {
    matches!(&node.kind, Some(NodeKind::Run(kind)) if kind == "dibs")
}

/// The word over something an agent started, when there is room to name each.
pub fn started_word(node: &Node) -> &str {
    match &node.kind {
        _ if dibs_job(node) => "dibs",
        Some(NodeKind::Run(kind)) => kind.as_str(),
        _ => "subagent",
    }
}

pub fn thousands(tokens: u64) -> String {
    match tokens {
        0..1000 => tokens.to_string(),
        1000..1_000_000 => format!("{}k", tokens / 1000),
        _ => format!("{:.1}M", tokens as f64 / 1e6),
    }
}

/// What a node is, as a person says it: "Claude", "Codex review", "dibs job".
pub fn kind(node: &Node) -> String {
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

pub fn provider(provider: &Provider) -> String {
    match provider {
        Provider::Claude => "Claude".to_string(),
        Provider::Codex => "Codex".to_string(),
        Provider::Other(name) => name.clone(),
    }
}

pub fn state(state: Option<NodeState>) -> &'static str {
    match state {
        Some(NodeState::Working) => "Working",
        Some(NodeState::Blocked) => "Blocked",
        Some(NodeState::Idle) => "Idle",
        Some(NodeState::Done) => "Done",
        Some(NodeState::Failed) => "Failed",
        Some(NodeState::Unknown) | None => "Unknown",
    }
}

pub fn state_tint(state: Option<NodeState>) -> Tint {
    match state {
        Some(NodeState::Working) => Tint::Rgb([80, 190, 120]),
        Some(NodeState::Blocked) => Tint::Warn,
        Some(NodeState::Failed) => Tint::Error,
        Some(NodeState::Done) => Tint::Rgb([90, 140, 230]),
        Some(NodeState::Idle) | Some(NodeState::Unknown) | None => Tint::Weak,
    }
}

/// The states the legend names, in its order.
pub const LEGEND: [NodeState; 5] = [
    NodeState::Working,
    NodeState::Blocked,
    NodeState::Idle,
    NodeState::Done,
    NodeState::Failed,
];

/// What the legend says the shapes and colours of what an agent started mean.
pub fn legend_kinds() -> [(&'static str, [u8; 3]); 3] {
    [
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
    ]
}

/// A repository's own colour, the same on every page.
pub fn hue(repo: &str, dark: bool) -> [u8; 3] {
    let h = repo
        .bytes()
        .fold(7u32, |h, b| h.wrapping_mul(31).wrapping_add(u32::from(b)));
    let (s, value) = if dark { (0.45, 0.75) } else { (0.5, 0.8) };
    hsv((h % 360) as f32, s, value)
}

fn hsv(hue: f32, saturation: f32, value: f32) -> [u8; 3] {
    let chroma = value * saturation;
    let x = chroma * (1.0 - ((hue / 60.0) % 2.0 - 1.0).abs());
    let (r, g, b) = match hue as u32 / 60 {
        0 => (chroma, x, 0.0),
        1 => (x, chroma, 0.0),
        2 => (0.0, chroma, x),
        3 => (0.0, x, chroma),
        4 => (x, 0.0, chroma),
        _ => (chroma, 0.0, x),
    };
    let m = value - chroma;
    [r, g, b].map(|c| ((c + m) * 255.0).round() as u8)
}

/// Where in its pulse a node starts, so neighbours do not beat together.
pub fn phase(id: &str) -> f64 {
    id.bytes().map(u64::from).sum::<u64>() as f64 % 97.0 / 97.0
}

pub fn ago(seconds: f64) -> String {
    let seconds = seconds.max(0.0) as u64;
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        _ => format!("{}h {}m", seconds / 3600, seconds % 3600 / 60),
    }
}

/// The last part of a path: a repository's folder name.
pub fn short(repo: &str) -> &str {
    repo.rsplit('/').next().unwrap_or(repo)
}

/// Where a node sits, what it is doing and why it waits, for a hover or its details.
pub fn about(node: &Node, held: Option<f64>) -> Vec<String> {
    let mut lines = vec![node.name.as_deref().unwrap_or("unnamed").to_string()];
    let mut line = format!("{} {}", kind(node), state(node.state).to_lowercase());
    if let Some(held) = held {
        line.push_str(&format!(" for {}", ago(held)));
    }
    lines.push(line);
    if let Some(doing) = &node.doing {
        lines.push(doing.clone());
    }
    if let Some(note) = node.note.as_ref().filter(|note| !note.text.is_empty()) {
        lines.push(format!("noted: {}", note.text));
    }
    if let Some(tokens) = node.context {
        lines.push(format!("{} tokens in context", thousands(tokens)));
    }
    for pr in open_prs(node) {
        let ci = pr.checks.map(|state| format!(", {}", checks(state).0));
        lines.push(format!("PR {}{}", pr.about, ci.unwrap_or_default()));
    }
    if let Some(repo) = &node.repo {
        lines.push(repo.clone());
    }
    if let Some(machine) = &node.machine {
        lines.push(format!("on {machine}"));
    }
    lines
}

/// Why an agent waits on the person: its note, else what it is doing.
pub fn reason(node: &Node) -> Option<&str> {
    let note = node.note.as_ref().map(|note| note.text.as_str());
    note.filter(|text| !text.is_empty())
        .or(node.doing.as_deref())
}

/// What an agent is and where, under its name on its waiting card.
pub fn place_of(node: &Node) -> String {
    let mut place = kind(node);
    if let Some(repo) = &node.repo {
        place.push_str(&format!(" in {}", short(repo)));
    }
    place
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
        let mut room = Agents::default();
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
        let mut room = Agents::default();
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
        let mut room = Agents::default();
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
        let mut room = Agents::default();
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
        let mut room = Agents::default();
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
            for placed in &island.placed {
                let reach = if placed.depth == 0 { AGENT } else { CHILD };
                let inside = placed.at.x - reach >= 0.0
                    && placed.at.y - reach >= 0.0
                    && placed.at.x + reach <= island.size.x
                    && placed.at.y + reach <= island.size.y;
                assert!(inside, "{placed:?} outside {:?}", island.size);
            }
            assert!(island.top.x >= 0.0 && island.top.x + island.size.x <= width);
        }
        let [alpha, beta] = &map.islands[..] else {
            panic!("two islands");
        };
        let apart = alpha.top.x + alpha.size.x <= beta.top.x
            || beta.top.x + beta.size.x <= alpha.top.x
            || alpha.top.y + alpha.size.y <= beta.top.y
            || beta.top.y + beta.size.y <= alpha.top.y;
        assert!(apart);
        let bottom = (alpha.top.y + alpha.size.y).max(beta.top.y + beta.size.y);
        assert!(map.height >= bottom);
    }

    #[test]
    fn the_map_shrinks_to_fit_the_screen_while_it_stays_readable() {
        let mut room = Agents::default();
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

    #[test]
    fn a_repository_keeps_its_colour() {
        assert_eq!(hue("/src/alpha", true), hue("/src/alpha", true));
        assert_ne!(hue("/src/alpha", true), hue("/src/beta", true));
    }
}
