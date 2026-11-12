//! The control room, as the daemon serves it on its room socket: the agents waiting on the
//! person as cards and every agent on a map, beside each dibs machine's state, what its CPU,
//! memory, disks and cards are doing, and its jobs, with dibstop's actions, which the daemon
//! runs. What is shown and how is worked out in `claudo_room`, as for the web page.

use std::collections::HashMap;
use std::time::Instant;

use claudo_protocol::{
    Action as Act, DIBS_POLL, DibsMachine, DibsWatch, Node, NodeId, NodeKind, NodeState,
};
use claudo_room::agents::{
    self as room_agents, Agents, LEGEND, Map, ago, checks, fit, legend_kinds, open_prs, reason,
    state, state_tint,
};
use claudo_room::machines::{
    Arrangement, Cards, History, KEPT, Show, card_name, filled, gib, heat, held_by, level, memory,
    percent, running_on,
};
use claudo_transport::connect_room;
use cosmic::iced::widget::canvas::{self, Frame, Path};
use cosmic::iced::{Rectangle, mouse};
use dibstop_view::action::{Action, Call};
use dibstop_view::dibs_format::status::{GpuMetrics, Status};
use dibstop_view::dibs_format::{Label, Mode, Span};
use dibstop_view::item::{Item, Progress as _};
use dibstop_view::text::{Tone, cores, plain, state_words};

use super::*;

/// The narrowest a cell gets when the columns fit the width.
const NARROWEST: f32 = 420.0;
const GAP: f32 = 8.0;
const CPU_GRAPH: f32 = 40.0;
const BAR: f32 = 16.0;
const CARD: f32 = 210.0;
const CARD_GRAPH: f32 = 30.0;
const ROW_BAR: f32 = 90.0;
const ARRANGEMENT: &str = "room-arrangement";

#[derive(Default)]
pub struct Room {
    commands: Option<Commands>,
    /// A window has shown the room, so the app keeps a link to it.
    pub wanted: bool,
    agents: Agents,
    /// When the room was first heard, the zero of its clock.
    clock: Option<Instant>,
    /// The last action on an agent that failed, and why.
    failed: Option<(NodeId, String)>,
    /// Answers being typed to agents.
    drafts: HashMap<NodeId, String>,
    /// The node whose details are open beside the map.
    picked: Option<NodeId>,
    machines: Vec<DibsMachine>,
    watch: Option<DibsWatch>,
    histories: HashMap<String, History>,
    arrangement: Arrangement,
    arranging: bool,
    /// The job whose details are open on each machine, by pid.
    selected: HashMap<String, u32>,
    confirm: Option<(String, u32, Label)>,
    /// What the last action printed.
    shown: Option<(String, String)>,
    busy: Option<String>,
    /// The poll the slider is set to while it moves.
    poll: Option<u64>,
    lost: Option<String>,
    /// The room has said something since the link was made.
    answered: bool,
    /// The node under the pointer on the map, and where it is drawn.
    hover: Option<(NodeId, f32, f32)>,
}

#[derive(Debug, Clone)]
pub enum RoomMessage {
    Connected(Commands),
    Events(Vec<ServerEvent>),
    Select(String, u32),
    Ask(String, Call, String),
    Kill(String, u32, Label),
    Killing(bool),
    CloseShown,
    Arrange,
    Shown(String, bool),
    Metrics(String, bool),
    Swap(usize, usize),
    Columns(Option<usize>),
    Show(Show),
    Poll(u64),
    PollDone,
    /// Time passed while something on the map moves.
    Tick,
    Draft(NodeId, String),
    Answer(NodeId),
    Focus(NodeId),
    Pick(NodeId),
    Unpick,
    Dismiss,
    Hover(Option<(NodeId, f32, f32)>),
}

impl Room {
    pub fn new() -> Self {
        let arrangement = UiState::store()
            .and_then(|store| store.get(&remote::scoped(ARRANGEMENT)).ok())
            .unwrap_or_default();
        Self {
            arrangement,
            ..Self::default()
        }
    }

    fn send(&self, command: Command) {
        if let Some(commands) = &self.commands {
            commands.send(command);
        }
    }

    fn arranged(&self) {
        if let Some(store) = UiState::store() {
            let _ = store.set(&remote::scoped(ARRANGEMENT), &self.arrangement);
        }
    }

    pub fn update(&mut self, message: RoomMessage) {
        match message {
            RoomMessage::Connected(commands) => self.commands = Some(commands),
            RoomMessage::Events(events) => {
                self.answered = true;
                for event in events {
                    self.heard(event);
                }
            }
            RoomMessage::Select(machine, pid) => {
                if self.selected.get(&machine) == Some(&pid) {
                    self.selected.remove(&machine);
                } else {
                    self.selected.insert(machine, pid);
                }
            }
            RoomMessage::Ask(machine, call, label) => {
                self.busy = Some(Action::new(machine.clone(), call, &label).doing);
                self.send(Command::Dibs {
                    machine,
                    call,
                    label,
                });
            }
            RoomMessage::Kill(machine, pid, label) => self.confirm = Some((machine, pid, label)),
            RoomMessage::Killing(yes) => {
                if let Some((machine, pid, label)) = self.confirm.take().filter(|_| yes) {
                    self.update(RoomMessage::Ask(
                        machine,
                        Call::Kill { pid },
                        label.as_str().to_string(),
                    ));
                }
            }
            RoomMessage::CloseShown => self.shown = None,
            RoomMessage::Arrange => self.arranging = !self.arranging,
            RoomMessage::Shown(name, shown) => {
                self.arrangement.set_shown(&name, shown);
                self.arranged();
            }
            RoomMessage::Metrics(name, read) => {
                if let Some(watch) = &self.watch {
                    let mut next = watch.clone();
                    next.no_metrics.retain(|other| *other != name);
                    if !read {
                        next.no_metrics.push(name);
                    }
                    self.send(Command::DibsWatch(next));
                }
            }
            RoomMessage::Swap(from, to) => {
                self.arrangement.swap(self.names(), from, to);
                self.arranged();
            }
            RoomMessage::Columns(columns) => {
                self.arrangement.columns = columns;
                self.arranged();
            }
            RoomMessage::Show(show) => {
                self.arrangement.show = show;
                self.arranged();
            }
            RoomMessage::Tick => {
                let now = self.now();
                self.agents.stamp(now);
            }
            RoomMessage::Draft(id, text) => {
                self.drafts.insert(id, text);
            }
            RoomMessage::Answer(id) => {
                let text = self.drafts.remove(&id).unwrap_or_default();
                if !text.trim().is_empty() {
                    self.act(id, Act::Prompt(text.trim().to_string()));
                }
            }
            RoomMessage::Focus(id) => self.act(id, Act::Focus),
            RoomMessage::Pick(id) => {
                self.picked = (self.picked.as_ref() != Some(&id)).then_some(id);
            }
            RoomMessage::Unpick => self.picked = None,
            RoomMessage::Hover(hover) => self.hover = hover,
            RoomMessage::Dismiss => self.failed = None,
            RoomMessage::Poll(seconds) => self.poll = Some(seconds),
            // Each change makes the daemon look again at once, so only where a drag ends is sent.
            RoomMessage::PollDone => {
                if let (Some(poll), Some(watch)) = (self.poll.take(), &self.watch)
                    && poll != watch.poll
                {
                    self.send(Command::DibsWatch(DibsWatch {
                        poll,
                        ..watch.clone()
                    }));
                }
            }
        }
    }

