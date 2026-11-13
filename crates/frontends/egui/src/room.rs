//! The control room: the agents waiting on a person as tiles, then a map of every agent the
//! daemon's collectors see, one island per repository with what each agent started around it.

use std::collections::BTreeMap;
use std::f32::consts::PI;
use std::time::Duration;

use claudo_protocol::{
    Action, Checks, Command, Node, NodeId, NodeKind, NodeState, Provider, QuickPrompt, RoomEvent,
};
use claudo_room::agents::{
    AGENT, Agents, BADGE_ROW, BADGE_SIZE, CAPTION_REACH, CHILD, Island, LEGEND, PAD, STARTED_GAP,
    STARTED_ROW, STARTED_SIZE, Shown, Steady, Tint, V, agent_colour, ago, badges, checks,
    ci_counts, codex, dibs_job, hue, kind, legend_kinds, made_by, open_prs, phase, pull_requests,
    reason, short, started, started_rows, started_word, state, state_tint, thousands,
};
use egui::epaint::QuadraticBezierShape;
use egui::{Align2, Color32, FontId, Pos2, Rect, RichText, Sense, Stroke, Vec2, vec2};
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};

#[derive(Default)]
pub struct Room {
    /// Every node, each state timed by `Context::time`.
    agents: Agents,
    /// Answers being typed to the agents waiting on the person.
    drafts: BTreeMap<NodeId, String>,
    /// The messages the person sends agents often, as buttons beside the answer box.
    pub quick_prompts: Vec<QuickPrompt>,
    /// The last action that failed, and why.
    failed: Option<(NodeId, String)>,
    sent: Vec<Command>,
    /// The node whose details are open beside the map.
    selected: Option<NodeId>,
    markdown: CommonMarkCache,
    /// The open pull requests are listed under the summary.
    prs_open: bool,
    steady: Steady,
}

impl Room {
    pub fn apply(&mut self, event: RoomEvent) {
        if let Some(failed) = self.agents.apply(event) {
            self.failed = Some(failed);
        }
    }