    fn heard(&mut self, event: ServerEvent) {
        match event {
            ServerEvent::Room(event) => {
                if let Some(failed) = self.agents.apply(event) {
                    self.failed = Some(failed);
                }
                let now = self.now();
                self.agents.stamp(now);
                if self
                    .picked
                    .as_ref()
                    .is_some_and(|id| !self.agents.nodes.contains_key(id))
                {
                    self.picked = None;
                }
            }
            ServerEvent::Dibs(machines) => {
                for machine in &machines {
                    if let Some(status) = &machine.status {
                        self.histories
                            .entry(machine.name.clone())
                            .or_default()
                            .heard(status);
                    }
                }
                self.machines = machines;
            }
            ServerEvent::DibsWatch(watch) => self.watch = Some(watch),
            ServerEvent::DibsAnswer { title, body } => {
                self.busy = None;
                self.shown = Some((title, body));
            }
            ServerEvent::Connection { connected, detail } => {
                self.lost = (!connected).then_some(detail);
            }
            _ => {}
        }
    }

    fn act(&self, node: NodeId, action: Act) {
        self.send(Command::Act {
            node,
            action,
            request: 0,
        });
    }

    /// Seconds on the room's clock.
    fn now(&mut self) -> f64 {
        self.clock
            .get_or_insert_with(Instant::now)
            .elapsed()
            .as_secs_f64()
    }

    fn time(&self) -> f64 {
        self.clock
            .map_or(0.0, |clock| clock.elapsed().as_secs_f64())
    }

    /// Something on the map moves, so it is drawn again while it shows.
    pub fn moving(&self) -> bool {
        self.agents.moving()
    }

    fn names(&self) -> Vec<String> {
        self.machines
            .iter()
            .map(|machine| machine.name.clone())
            .collect()
    }

    /// The room page, `width` by `height`: the agents, their details when one is picked,
    /// and the machines.
    pub fn view(&self, zoom: f32, width: f32, height: f32) -> Element<'_, Message> {
        if !self.answered || (self.lost.is_some() && self.agents.nodes.is_empty()) {
            let words = match &self.lost {
                Some(lost) => format!(
                    "Cannot reach the daemon's room ({lost}); trying again every few seconds."
                ),
                None => "Connecting to the daemon's room…".to_string(),
            };
            return widget::container(widget::text(words).class(dim()))
                .center(Length::Fill)
                .into();
        }
        let details = self
            .picked
            .as_ref()
            .and_then(|id| self.agents.nodes.get(id));
        let aside = if details.is_some() { 380.0 * zoom } else { 0.0 };
        let agents = (width - aside) * 0.6;
        let mut columns: Vec<Element<Message>> = vec![self.agents_view(zoom, agents, height)];
        if let Some(node) = details {
            columns.push(widget::divider::vertical::default().into());
            columns.push(self.details(node, aside));
        }
        columns.push(widget::divider::vertical::default().into());
        columns.push(self.machines_view(zoom, width - aside - agents));
        widget::Row::with_children(columns)
            .height(Length::Fill)
            .into()
    }

    fn machines_view(&self, zoom: f32, width: f32) -> Element<'_, Message> {
        let mut page: Vec<Element<Message>> = Vec::new();
        let mut bar: Vec<Element<Message>> = vec![widget::text::title3("Machines").into()];
        if let Some(busy) = &self.busy {
            bar.push(widget::text(format!("{busy}…")).class(dim()).into());
        }
        bar.push(widget::space::horizontal().into());
        bar.push(
            widget::button::text(if self.arranging { "Done" } else { "Arrange" })
                .on_press(Message::Room(RoomMessage::Arrange))
                .into(),
        );
        page.push(
            widget::Row::with_children(bar)
                .spacing(12)
                .align_y(Alignment::Center)
                .into(),
        );
        if let Some(lost) = &self.lost {
            page.push(
                widget::text(format!("Lost the daemon's room ({lost}); trying again."))
                    .class(destructive())
                    .into(),
            );
        }
        if let Some(confirm) = self.confirming() {
            page.push(confirm);
        }
        if let Some((title, body)) = &self.shown {
            page.push(answer(title, body));
        }
        if self.arranging {
            page.push(self.arrange());
        }
        page.push(self.grid(zoom, width - 32.0));
        widget::scrollable(
            widget::Column::with_children(page)
                .spacing(12)
                .padding([16, 16]),
        )
        .width(Length::Fixed(width))
        .height(Length::Fill)
        .into()
    }

    /// How many agents and how they stand with the open pull requests on one line, a line
    /// for each agent waiting on the person, and the map in whatever height is left.
    fn agents_view(&self, zoom: f32, width: f32, height: f32) -> Element<'_, Message> {
        let mut page: Vec<Element<Message>> = Vec::new();
        let mut summary: Vec<Element<Message>> =
            vec![widget::text::title4(format!("{} agents", self.agents.agents())).into()];
        for (shown, word) in [
            (NodeState::Working, "working"),
            (NodeState::Blocked, "need you"),
            (NodeState::Failed, "failed"),
        ] {
            let n = self.agents.count(shown);
            if n > 0 {
                summary.push(dot(Some(shown)));
                summary.push(widget::text(format!("{n} {word}")).into());
            }
        }
        summary.push(
            widget::tooltip(
                widget::text("?").class(dim()),
                legend(),
                widget::tooltip::Position::Bottom,
            )
            .into(),
        );
        for (node, pr) in self.agents.pull_requests() {
            summary.push(widget::text("·").class(dim()).into());
            let repo = pr.about.split(':').next().unwrap_or(pr.number).to_string();
            summary.push(link(repo, pr.link));
            if let Some((words, tint)) = pr.checks.map(checks) {
                summary.push(
                    widget::text(words)
                        .size(11.0)
                        .class(cosmic::theme::Text::Color(super::room_map::rgb(tint)))
                        .into(),
                );
            }
            summary.push(
                widget::text(node.name.clone().unwrap_or_else(|| "unnamed".into()))
                    .size(12.0)
                    .class(dim())
                    .into(),
            );
        }
        page.push(
            widget::flex_row(summary)
                .spacing(8)
                .align_items(Alignment::Center)
                .into(),
        );
        let mut used = 40.0 * zoom;
        if let Some((node, detail)) = &self.failed {
            let name = self
                .agents
                .nodes
                .get(node)
                .and_then(|node| node.name.clone())
                .unwrap_or_else(|| node.clone());
            page.push(
                widget::Row::with_children(vec![
                    widget::text(format!("Could not reach {name}: {detail}"))
                        .class(destructive())
                        .into(),
                    widget::button::text("Dismiss")
                        .on_press(Message::Room(RoomMessage::Dismiss))
                        .into(),
                ])
                .spacing(8)
                .align_y(Alignment::Center)
                .into(),
            );
            used += 40.0 * zoom;
        }
        for node in self.agents.waiting() {
            page.push(self.waiting_line(node, zoom));
            used += 46.0 * zoom;
        }
        let width = width - 24.0;
        let clusters = self.agents.clusters();
        // The header bar, this column's padding, and what is above the map.
        let screen = (height - 64.0 - 24.0 - used).max(160.0);
        let scale = fit(&clusters, width / zoom, screen / zoom) * zoom;
        let map = Map::lay_out(&clusters, width / scale);
        let tall = map.height * scale;
        let canvas: Element<Message> = canvas::Canvas::new(super::room_map::MapView {
            map,
            scale,
            now: self.time(),
            selected: self.picked.as_deref(),
        })
        .width(Length::Fixed(width))
        .height(Length::Fixed(tall))
        .into();
        let mut layers = vec![canvas];
        if let Some(card) = self.hover_card(width, tall) {
            layers.push(card);
        }
        page.push(cosmic::iced::widget::Stack::with_children(layers).into());
        widget::scrollable(
            widget::Column::with_children(page)
                .spacing(6)
                .padding([12, 12]),
        )
        .width(Length::Fixed(width + 24.0))
        .height(Length::Fill)
        .into()
    }

    /// What the hover says about the node under the pointer, in a card beside it.
    fn hover_card(&self, width: f32, tall: f32) -> Option<Element<'_, Message>> {
        const CARD: f32 = 420.0;
        let (id, x, y) = self.hover.as_ref()?;
        let node = self.agents.nodes.get(id)?;
        let held = self.agents.held(id, self.time());
        let mut lines: Vec<Element<Message>> = room_agents::about(node, held)
            .into_iter()
            .enumerate()
            .map(|(i, line)| match i {
                0 => widget::text(line).font(cosmic::font::bold()).into(),
                _ => widget::text(line).size(13.0).into(),
            })
            .collect();
        let hint = match node.kind {
            Some(NodeKind::Agent(_)) => {
                "Click for details, shift-click to bring its terminal forward"
            }
            _ => "Click for details, shift-click to bring its agent's terminal forward",
        };
        lines.push(widget::text(hint).size(12.0).class(dim()).into());
        let left = (x + 16.0).min(width - CARD).max(0.0);
        // Below the node, or above it when it is low on the map.
        let top = if *y > tall * 0.6 {
            (y - 220.0).max(0.0)
        } else {
            y + 16.0
        };
        let card = widget::container(widget::Column::with_children(lines).spacing(2))
            .padding([8, 12])
            .max_width(CARD)
            .class(cosmic::theme::Container::custom(|theme| {
                let cosmic = theme.cosmic();
                container::Style {
                    background: Some(Color::from(cosmic.bg_component_color()).into()),
                    border: cosmic::iced::Border {
                        color: Color::from(cosmic.bg_divider()),
                        width: 1.0,
                        radius: 6.0.into(),
                    },
                    shadow: cosmic::iced::Shadow {
                        color: Color::from_rgba(0.0, 0.0, 0.0, 0.4),
                        offset: cosmic::iced::Vector::new(0.0, 4.0),
                        blur_radius: 12.0,
                    },
                    ..Default::default()
                }
            }));
        Some(
            widget::container(card)
                .padding(Padding {
                    top,
                    left,
                    ..Padding::ZERO
                })
                .into(),
        )
    }

    /// An agent waiting on the person, on one line: its name, why, for how long, a box to
    /// answer it, and its terminal. The whole reason shows on hover and in its details.
    fn waiting_line(&self, node: &Node, zoom: f32) -> Element<'_, Message> {
        let id = node.id.clone();
        let draft = self.drafts.get(&node.id).map_or("", String::as_str);
        let name = node.name.clone().unwrap_or_else(|| "unnamed".into());
        let colour = super::room_map::rgb(room_agents::agent_colour(&name));
        let why = reason(node).unwrap_or_default().to_string();
        let mut parts: Vec<Element<Message>> = vec![
            widget::text("!")
                .font(cosmic::font::bold())
                .class(cosmic::theme::Text::Color(warning()))
                .into(),
            widget::mouse_area(
                widget::text(name)
                    .font(cosmic::font::bold())
                    .class(cosmic::theme::Text::Color(colour)),
            )
            .on_press(Message::Room(RoomMessage::Pick(id.clone())))
            .into(),
            widget::tooltip(
                widget::text(why.clone())
                    .class(dim())
                    .width(Length::Fill)
                    .wrapping(cosmic::iced::widget::text::Wrapping::None)
                    .ellipsize(Ellipsize::End(EllipsizeHeightLimit::Lines(1))),
                widget::text(why),
                widget::tooltip::Position::Bottom,
            )
            .into(),
        ];
        if let Some(held) = self.agents.held(&node.id, self.time()) {
            parts.push(widget::text(ago(held)).size(12.0).class(dim()).into());
        }
        parts.push(
            widget::text_input("Answer", draft)
                .on_input({
                    let id = id.clone();
                    move |text| Message::Room(RoomMessage::Draft(id.clone(), text))
                })
                .on_submit({
                    let id = id.clone();
                    move |_| Message::Room(RoomMessage::Answer(id.clone()))
                })
                .width(Length::Fixed(240.0 * zoom))
                .into(),
        );
        parts.push(
            widget::tooltip(
                widget::button::icon(widget::icon::from_name("mail-send-symbolic"))
                    .on_press(Message::Room(RoomMessage::Answer(id.clone()))),
                widget::text("Send"),
                widget::tooltip::Position::Bottom,
            )
            .into(),
        );
        parts.push(
            widget::tooltip(
                widget::button::icon(widget::icon::from_name("utilities-terminal-symbolic"))
                    .on_press(Message::Room(RoomMessage::Focus(id))),
                widget::text("Bring its terminal forward"),
                widget::tooltip::Position::Bottom,
            )
            .into(),
        );
        widget::container(
            widget::Row::with_children(parts)
                .spacing(8)
                .align_y(Alignment::Center),
        )
        .padding([4, 10])
        .width(Length::Fill)
        .class(cosmic::theme::Container::custom(|theme| {
            let warn = Color::from(theme.cosmic().warning_color());
            container::Style {
                background: Some(Color { a: 0.08, ..warn }.into()),
                border: cosmic::iced::Border {
                    color: Color { a: 0.5, ..warn },
                    width: 1.0,
                    radius: 6.0.into(),
                },
                ..Default::default()
            }
        }))
        .into()
    }

    /// A box to write to the agent in, and its terminal brought forward.
    fn answer_box(&self, node: &Node) -> Element<'_, Message> {
        let id = node.id.clone();
        let draft = self.drafts.get(&node.id).map_or("", String::as_str);
        widget::Column::with_children(vec![
            widget::Row::with_children(vec![
                widget::text_input("Answer", draft)
                    .on_input({
                        let id = id.clone();
                        move |text| Message::Room(RoomMessage::Draft(id.clone(), text))
                    })
                    .on_submit({
                        let id = id.clone();
                        move |_| Message::Room(RoomMessage::Answer(id.clone()))
                    })
                    .width(Length::Fill)
                    .into(),
                widget::button::standard("Send")
                    .on_press(Message::Room(RoomMessage::Answer(id.clone())))
                    .into(),
            ])
            .spacing(6)
            .align_y(Alignment::Center)
            .into(),
            widget::button::link("Bring its terminal forward")
                .on_press(Message::Room(RoomMessage::Focus(id)))
                .into(),
        ])
        .spacing(4)
        .into()
    }

    /// The picked node: what the hover says, who started it, its pull requests, a box to
    /// answer an agent, its digest, and the runs an agent started with theirs.
    fn details<'a>(&'a self, node: &'a Node, width: f32) -> Element<'a, Message> {
        let mut parts: Vec<Element<Message>> = vec![
            widget::Row::with_children(vec![
                dot(node.state),
                widget::text(room_agents::kind(node)).class(dim()).into(),
                widget::space::horizontal().into(),
                widget::button::text("Close")
                    .on_press(Message::Room(RoomMessage::Unpick))
                    .into(),
            ])
            .spacing(8)
            .align_y(Alignment::Center)
            .into(),
        ];
        let held = self.agents.held(&node.id, self.time());
        for (i, line) in room_agents::about(node, held).into_iter().enumerate() {
            parts.push(match i {
                0 => widget::text::title4(line).into(),
                _ => widget::text(line).into(),
            });
        }
        if let Some(agent) = self.agents.agent_of(node) {
            let name = agent.name.clone().unwrap_or_else(|| "its agent".into());
            parts.push(
                widget::button::link(format!("Started by {name}"))
                    .on_press(Message::Room(RoomMessage::Pick(agent.id.clone())))
                    .into(),
            );
        }
        for pr in open_prs(node) {
            parts.push(link(format!("PR #{}", pr.number), pr.link));
        }
        if matches!(node.kind, Some(NodeKind::Agent(_))) {
            parts.push(self.answer_box(node));
        }
        if let Some(digest) = &node.digest {
            parts.push(widget::divider::horizontal::default().into());
            parts.push(widget::text(digest.clone()).into());
        }
        for run in self.agents.runs(&node.id) {
            parts.push(widget::divider::horizontal::default().into());
            parts.push(
                widget::Row::with_children(vec![
                    dot(run.state),
                    widget::text(run.name.clone().unwrap_or_else(|| "unnamed".into()))
                        .font(cosmic::font::bold())
                        .into(),
                    widget::text(room_agents::kind(run)).class(dim()).into(),
                ])
                .spacing(8)
                .align_y(Alignment::Center)
                .into(),
            );
            if let Some(doing) = &run.doing {
                parts.push(widget::text(doing.clone()).class(dim()).into());
            }
            if let Some(digest) = &run.digest {
                parts.push(widget::text(digest.clone()).into());
            }
        }
        widget::scrollable(
            widget::Column::with_children(parts)
                .spacing(8)
                .padding([16, 12]),
        )
        .width(Length::Fixed(width))
        .height(Length::Fill)
        .into()
    }

    fn grid(&self, zoom: f32, width: f32) -> Element<'_, Message> {
        if self.machines.is_empty() {
            return widget::text(
                "No dibs machines yet: the daemon looks every few seconds, unless dibs is off \
                 there or the daemon is not running.",
            )
            .class(dim())
            .into();
        }
        let shown = self.arrangement.shown(self.names());
        if shown.is_empty() {
            return widget::text("Every machine is hidden: show one from Arrange.")
                .class(dim())
                .into();
        }
        let columns = self
            .arrangement
            .columns(shown.len(), width, NARROWEST * zoom, GAP);
        let cell = (width - GAP * (columns - 1) as f32) / columns as f32;
        let rows: Vec<Element<Message>> = shown
            .chunks(columns)
            .map(|row| {
                let mut cells: Vec<Element<Message>> = row
                    .iter()
                    .filter_map(|name| self.machines.iter().find(|m| m.name == *name))
                    .map(|machine| self.cell(machine, zoom, cell))
                    .collect();
                // A short last row keeps the others' width.
                for _ in row.len()..columns {
                    cells.push(
                        widget::space::horizontal()
                            .width(Length::FillPortion(1))
                            .into(),
                    );
                }
                widget::Row::with_children(cells).spacing(GAP as u16).into()
            })
            .collect();
        widget::Column::with_children(rows)
            .spacing(GAP as u16)
            .into()
    }

    fn cell<'a>(&'a self, machine: &'a DibsMachine, zoom: f32, width: f32) -> Element<'a, Message> {
        let mut parts: Vec<Element<Message>> = vec![header(machine)];
        let jobs = machine
            .status
            .as_ref()
            .map(|status| Item::all(&machine.name, status))
            .unwrap_or_default();
        match &machine.status {
            Some(status) => {
                let history = self.histories.get(&machine.name);
                parts.extend(meters(
                    status,
                    history,
                    &jobs,
                    self.arrangement.show,
                    width - 24.0,
                    zoom,
                ));
                parts.push(widget::divider::horizontal::default().into());
                parts.push(self.jobs(&machine.name, jobs, zoom));
            }
            None => {
                if let Some(trouble) = &machine.trouble {
                    parts.push(
                        widget::text(plain(trouble))
                            .font(cosmic::font::mono())
                            .class(dim())
                            .into(),
                    );
                }
            }
        }
        widget::container(widget::Column::with_children(parts).spacing(6))
            .padding(12)
            .width(Length::FillPortion(1))
            .class(cosmic::theme::Container::Card)
            .into()
    }

    fn jobs(&self, machine: &str, jobs: Vec<Item>, zoom: f32) -> Element<'_, Message> {
        if jobs.is_empty() {
            return widget::text("nothing holding it, nothing queued")
                .class(cosmic::theme::Text::Color(green()))
                .into();
        }
        let mut parts: Vec<Element<Message>> = Vec::new();
        for running in [true, false] {
            let group: Vec<&Item> = jobs.iter().filter(|it| it.holding == running).collect();
            if group.is_empty() {
                continue;
            }
            parts.push(match running {
                true => widget::text(format!("Running now ({})", group.len()))
                    .class(cosmic::theme::Text::Color(green()))
                    .into(),
                false => widget::text(format!("Waiting ({})", group.len()))
                    .class(dim())
                    .into(),
            });
            for it in group {
                let selected = self.selected.get(machine) == Some(&it.pid);
                parts.push(job(it, selected, zoom));
            }
        }
        widget::Column::with_children(parts).spacing(6).into()
    }

    fn confirming(&self) -> Option<Element<'_, Message>> {
        let (machine, pid, label) = self.confirm.as_ref()?;
        Some(
            widget::container(
                widget::Column::with_children(vec![
                    widget::text::heading("Stop this job?").into(),
                    widget::text(format!(
                        "Kill {pid} ({label}) on {machine}, whoever started it. It runs dibs \
                         --kill {pid} --anyone."
                    ))
                    .into(),
                    widget::Row::with_children(vec![
                        widget::button::destructive("Kill it")
                            .on_press(Message::Room(RoomMessage::Killing(true)))
                            .into(),
                        widget::button::text("Leave it")
                            .on_press(Message::Room(RoomMessage::Killing(false)))
                            .into(),
                    ])
                    .spacing(8)
                    .into(),
                ])
                .spacing(8),
            )
            .padding(12)
            .class(cosmic::theme::Container::Card)
            .into(),
        )
    }

    /// Which machines show and in what order, which are read for metrics, how many columns,
    /// what a cell shows, and how often the daemon looks.
    fn arrange(&self) -> Element<'_, Message> {
        let ordered = self.arrangement.ordered(self.names());
        let mut rows: Vec<Element<Message>> = Vec::new();
        for (i, name) in ordered.iter().enumerate() {
            let shown = !self.arrangement.hidden.contains(name);
            let read = self
                .watch
                .as_ref()
                .is_some_and(|watch| !watch.no_metrics.contains(name));
            let name_for = |f: fn(String, bool) -> RoomMessage, name: &String| {
                let name = name.clone();
                move |on| Message::Room(f(name.clone(), on))
            };
            let mut up = widget::button::icon(widget::icon::from_name("go-up-symbolic"));
            if i > 0 {
                up = up.on_press(Message::Room(RoomMessage::Swap(i, i - 1)));
            }
            let mut down = widget::button::icon(widget::icon::from_name("go-down-symbolic"));
            if i + 1 < ordered.len() {
                down = down.on_press(Message::Room(RoomMessage::Swap(i, i + 1)));
            }
            rows.push(
                widget::Row::with_children(vec![
                    widget::text(name.clone())
                        .width(Length::Fixed(180.0))
                        .into(),
                    widget::checkbox(shown)
                        .label("shown")
                        .on_toggle(name_for(RoomMessage::Shown, name))
                        .into(),
                    widget::checkbox(read)
                        .label("metrics")
                        .on_toggle(name_for(RoomMessage::Metrics, name))
                        .into(),
                    up.into(),
                    down.into(),
                ])
                .spacing(12)
                .align_y(Alignment::Center)
                .into(),
            );
        }
        let columns = self.arrangement.columns;
        let mut choices: Vec<Element<Message>> = vec![widget::text("columns").into()];
        for (label, value) in [
            ("fit", None),
            ("1", Some(1)),
            ("2", Some(2)),
            ("3", Some(3)),
            ("4", Some(4)),
        ] {
            let button = match columns == value {
                true => widget::button::suggested(label),
                false => widget::button::standard(label),
            };
            choices.push(
                button
                    .on_press(Message::Room(RoomMessage::Columns(value)))
                    .into(),
            );
        }
        rows.push(
            widget::Row::with_children(choices)
                .spacing(6)
                .align_y(Alignment::Center)
                .into(),
        );
        let show = self.arrangement.show;
        let toggle =
            |label: &'static str, on: bool, set: fn(&mut Show, bool)| -> Element<'_, Message> {
                widget::checkbox(on)
                    .label(label)
                    .on_toggle(move |on| {
                        let mut next = show;
                        set(&mut next, on);
                        Message::Room(RoomMessage::Show(next))
                    })
                    .into()
            };
        rows.push(
            widget::Row::with_children(vec![
                toggle("CPU graph", show.cpu, |show, on| show.cpu = on),
                toggle("memory", show.memory, |show, on| show.memory = on),
                toggle("disks", show.disks, |show, on| show.disks = on),
                toggle("sleeping cards", show.asleep, |show, on| show.asleep = on),
            ])
            .spacing(12)
            .into(),
        );
        let mut cards: Vec<Element<Message>> = vec![widget::text("cards").into()];
        for (label, value) in [
            ("graphs", Cards::Graphs),
            ("rows", Cards::Rows),
            ("hidden", Cards::Hidden),
        ] {
            let button = match show.cards == value {
                true => widget::button::suggested(label),
                false => widget::button::standard(label),
            };
            cards.push(
                button
                    .on_press(Message::Room(RoomMessage::Show(Show {
                        cards: value,
                        ..show
                    })))
                    .into(),
            );
        }
        rows.push(
            widget::Row::with_children(cards)
                .spacing(6)
                .align_y(Alignment::Center)
                .into(),
        );
        if let Some(watch) = &self.watch {
            let poll = self.poll.unwrap_or(watch.poll);
            rows.push(
                widget::Row::with_children(vec![
                    widget::text(format!("look every {poll} s"))
                        .width(Length::Fixed(140.0))
                        .into(),
                    widget::slider(
                        *DIBS_POLL.start() as f64..=*DIBS_POLL.end() as f64,
                        poll as f64,
                        |seconds| Message::Room(RoomMessage::Poll(seconds.round() as u64)),
                    )
                    .on_release(Message::Room(RoomMessage::PollDone))
                    .width(Length::Fixed(260.0))
                    .into(),
                ])
                .spacing(12)
                .align_y(Alignment::Center)
                .into(),
            );
        }
        widget::container(widget::Column::with_children(rows).spacing(8))
            .padding(12)
            .class(cosmic::theme::Container::Card)
            .into()
    }
}