    /// What the person asked of the agents since the last call, for the backend.
    pub fn take_sent(&mut self) -> Vec<Command> {
        std::mem::take(&mut self.sent)
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        let now = ui.ctx().time();
        self.agents.stamp(now);
        let moving = self
            .agents
            .nodes
            .values()
            .any(|node| matches!(node.state, Some(NodeState::Working | NodeState::Blocked)));
        if moving {
            ui.ctx().request_repaint_after(Duration::from_millis(33));
        }
        self.selected = self
            .selected
            .take()
            .filter(|id| self.agents.nodes.contains_key(id));
        if let Some(id) = self.selected.clone() {
            // Leaves the map at least half its tile, a narrow one too.
            let room = ui.available_width() / 2.0;
            egui::Panel::right("room-details")
                .default_size(room.min(440.0))
                .size_range(room.min(200.0)..=room)
                .resizable(true)
                .show(ui, |ui| self.details(ui, &id, now));
        }
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
                    ui.add_space(8.0);
                    ui.weak("?").on_hover_ui(legend);
                    self.pull_request_count(ui);
                });
                if self.prs_open {
                    self.pull_requests(ui);
                }
                self.failure(ui);
                ui.add_space(4.0);
                self.waiting_lines(ui, width, now);
                let left = screen - (ui.cursor().top() - top);
                let picked = self.map(ui, width, left, now);
                match picked {
                    Some(Picked::Node(id)) => {
                        self.selected = (self.selected.as_ref() != Some(&id)).then_some(id);
                    }
                    Some(Picked::Focus(node)) => self.sent.push(Command::Act {
                        node,
                        action: Action::Focus,
                        request: 0,
                    }),
                    None => {}
                }
            });
    }

    /// How many pull requests the agents here have open and how their CI stands, which
    /// opens the list of them.
    fn pull_request_count(&mut self, ui: &mut egui::Ui) {
        let open = self.agents.pull_requests();
        if open.is_empty() {
            return;
        }
        ui.weak("·");
        let arrow = if self.prs_open { "▾" } else { "▸" };
        let toggle = ui.selectable_label(
            self.prs_open,
            format!("{} {arrow}", pull_requests(open.len())),
        );
        for (state, count) in ci_counts(&open) {
            let (words, shade) = checks(state);
            ui.label(
                RichText::new(format!("{count} {words}"))
                    .color(rgb(shade))
                    .small(),
            );
        }
        if toggle.clicked() {
            self.prs_open = !self.prs_open;
        }
    }

    /// Each open pull request on a line: its number, its title, its CI and the agent that
    /// opened it.
    fn pull_requests(&mut self, ui: &mut egui::Ui) {
        let open = self.agents.pull_requests();
        let mut sent = Vec::new();
        egui::Grid::new("pull requests")
            .spacing(vec2(14.0, 3.0))
            .striped(true)
            .show(ui, |ui| {
                for (node, pr) in &open {
                    let (name, title) = pr.about.split_once(": ").unwrap_or((&pr.about, ""));
                    pull_request(ui, name.to_string(), pr, &node.id, &mut sent);
                    ui.add(egui::Label::new(RichText::new(title).weak()).truncate());
                    ci(ui, pr.checks);
                    let agent = node.name.as_deref().unwrap_or("unnamed");
                    let label =
                        egui::Label::new(RichText::new(agent).color(rgb(agent_colour(agent))))
                            .sense(Sense::click());
                    let response = ui
                        .add(label)
                        .on_hover_text("Click for details, shift-click for its terminal");
                    if response.clicked() {
                        match ui.input(|input| input.modifiers.shift) {
                            true => sent.push(focus(&node.id)),
                            false => self.selected = Some(node.id.clone()),
                        }
                    }
                    ui.end_row();
                }
            });
        self.sent.extend(sent);
    }

    fn failure(&mut self, ui: &mut egui::Ui) {
        let Some((node, detail)) = &self.failed else {
            return;
        };
        let name = self
            .agents
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
            self.agents
                .nodes
                .values()
                .filter(|node| node.state == Some(state))
                .count()
        };
        let agents = self
            .agents
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
        if self.agents.waiting().next().is_none() {
            ui.add_space(10.0);
            dot(ui, color(ui.visuals(), Some(NodeState::Working)));
            ui.label(RichText::new("nobody is waiting on you").size(15.0));
        }
    }

    /// The selected node: what the hover says, a box to answer an agent, its digest when it
    /// is a finished review, and the runs an agent started with theirs.
    fn details(&mut self, ui: &mut egui::Ui, id: &str, now: f64) {
        let Some(node) = self.agents.nodes.get(id).cloned() else {
            return;
        };
        let mut runs: Vec<Node> = self
            .agents
            .nodes
            .values()
            .filter(|run| run.parent.as_deref() == Some(id))
            .filter(|run| matches!(run.kind, Some(NodeKind::Run(_))))
            .cloned()
            .collect();
        // Run folders begin with when they started.
        runs.sort_by(|a, b| b.id.cmp(&a.id));
        let agent = self
            .agents
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
                about(ui, &node, self.agents.held(id, now));
                if let Some((parent, name)) = agent {
                    let name = name.unwrap_or_else(|| "its agent".to_string());
                    if ui.link(format!("Started by {name}")).clicked() {
                        self.selected = Some(parent);
                    }
                }
                for pr in open_prs(&node) {
                    ui.horizontal(|ui| {
                        let label = format!("PR #{}", pr.number);
                        pull_request(ui, label, &pr, &node.id, &mut self.sent);
                        ci(ui, pr.checks);
                    });
                }
                if matches!(node.kind, Some(NodeKind::Agent(_))) {
                    ui.add_space(6.0);
                    let draft = self.drafts.entry(node.id.clone()).or_default();
                    let width = ui.available_width() - 60.0;
                    if let Some(action) =
                        answer(ui, "details", &node, draft, width, &self.quick_prompts)
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

    /// A line for each agent waiting on the person: its name, why, for how long, a box to
    /// answer it, and its terminal. The whole reason shows on hover and in its details.
    fn waiting_lines(&mut self, ui: &mut egui::Ui, width: f32, now: f64) {
        let waiting: Vec<(Node, Option<f64>)> = self
            .agents
            .waiting()
            .map(|node| (node.clone(), self.agents.held(&node.id, now)))
            .collect();
        let selected = self.selected.clone();
        self.drafts.retain(|id, _| {
            selected.as_ref() == Some(id) || waiting.iter().any(|(node, _)| node.id == *id)
        });
        let warn = ui.visuals().warn_fg_color;
        for (node, held) in waiting {
            egui::Frame::new()
                .fill(warn.gamma_multiply(0.08))
                .stroke(Stroke::new(1.0, warn.gamma_multiply(0.5)))
                .corner_radius(6.0)
                .inner_margin(egui::Margin::symmetric(10, 4))
                .show(ui, |ui| {
                    ui.set_width(width - 20.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("!").strong().color(warn));
                        let name = node.name.as_deref().unwrap_or("unnamed");
                        let label = egui::Label::new(RichText::new(name).strong().color(warn))
                            .sense(Sense::click());
                        let response = ui
                            .add(label)
                            .on_hover_text("Click for details, shift-click for its terminal");
                        if response.clicked() {
                            match ui.input(|input| input.modifiers.shift) {
                                true => self.sent.push(focus(&node.id)),
                                false => self.selected = Some(node.id.clone()),
                            }
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .small_button("terminal")
                                .on_hover_text("Bring its terminal forward")
                                .clicked()
                            {
                                self.sent.push(Command::Act {
                                    node: node.id.clone(),
                                    action: Action::Focus,
                                    request: 0,
                                });
                            }
                            let draft = self.drafts.entry(node.id.clone()).or_default();
                            let send = ui.small_button("Send").clicked();
                            let answer = ui.add(
                                egui::TextEdit::singleline(draft)
                                    .id(egui::Id::new(("answer", "line", &node.id)))
                                    .hint_text("Answer")
                                    .desired_width(220.0),
                            );
                            let entered = answer.lost_focus()
                                && ui.input(|input| input.key_pressed(egui::Key::Enter));
                            if (send || entered) && !draft.trim().is_empty() {
                                let text = std::mem::take(draft).trim().to_string();
                                self.sent.push(Command::Act {
                                    node: node.id.clone(),
                                    action: Action::Prompt(text),
                                    request: 0,
                                });
                            }
                            if let Some(held) = held {
                                ui.weak(ago(held));
                            }
                            let why = reason(&node).unwrap_or_default();
                            ui.add(egui::Label::new(RichText::new(why).weak()).truncate())
                                .on_hover_text(why);
                        });
                    });
                });
            ui.add_space(4.0);
        }
    }

    /// `screen` is the height left on screen, which the map fits into while it stays readable.
    /// Clicking a node opens its details, shift-clicking one brings its agent's terminal forward,
    /// and a pull request's number opens it.
    fn map(&self, ui: &mut egui::Ui, width: f32, screen: f32, now: f64) -> Option<Picked> {
        let clusters = self.agents.clusters();
        let (map, scale) = self.steady.lay_out(&clusters, width, screen);
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

        let mut drawn: Vec<(&Node, Pos2, f32, &str)> = Vec::new();
        let mut links: Vec<(Rect, &str, &str)> = Vec::new();
        for island in &map.islands {
            let key = island.repo.unwrap_or("");
            let corner = glide(&format!("island:{key}"), spot(island.top));
            shore(&painter, &visuals, island, corner, scale);
            let at: BTreeMap<&str, Pos2> = island
                .placed
                .iter()
                .map(|p| {
                    let here = glide(&p.node.id, spot(island.top) + spot(p.at));
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
                        let label = island.labelled(placed);
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
                    let started: Vec<(String, Color32)> = started(children)
                        .into_iter()
                        .map(|(text, tint)| (text, rgb(tint)))
                        .collect();
                    let agent = placed.node.id.as_str();
                    links.extend(
                        caption(&painter, &visuals, placed.node, &started, here, scale)
                            .into_iter()
                            .map(|(rect, link)| (rect, link, agent)),
                    );
                }
                drawn.push((placed.node, here, radius, island.agent_of(&placed.node.id)));
            }
        }

        let pointer = response.hover_pos();
        let link =
            pointer.and_then(|pointer| links.iter().find(|(rect, ..)| rect.contains(pointer)));
        if let Some(&(rect, link, agent)) = link {
            painter.hline(
                rect.x_range(),
                rect.bottom() - 1.0,
                Stroke::new(1.0, visuals.text_color()),
            );
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            let shift = ui.input(|input| input.modifiers.shift);
            let picked = response.clicked().then(|| match shift {
                true => Some(Picked::Focus(agent.to_string())),
                false => {
                    ui.ctx().open_url(egui::OpenUrl::new_tab(link));
                    None
                }
            });
            response.on_hover_text_at_pointer(format!(
                "{link}\nShift-click to bring forward the agent that opened it"
            ));
            return picked.flatten();
        }
        let hovered = pointer.and_then(|pointer| {
            drawn
                .iter()
                .filter(|(_, at, radius, _)| at.distance(pointer) <= radius + 6.0)
                .min_by(|a, b| a.1.distance(pointer).total_cmp(&b.1.distance(pointer)))
        });
        let &(node, at, radius, owner) = hovered?;
        painter.circle_stroke(
            at,
            radius + 4.0,
            Stroke::new(1.5, visuals.strong_text_color()),
        );
        let agent = matches!(node.kind, Some(NodeKind::Agent(_)));
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        let shift = ui.input(|input| input.modifiers.shift);
        let picked = response.clicked().then(|| match shift {
            true => Picked::Focus(owner.to_string()),
            false => Picked::Node(node.id.clone()),
        });
        let held = self.agents.held(&node.id, now);
        response.on_hover_ui_at_pointer(|ui| {
            about(ui, node, held);
            ui.weak(match agent {
                true => "Click for details, shift-click to bring its terminal forward",
                false => "Click for details, shift-click to bring its agent's terminal forward",
            });
        });
        picked
    }
}