/// A dot in a node state's colour.
fn dot<'a>(shown: Option<NodeState>) -> Element<'a, Message> {
    let colour = match state_tint(shown) {
        room_agents::Tint::Rgb(rgb) => super::room_map::rgb(rgb),
        room_agents::Tint::Warn => warning(),
        room_agents::Tint::Error => danger(),
        _ => dim_color(),
    };
    widget::text("●")
        .class(cosmic::theme::Text::Color(colour))
        .into()
}

fn legend<'a>() -> Element<'a, Message> {
    let mut parts: Vec<Element<Message>> = Vec::new();
    for shown in LEGEND {
        parts.push(dot(Some(shown)));
        parts.push(widget::text(state(Some(shown))).class(dim()).into());
    }
    parts.push(
        widget::text("round: Claude, square: Codex, diamond: dibs job.")
            .class(dim())
            .into(),
    );
    for (word, tint) in legend_kinds() {
        parts.push(
            widget::text(word)
                .class(cosmic::theme::Text::Color(super::room_map::rgb(tint)))
                .into(),
        );
    }
    parts.push(
        widget::text("around an agent: filled and turning while it runs, hollow while it waits. Click one for details.")
            .class(dim())
            .into(),
    );
    widget::flex_row(parts)
        .spacing(6)
        .align_items(Alignment::Center)
        .into()
}

fn link<'a>(label: String, url: &str) -> Element<'a, Message> {
    widget::button::link(label)
        .on_press(Message::Link(url.to_string()))
        .into()
}

/// The machine's name, its state, and the calls about the whole machine.
fn header(machine: &DibsMachine) -> Element<'_, Message> {
    let mut parts: Vec<Element<Message>> = vec![widget::text::title4(machine.name.clone()).into()];
    match &machine.status {
        Some(status) => {
            let (words, tone) = state_words(status.state);
            let colour = match tone {
                Tone::Free => green(),
                Tone::InUse => warning(),
                Tone::Alarm => danger(),
            };
            parts.push(
                widget::text(words)
                    .class(cosmic::theme::Text::Color(colour))
                    .into(),
            );
            if !status.queue.is_empty() {
                parts.push(
                    widget::text(format!("{} queued", status.queue.len()))
                        .class(dim())
                        .into(),
                );
            }
            if let Some(closed) = &status.closed {
                parts.push(
                    widget::text(format!("closed by {}", closed.by))
                        .class(destructive())
                        .into(),
                );
            }
        }
        None if machine.trouble.is_some() => {
            parts.push(widget::text("down").class(destructive()).into())
        }
        None => parts.push(widget::text("connecting…").class(dim()).into()),
    }
    for (label, call) in [
        ("log", Call::Log),
        ("gpu", Call::Gpu),
        ("refresh", Call::Status),
    ] {
        parts.push(
            widget::button::text(label)
                .on_press(Message::Room(RoomMessage::Ask(
                    machine.name.clone(),
                    call,
                    String::new(),
                )))
                .into(),
        );
    }
    widget::flex_row(parts)
        .spacing(8)
        .align_items(Alignment::Center)
        .into()
}