fn focus(agent: &str) -> Command {
    Command::Act {
        node: agent.to_string(),
        action: Action::Focus,
        request: 0,
    }
}

/// A pull request's link: opens it, or with Shift brings forward the agent that opened it.
fn pull_request(
    ui: &mut egui::Ui,
    label: String,
    pr: &Shown,
    agent: &str,
    sent: &mut Vec<Command>,
) {
    let hint = format!(
        "{}\nShift-click to bring forward the agent that opened it",
        pr.about
    );
    if ui.link(label).on_hover_text(hint).clicked() {
        match ui.input(|input| input.modifiers.shift) {
            true => sent.push(focus(agent)),
            false => ui.ctx().open_url(egui::OpenUrl::new_tab(pr.link)),
        }
    }
}

/// What a click on the map asked for.
enum Picked {
    Node(NodeId),
    Focus(NodeId),
}

fn shore(
    painter: &egui::Painter,
    visuals: &egui::Visuals,
    island: &Island,
    corner: Pos2,
    scale: f32,
) {
    let rect = egui::Rect::from_min_size(corner, spot(island.size) * scale);
    let round = 16.0 * scale;
    let sea = rgb(hue(island.repo.unwrap_or(""), visuals.dark_mode));
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
    let tint = rgb(made_by(node));
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
    if codex(node) {
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

/// Under an agent: its name, what it is doing, its badges, and what it started. Gives back
/// where its pull requests' numbers are written, and what each opens.
fn caption<'a>(
    painter: &egui::Painter,
    visuals: &egui::Visuals,
    node: &'a Node,
    started: &[(String, Color32)],
    at: Pos2,
    scale: f32,
) -> Vec<(Rect, &'a str)> {
    let top = at + vec2(0.0, (AGENT + 6.0) * scale);
    let name = node.name.as_deref().unwrap_or("unnamed");
    // Written as dibs writes the agent's jobs, so they match beside the machines.
    let colour = rgb(agent_colour(name));
    let name = lines(painter, name, 13.0 * scale, colour, 1, scale);
    let below = top + vec2(0.0, name.size().y + 2.0 * scale);
    painter.galley(top, name, colour);
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
    let rows = badges(node);
    let mut links = Vec::new();
    for (i, parts) in rows.iter().enumerate() {
        let coloured: Vec<(String, Color32)> = parts
            .iter()
            .map(|part| (part.text.clone(), tint(visuals, part.tint)))
            .collect();
        let galley = row(painter, &coloured, BADGE_SIZE * scale, scale);
        let at = below + vec2(0.0, (15.0 + i as f32 * BADGE_ROW) * scale);
        if let Some(placed) = galley.rows.first() {
            // One glyph a character, on the one row a badge line is cut to.
            let mut first = 0;
            for part in parts {
                let count = part.text.chars().count();
                let glyphs = placed.row.glyphs.get(first..first + count);
                if let (Some(link), Some(glyphs)) = (part.link, glyphs) {
                    let rect = glyphs
                        .iter()
                        .map(|glyph| glyph.logical_rect())
                        .fold(Rect::NOTHING, Rect::union);
                    links.push((rect.translate(at.to_vec2() + placed.pos.to_vec2()), link));
                }
                first += count;
            }
        }
        painter.galley(at, galley, visuals.text_color());
    }
    let more = rows.len().saturating_sub(1) as f32 * BADGE_ROW;
    let dot = (STARTED_GAP.to_string(), visuals.weak_text_color());
    for (i, started) in started_rows(started.to_vec()).into_iter().enumerate() {
        let parts: Vec<(String, Color32)> = started
            .into_iter()
            .enumerate()
            .flat_map(|(i, part)| (i > 0).then(|| dot.clone()).into_iter().chain([part]))
            .collect();
        let started = row(painter, &parts, STARTED_SIZE * scale, scale);
        painter.galley(
            below + vec2(0.0, (29.0 + more + i as f32 * STARTED_ROW) * scale),
            started,
            visuals.text_color(),
        );
    }
    links
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

fn ci(ui: &mut egui::Ui, state: Option<Checks>) {
    if let Some((words, shade)) = state.map(checks) {
        ui.label(RichText::new(words).color(rgb(shade)).small());
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
    let tint = rgb(made_by(node));
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
        let word = started_word(node);
        painter.text(
            at - vec2(0.0, radius + 6.0 * scale),
            Align2::CENTER_BOTTOM,
            word,
            FontId::proportional(9.5 * scale),
            tint,
        );
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

/// A box to write to the agent in, and a link to its terminal. `place` keeps the tile's box
/// and the details' apart when both show one agent.
fn answer(
    ui: &mut egui::Ui,
    place: &str,
    node: &Node,
    draft: &mut String,
    width: f32,
    quick_prompts: &[QuickPrompt],
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
    ui.horizontal_wrapped(|ui| {
        for prompt in quick_prompts {
            if ui.button(&prompt.label).on_hover_text(&prompt.text).clicked() {
                action = Some(Action::Prompt(prompt.text.clone()));
            }
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
    for shown in LEGEND {
        dot(ui, color(ui.visuals(), Some(shown)));
        ui.weak(state(Some(shown)));
        ui.add_space(6.0);
    }
    ui.add_space(10.0);
    ui.weak("round: Claude, square: Codex, diamond: dibs job.");
    ui.add_space(6.0);
    for (word, shade) in legend_kinds() {
        ui.label(RichText::new(word).color(rgb(shade)));
    }
    ui.weak("around an agent: filled and turning while it runs, hollow while it waits.");
    ui.add_space(10.0);
    ui.weak("click one for details");
}

fn dot(ui: &mut egui::Ui, tint: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(12.0, 12.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 5.0, tint);
}

fn color(visuals: &egui::Visuals, state: Option<NodeState>) -> Color32 {
    tint(visuals, state_tint(state))
}

fn tint(visuals: &egui::Visuals, tint: Tint) -> Color32 {
    match tint {
        Tint::Text => visuals.text_color(),
        Tint::Weak => visuals.weak_text_color(),
        Tint::Warn => visuals.warn_fg_color,
        Tint::Error => visuals.error_fg_color,
        Tint::Rgb(rgb) => self::rgb(rgb),
    }
}

fn rgb([r, g, b]: [u8; 3]) -> Color32 {
    Color32::from_rgb(r, g, b)
}

/// A map position as egui places it.
fn spot(position: V) -> Vec2 {
    vec2(position.x, position.y)
}