/// The CPU as a graph, memory and each disk as a bar, then the cards, as btop draws them.
fn meters<'a>(
    status: &'a Status,
    history: Option<&'a History>,
    jobs: &[Item],
    show: Show,
    width: f32,
    zoom: f32,
) -> Vec<Element<'a, Message>> {
    let mut parts = Vec::new();
    let metrics = status.metrics.as_ref();
    let busy = metrics
        .and_then(|metrics| metrics.cpu_busy_permille)
        .map(|busy| busy as f32 / 1000.0);
    let load = format!("load {:.1}/{}", status.load as f32 / 100.0, status.cores);
    if show.cpu {
        let caption = match busy {
            Some(busy) => format!("CPU {}   {load}", percent(busy)),
            None => format!("CPU   {load}"),
        };
        let points = history
            .map(|history| history.cpu.iter().copied().collect())
            .unwrap_or_default();
        parts.push(graph(points, busy, caption, CPU_GRAPH * zoom));
    }
    if let Some((used, total)) = metrics.and_then(memory).filter(|_| show.memory) {
        let share = used as f32 / total.max(1) as f32;
        parts.push(bar(
            share,
            format!("Mem {}   {}", percent(share), gib(used, total)),
            BAR * zoom,
        ));
    }
    if show.disks {
        for disk in metrics.map_or(&[][..], |metrics| &metrics.disks[..]) {
            let (share, caption) = filled(disk);
            parts.push(bar(share, caption, BAR * zoom));
        }
    }
    let gpus = metrics.map_or(&[][..], |metrics| &metrics.gpus[..]);
    let (asleep, awake): (Vec<&GpuMetrics>, Vec<&GpuMetrics>) =
        gpus.iter().partition(|gpu| gpu.suspended);
    match show.cards {
        Cards::Graphs if !awake.is_empty() => {
            let per_row =
                (((width + GAP) / (CARD * zoom + GAP)).floor() as usize).clamp(1, awake.len());
            for row in awake.chunks(per_row) {
                let mut panels: Vec<Element<Message>> = row
                    .iter()
                    .map(|gpu| {
                        let points = history
                            .and_then(|history| history.gpus.get(&gpu.pci))
                            .map(|points| points.iter().copied().collect())
                            .unwrap_or_default();
                        panel(gpu, points, jobs, zoom)
                    })
                    .collect();
                for _ in row.len()..per_row {
                    panels.push(
                        widget::space::horizontal()
                            .width(Length::FillPortion(1))
                            .into(),
                    );
                }
                parts.push(
                    widget::Row::with_children(panels)
                        .spacing(GAP as u16)
                        .into(),
                );
            }
        }
        Cards::Rows => {
            for gpu in &awake {
                parts.push(card_row(gpu, jobs, zoom));
            }
        }
        _ => {}
    }
    if !asleep.is_empty() && show.asleep && show.cards != Cards::Hidden {
        let names: Vec<String> = asleep.iter().map(|gpu| card_name(gpu)).collect();
        parts.push(
            widget::text(format!("asleep: {}", names.join(", ")))
                .class(dim())
                .into(),
        );
    }
    parts
}

/// One card as a panel: its name and temperature, its use as a graph, its memory as a bar,
/// its power, clock and what holds it down, and the job on it.
fn panel<'a>(gpu: &GpuMetrics, points: Vec<f32>, jobs: &[Item], zoom: f32) -> Element<'a, Message> {
    let on = running_on(gpu, jobs);
    let mut name = widget::text(card_name(gpu))
        .font(cosmic::font::bold())
        .width(Length::Fill)
        .wrapping(cosmic::iced::widget::text::Wrapping::None)
        .ellipsize(Ellipsize::End(EllipsizeHeightLimit::Lines(1)));
    if !on.is_empty() {
        name = name.class(cosmic::theme::Text::Color(warning()));
    }
    let mut top: Vec<Element<Message>> = vec![name.into()];
    if let Some(temp) = gpu.temp_c {
        top.push(
            widget::text(format!("{temp}°C"))
                .class(cosmic::theme::Text::Color(rgb(level(heat(temp)))))
                .into(),
        );
    }
    let mut parts: Vec<Element<Message>> = vec![widget::Row::with_children(top).into()];
    let util = gpu.util_pct.map(|util| util as f32 / 100.0);
    parts.push(graph(
        points,
        util,
        util.map_or(String::new(), percent),
        CARD_GRAPH * zoom,
    ));
    if let (Some(used), Some(total)) = (gpu.mem_used, gpu.mem_total) {
        parts.push(bar(
            used as f32 / total.max(1) as f32,
            gib(used, total),
            (BAR - 2.0) * zoom,
        ));
    }
    let mut facts: Vec<Element<Message>> = Vec::new();
    if let Some(power) = gpu.power_mw {
        facts.push(
            widget::text(format!("{} W", power / 1000))
                .class(dim())
                .into(),
        );
    }
    if let Some(clock) = gpu.clock_mhz {
        facts.push(widget::text(format!("{clock} MHz")).class(dim()).into());
    }
    facts.extend(throttles(gpu));
    parts.push(widget::flex_row(facts).spacing(6).into());
    for job in on {
        parts.push(
            widget::text(format!("← {}", job.label.as_str()))
                .class(cosmic::theme::Text::Color(warning()))
                .into(),
        );
    }
    let busy = !running_on(gpu, jobs).is_empty();
    widget::container(widget::Column::with_children(parts).spacing(3))
        .padding(6)
        .width(Length::FillPortion(1))
        .class(cosmic::theme::Container::custom(move |theme| {
            let cosmic = theme.cosmic();
            let border = match busy {
                true => Color::from(cosmic.warning_color()),
                false => Color::from(cosmic.bg_divider()),
            };
            container::Style {
                border: cosmic::iced::Border {
                    color: border,
                    width: if busy { 1.5 } else { 1.0 },
                    radius: 4.0.into(),
                },
                ..Default::default()
            }
        }))
        .into()
}

/// One card as a line: its name, its use and memory as bars, its temperature and its job.
fn card_row<'a>(gpu: &GpuMetrics, jobs: &[Item], zoom: f32) -> Element<'a, Message> {
    let on = running_on(gpu, jobs);
    let mut name = widget::text(card_name(gpu))
        .font(cosmic::font::bold())
        .width(Length::Fixed(140.0 * zoom));
    if !on.is_empty() {
        name = name.class(cosmic::theme::Text::Color(warning()));
    }
    let util = gpu.util_pct.unwrap_or(0) as f32 / 100.0;
    let mut parts: Vec<Element<Message>> = vec![
        name.into(),
        widget::container(bar(util, percent(util), BAR * zoom))
            .width(Length::Fixed(ROW_BAR * zoom))
            .into(),
    ];
    if let (Some(used), Some(total)) = (gpu.mem_used, gpu.mem_total) {
        parts.push(
            widget::container(bar(
                used as f32 / total.max(1) as f32,
                gib(used, total),
                BAR * zoom,
            ))
            .width(Length::Fixed(ROW_BAR * 1.5 * zoom))
            .into(),
        );
    }
    if let Some(temp) = gpu.temp_c {
        parts.push(
            widget::text(format!("{temp}°C"))
                .class(cosmic::theme::Text::Color(rgb(level(heat(temp)))))
                .into(),
        );
    }
    let names: Vec<&str> = on.iter().map(|job| job.label.as_str()).collect();
    if !names.is_empty() {
        parts.push(
            widget::text(format!("← {}", names.join(", ")))
                .class(cosmic::theme::Text::Color(warning()))
                .into(),
        );
    }
    widget::Row::with_children(parts)
        .spacing(8)
        .align_y(Alignment::Center)
        .into()
}

fn throttles<'a>(gpu: &GpuMetrics) -> Vec<Element<'a, Message>> {
    gpu.throttle
        .iter()
        .flatten()
        .filter_map(|reason| held_by(reason))
        .map(|(words, alarm)| {
            let colour = if alarm { danger() } else { warning() };
            widget::text(words)
                .class(cosmic::theme::Text::Color(colour))
                .into()
        })
        .collect()
}

/// One job on a line that wraps: how it holds the machine, its label, who, on which card,
/// how far along and for how long; selected, what it runs and what can be done about it.
fn job(it: &Item, selected: bool, zoom: f32) -> Element<'static, Message> {
    let hue = super::room_map::rgb(room_agents::agent_colour(&it.agent));
    let mode = match it.mode {
        Mode::Bench => danger(),
        _ => warning(),
    };
    let mut first: Vec<Element<Message>> = vec![
        widget::text(it.mode.to_string())
            .font(cosmic::font::bold())
            .class(cosmic::theme::Text::Color(mode))
            .into(),
    ];
    if it.holding && it.slot != "HOLDING" {
        first.push(
            widget::text(it.slot.to_lowercase())
                .class(cosmic::theme::Text::Color(warning()))
                .into(),
        );
    }
    if !it.holding {
        first.push(
            widget::text(it.slot.replace("queued ", "#"))
                .class(dim())
                .into(),
        );
    }
    first.push(
        widget::text(it.label.as_str().to_string())
            .font(cosmic::font::bold())
            .width(Length::Fill)
            .wrapping(cosmic::iced::widget::text::Wrapping::None)
            .ellipsize(Ellipsize::End(EllipsizeHeightLimit::Lines(1)))
            .into(),
    );
    let time = Span(it.time).to_string();
    first.push(match it.holding {
        true => widget::text(format!("for {time}")).into(),
        false => widget::text(format!("waited {time}")).class(dim()).into(),
    });
    let mut second: Vec<Element<Message>> = vec![
        widget::text(it.agent.clone())
            .class(cosmic::theme::Text::Color(hue))
            .into(),
    ];
    if let Some(device) = &it.device {
        second.push(
            widget::text(device.as_str().to_string())
                .class(cosmic::theme::Text::Color(Color::from_rgb8(200, 110, 200)))
                .into(),
        );
    }
    if let Some(batch) = &it.batch {
        second.push(widget::text(batch.progress()).class(dim()).into());
    } else if let Some(eta) = it.eta.filter(|_| it.holding) {
        second.push(
            widget::text(format!("~{} left", Span(eta)))
                .class(dim())
                .into(),
        );
    }
    if let Some(rate) = it.rate {
        second.push(
            widget::text(format!("{} cpu", cores(rate)))
                .class(dim())
                .into(),
        );
    }
    let mut parts: Vec<Element<Message>> = vec![
        widget::Row::with_children(first)
            .spacing(8)
            .align_y(Alignment::Center)
            .into(),
        widget::flex_row(second).spacing(8).into(),
    ];
    if it.alarm && !selected {
        parts.push(
            widget::text(it.note.clone())
                .class(cosmic::theme::Text::Color(warning()))
                .into(),
        );
    }
    if selected {
        parts.extend(detail(it, zoom));
    }
    // The stripe says how it holds the machine, or that it waits.
    let stripe = if it.holding { mode } else { dim_color() };
    let running = it.holding;
    let card = widget::container(
        widget::Row::with_children(vec![
            widget::container(widget::space::horizontal())
                .width(Length::Fixed(4.0))
                .height(Length::Fill)
                .class(cosmic::theme::Container::custom(move |_| {
                    container::Style {
                        background: Some(stripe.into()),
                        border: cosmic::iced::Border {
                            radius: [6.0, 0.0, 0.0, 6.0].into(),
                            ..Default::default()
                        },
                        ..Default::default()
                    }
                }))
                .into(),
            widget::container(widget::Column::with_children(parts).spacing(2))
                .padding([6, 10])
                .width(Length::Fill)
                .into(),
        ])
        .height(Length::Shrink),
    )
    .width(Length::Fill)
    .class(cosmic::theme::Container::custom(move |theme| {
        let cosmic = theme.cosmic();
        let base = Color::from(cosmic.bg_component_color());
        let fill = match (selected, running) {
            (true, _) => Color {
                a: 0.4,
                ..Color::from(cosmic.accent_color())
            },
            (false, true) => base,
            (false, false) => Color { a: 0.55, ..base },
        };
        container::Style {
            background: Some(fill.into()),
            border: cosmic::iced::Border {
                radius: 6.0.into(),
                ..Default::default()
            },
            ..Default::default()
        }
    }));
    let pressed = Message::Room(RoomMessage::Select(it.machine.clone(), it.pid));
    let card = widget::mouse_area(card).on_press(pressed);
    match it.note.is_empty() || it.alarm || selected {
        true => card.into(),
        false => widget::tooltip(
            card,
            widget::text(it.note.clone()),
            widget::tooltip::Position::Top,
        )
        .into(),
    }
}

fn detail(it: &Item, zoom: f32) -> Vec<Element<'static, Message>> {
    let mut parts: Vec<Element<Message>> = vec![
        widget::text(it.cmd.clone())
            .font(cosmic::font::mono())
            .size(12.0 * zoom)
            .into(),
    ];
    let mut facts: Vec<Element<Message>> =
        vec![widget::text(format!("pid {}", it.pid)).class(dim()).into()];
    if let Some(cpu) = it.cpu {
        facts.push(
            widget::text(format!("{} cpu in all", Span(cpu)))
                .class(dim())
                .into(),
        );
    }
    if let Some(out) = &it.output {
        facts.push(widget::text(format!("writing {out}")).class(dim()).into());
    }
    if let Some(batch) = &it.batch {
        facts.push(
            widget::text(format!("batch {}: {}", batch.id, batch.step))
                .class(dim())
                .into(),
        );
    }
    parts.push(widget::flex_row(facts).spacing(6).into());
    if !it.long.is_empty() {
        parts.push(match it.alarm {
            true => widget::text(format!("\u{26a0} {}", it.long))
                .class(cosmic::theme::Text::Color(warning()))
                .into(),
            false => widget::text(it.long.clone()).class(dim()).into(),
        });
    }
    let ask = |call: Call| {
        Message::Room(RoomMessage::Ask(
            it.machine.clone(),
            call,
            it.label.as_str().to_string(),
        ))
    };
    let mut buttons: Vec<Element<Message>> = vec![
        widget::button::text("process tree")
            .on_press(ask(Call::Tree { pid: it.pid }))
            .into(),
    ];
    if it.output.is_some() {
        buttons.push(
            widget::button::text("output")
                .on_press(ask(Call::Output { pid: it.pid }))
                .into(),
        );
    }
    buttons.push(
        widget::button::destructive("kill")
            .on_press(Message::Room(RoomMessage::Kill(
                it.machine.clone(),
                it.pid,
                it.label.clone(),
            )))
            .into(),
    );
    parts.push(widget::Row::with_children(buttons).spacing(6).into());
    parts
}

fn answer<'a>(title: &'a str, body: &'a str) -> Element<'a, Message> {
    widget::container(
        widget::Column::with_children(vec![
            widget::Row::with_children(vec![
                widget::text::heading(title).into(),
                widget::space::horizontal().into(),
                widget::button::text("Close")
                    .on_press(Message::Room(RoomMessage::CloseShown))
                    .into(),
            ])
            .align_y(Alignment::Center)
            .into(),
            widget::scrollable(widget::text(body).font(cosmic::font::mono()).size(12.0))
                .height(Length::Fixed(320.0))
                .into(),
        ])
        .spacing(8),
    )
    .padding(12)
    .class(cosmic::theme::Container::Card)
    .into()
}

/// The last looks as columns, each as tall and as hot as its value, with `caption` on top.
struct Graph {
    points: Vec<f32>,
    now: Option<f32>,
    caption: String,
}

impl canvas::Program<Message, cosmic::Theme, cosmic::Renderer> for Graph {
    type State = ();

    fn draw(
        &self,
        _state: &(),
        renderer: &cosmic::Renderer,
        theme: &cosmic::Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<canvas::Geometry<cosmic::Renderer>> {
        let mut frame = Frame::new(renderer, bounds.size());
        well(&mut frame, bounds, theme);
        let column = bounds.width / KEPT as f32;
        let single: Vec<f32> = self.now.into_iter().collect();
        let points = if self.points.is_empty() {
            &single
        } else {
            &self.points
        };
        for (i, value) in points.iter().rev().enumerate() {
            let right = bounds.width - column * i as f32;
            let height = (value * bounds.height).max(if *value > 0.0 { 1.0 } else { 0.0 });
            frame.fill_rectangle(
                Point::new(right - column, bounds.height - height),
                Size::new(column, height),
                rgb(level(*value)),
            );
        }
        caption(&mut frame, &self.caption, bounds);
        vec![frame.into_geometry()]
    }
}

/// How full it is, the colour of how full, with `caption` on it.
struct Bar {
    share: f32,
    caption: String,
}

impl canvas::Program<Message, cosmic::Theme, cosmic::Renderer> for Bar {
    type State = ();

    fn draw(
        &self,
        _state: &(),
        renderer: &cosmic::Renderer,
        theme: &cosmic::Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<canvas::Geometry<cosmic::Renderer>> {
        let mut frame = Frame::new(renderer, bounds.size());
        well(&mut frame, bounds, theme);
        let width = bounds.width * self.share.clamp(0.0, 1.0);
        frame.fill(
            &Path::rounded_rectangle(Point::ORIGIN, Size::new(width, bounds.height), 3.0.into()),
            Color {
                a: 0.8,
                ..rgb(level(self.share))
            },
        );
        caption(&mut frame, &self.caption, bounds);
        vec![frame.into_geometry()]
    }
}

fn graph<'a>(
    points: Vec<f32>,
    now: Option<f32>,
    caption: String,
    height: f32,
) -> Element<'a, Message> {
    canvas::Canvas::new(Graph {
        points,
        now,
        caption,
    })
    .width(Length::Fill)
    .height(Length::Fixed(height))
    .into()
}

fn bar<'a>(share: f32, caption: String, height: f32) -> Element<'a, Message> {
    canvas::Canvas::new(Bar { share, caption })
        .width(Length::Fill)
        .height(Length::Fixed(height))
        .into()
}

/// The ground a meter is drawn on: the window's own colour, so it reads the same over a
/// translucent card in either theme.
fn well(frame: &mut Frame<cosmic::Renderer>, bounds: Rectangle, theme: &cosmic::Theme) {
    frame.fill(
        &Path::rounded_rectangle(Point::ORIGIN, bounds.size(), 3.0.into()),
        Color {
            a: 0.85,
            ..Color::from(theme.cosmic().bg_color())
        },
    );
}

/// Text over a graph, with a dark edge so it reads over any colour.
fn caption(frame: &mut Frame<cosmic::Renderer>, text: &str, bounds: Rectangle) {
    let size = (bounds.height - 3.0).clamp(10.0, 13.0);
    let at = Point::new(5.0, 1.0);
    for (dx, dy) in [(1.0, 1.0), (-1.0, 0.0), (0.0, -1.0)] {
        frame.fill_text(canvas::Text {
            content: text.to_string(),
            position: Point::new(at.x + dx, at.y + dy),
            color: Color::from_rgba(0.0, 0.0, 0.0, 0.8),
            size: size.into(),
            ..Default::default()
        });
    }
    frame.fill_text(canvas::Text {
        content: text.to_string(),
        position: at,
        color: Color::WHITE,
        size: size.into(),
        ..Default::default()
    });
}

fn rgb([r, g, b]: [u8; 3]) -> Color {
    Color::from_rgb8(r, g, b)
}

fn green() -> Color {
    Color::from_rgb8(90, 190, 110)
}

fn warning() -> Color {
    Color::from(cosmic::theme::active().cosmic().warning_color())
}

fn danger() -> Color {
    Color::from(cosmic::theme::active().cosmic().destructive_color())
}

/// The room, while a window shows it.
pub fn worker() -> impl Stream<Item = Message> {
    stream::channel(
        64,
        |mut output: cosmic::iced::futures::channel::mpsc::Sender<Message>| async move {
            let (commands, mut events) = connect_room(remote::host().map(str::to_string));
            let _ = output
                .send(Message::Room(RoomMessage::Connected(commands)))
                .await;
            while let Some(event) = events.recv().await {
                let mut batch = vec![event];
                while let Some(more) = events.try_recv() {
                    batch.push(more);
                    if batch.len() >= 256 {
                        break;
                    }
                }
                if output
                    .send(Message::Room(RoomMessage::Events(batch)))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        },
    )
}
