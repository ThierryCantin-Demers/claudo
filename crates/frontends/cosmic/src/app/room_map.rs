//! The map of agents on a canvas: one island per repository, each agent with what it
//! started fanned out above it, laid out by `claudo_room` as the web page lays it out.
//! Clicking a node opens its details; double-clicking an agent brings its terminal forward.

use std::f32::consts::PI;
use std::time::{Duration, Instant};

use claudo_protocol::{Node, NodeKind, NodeState};
use claudo_room::agents::{
    AGENT, CAPTION_REACH, CHILD, LABELLED, Map, PAD, Tint, V, badges, codex, dibs_job, hue,
    made_by, phase, started, started_word, state_tint,
};
use cosmic::iced::widget::canvas::{self, Frame, Path, Stroke, path::Arc};
use cosmic::iced::{Radians, Rectangle, mouse};

use super::room::RoomMessage;
use super::*;

/// How soon after a click on an agent a second one brings its terminal forward.
const DOUBLE_CLICK: Duration = Duration::from_millis(600);

/// The map as laid out for this frame, at `scale` screen pixels to a map unit.
pub struct MapView<'a> {
    pub map: Map<'a>,
    pub scale: f32,
    /// Seconds on the room's clock, for what pulses.
    pub now: f64,
    pub selected: Option<&'a str>,
}

#[derive(Default)]
pub struct Pointer {
    hovered: Option<String>,
    last: Option<(String, Instant)>,
}

/// A node as drawn: where, how big, how deep under its agent.
struct Drawn<'a> {
    node: &'a Node,
    at: Point,
    radius: f32,
}

impl<'a> MapView<'a> {
    fn point(&self, at: V) -> Point {
        Point::new(at.x * self.scale, at.y * self.scale)
    }

    fn drawn(&self) -> Vec<Drawn<'a>> {
        self.map
            .islands
            .iter()
            .flat_map(|island| {
                island.placed.iter().map(move |placed| Drawn {
                    node: placed.node,
                    at: self.point(island.top + placed.at),
                    radius: self.scale * if placed.depth == 0 { AGENT } else { CHILD },
                })
            })
            .collect()
    }

    /// The node under `pointer`, the nearest when several are.
    fn under(&self, pointer: Point) -> Option<Drawn<'a>> {
        self.drawn()
            .into_iter()
            .filter(|drawn| drawn.at.distance(pointer) <= drawn.radius + 6.0)
            .min_by(|a, b| a.at.distance(pointer).total_cmp(&b.at.distance(pointer)))
    }
}

impl canvas::Program<Message, cosmic::Theme, cosmic::Renderer> for MapView<'_> {
    type State = Pointer;

    fn update(
        &self,
        state: &mut Pointer,
        event: &canvas::Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Option<canvas::Action<Message>> {
        let pointer = cursor.position_in(bounds);
        match event {
            canvas::Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                let under = pointer.and_then(|pointer| self.under(pointer));
                let hovered = under.as_ref().map(|drawn| drawn.node.id.clone());
                (hovered != state.hovered).then(|| {
                    state.hovered = hovered;
                    // The page draws the hover's card over the canvas: text a canvas draws
                    // goes over all its shapes, so a card drawn here would not hide the map.
                    let at = under.map(|drawn| (drawn.node.id.clone(), drawn.at.x, drawn.at.y));
                    canvas::Action::publish(Message::Room(RoomMessage::Hover(at)))
                })
            }
            canvas::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let drawn = self.under(pointer?)?;
                let id = drawn.node.id.clone();
                let agent = matches!(drawn.node.kind, Some(NodeKind::Agent(_)));
                let again = state
                    .last
                    .take()
                    .is_some_and(|(last, at)| last == id && at.elapsed() < DOUBLE_CLICK);
                let message = match again && agent {
                    true => RoomMessage::Focus(id),
                    false => {
                        state.last = Some((id.clone(), Instant::now()));
                        RoomMessage::Pick(id)
                    }
                };
                Some(canvas::Action::publish(Message::Room(message)).and_capture())
            }
            _ => None,
        }
    }

    fn draw(
        &self,
        state: &Pointer,
        renderer: &cosmic::Renderer,
        theme: &cosmic::Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<canvas::Geometry<cosmic::Renderer>> {
        let palette = Palette::of(theme);
        let mut frame = Frame::new(renderer, bounds.size());
        let scale = self.scale;
        for island in &self.map.islands {
            let corner = self.point(island.top);
            shore(
                &mut frame,
                &palette,
                island.repo,
                corner,
                island.size,
                scale,
            );
            let at = |placed: &claudo_room::agents::Placed| self.point(island.top + placed.at);
            for placed in &island.placed {
                let Some(parent) = placed
                    .parent
                    .and_then(|parent| island.placed.iter().find(|p| p.node.id == parent))
                else {
                    continue;
                };
                edge(
                    &mut frame,
                    placed.node,
                    at(parent),
                    at(placed),
                    self.now,
                    scale,
                );
            }
            for placed in island.placed.iter().rev() {
                let here = at(placed);
                let radius = scale * if placed.depth == 0 { AGENT } else { CHILD };
                if placed.depth == 0 {
                    body(
                        &mut frame,
                        &palette,
                        placed.node,
                        here,
                        radius,
                        self.now,
                        scale,
                    );
                } else {
                    let siblings = island
                        .placed
                        .iter()
                        .filter(|other| other.parent == placed.parent)
                        .count();
                    let label = placed.depth == 1 && siblings <= LABELLED;
                    started_body(
                        &mut frame,
                        &palette,
                        placed.node,
                        here,
                        radius,
                        self.now,
                        scale,
                        label,
                    );
                }
                if self.selected == Some(placed.node.id.as_str()) {
                    frame.stroke(
                        &Path::circle(here, radius + 6.0 * scale),
                        Stroke::default().with_color(palette.accent).with_width(2.5),
                    );
                }
                if placed.depth == 0 {
                    let children = island.children(&placed.node.id).map(|child| child.node);
                    caption(
                        &mut frame,
                        &palette,
                        placed.node,
                        started(children),
                        here,
                        scale,
                    );
                }
            }
        }
        if let Some(drawn) = state
            .hovered
            .as_ref()
            .and_then(|id| self.drawn().into_iter().find(|drawn| drawn.node.id == *id))
        {
            frame.stroke(
                &Path::circle(drawn.at, drawn.radius + 4.0),
                Stroke::default().with_color(palette.strong).with_width(1.5),
            );
        }
        vec![frame.into_geometry()]
    }

    fn mouse_interaction(
        &self,
        state: &Pointer,
        _bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        match state.hovered {
            Some(_) => mouse::Interaction::Pointer,
            None => mouse::Interaction::default(),
        }
    }
}

/// The theme's colours the map draws with.
struct Palette {
    text: Color,
    strong: Color,
    weak: Color,
    warn: Color,
    error: Color,
    accent: Color,
    ground: Color,
    dark: bool,
}

impl Palette {
    fn of(theme: &cosmic::Theme) -> Self {
        let cosmic = theme.cosmic();
        Palette {
            text: Color::from(cosmic.on_bg_color()),
            strong: Color::from(cosmic.on_bg_color()),
            weak: Color {
                a: 0.6,
                ..Color::from(cosmic.on_bg_color())
            },
            warn: Color::from(cosmic.warning_color()),
            error: Color::from(cosmic.destructive_color()),
            accent: Color::from(cosmic.accent_color()),
            ground: Color::from(cosmic.bg_color()),
            dark: cosmic.is_dark,
        }
    }

    fn tint(&self, tint: Tint) -> Color {
        match tint {
            Tint::Text => self.text,
            Tint::Weak => self.weak,
            Tint::Warn => self.warn,
            Tint::Error => self.error,
            Tint::Rgb(rgb) => self::rgb(rgb),
        }
    }
}

pub fn rgb([r, g, b]: [u8; 3]) -> Color {
    Color::from_rgb8(r, g, b)
}

fn faded(color: Color, alpha: f32) -> Color {
    Color {
        a: color.a * alpha,
        ..color
    }
}

fn shore(
    frame: &mut Frame<cosmic::Renderer>,
    palette: &Palette,
    repo: Option<&str>,
    corner: Point,
    size: V,
    scale: f32,
) {
    let size = Size::new(size.x * scale, size.y * scale);
    let round = 16.0 * scale;
    let sea = rgb(hue(repo.unwrap_or(""), palette.dark));
    for (grow, alpha) in [(8.0, 0.05), (4.0, 0.08)] {
        let grow = grow * scale;
        frame.fill(
            &Path::rounded_rectangle(
                Point::new(corner.x - grow, corner.y - grow),
                Size::new(size.width + 2.0 * grow, size.height + 2.0 * grow),
                (round + grow).into(),
            ),
            faded(sea, alpha),
        );
    }
    let land = Path::rounded_rectangle(corner, size, round.into());
    frame.fill(&land, palette.ground);
    frame.fill(&land, faded(sea, 0.12));
    frame.stroke(
        &land,
        Stroke::default()
            .with_color(faded(sea, 0.6))
            .with_width(1.5),
    );
    let name = repo.map_or("elsewhere", claudo_room::agents::short);
    frame.fill_text(canvas::Text {
        content: name.to_string(),
        position: Point::new(corner.x + (PAD + 4.0) * scale, corner.y + 8.0 * scale),
        color: palette.strong,
        size: (14.0 * scale).into(),
        ..Default::default()
    });
}

/// A curve from an agent to what it started, a dot running along it while that works.
fn edge(
    frame: &mut Frame<cosmic::Renderer>,
    node: &Node,
    from: Point,
    to: Point,
    now: f64,
    scale: f32,
) {
    let tint = rgb(made_by(node));
    let (dx, dy) = (to.x - from.x, to.y - from.y);
    let length = (dx * dx + dy * dy).sqrt().max(0.001);
    let bend = Point::new(-dy / length * 10.0 * scale, dx / length * 10.0 * scale);
    let control = Point::new(from.x + dx / 2.0 + bend.x, from.y + dy / 2.0 + bend.y);
    let curve = Path::new(|path| {
        path.move_to(from);
        path.quadratic_curve_to(control, to);
    });
    frame.stroke(
        &curve,
        Stroke::default()
            .with_color(faded(tint, 0.45))
            .with_width(1.5),
    );
    if node.state == Some(NodeState::Working) {
        let t = ((now * 0.6 + phase(&node.id)) % 1.0) as f32;
        let along = |a: f32, b: f32, c: f32| {
            (1.0 - t) * (1.0 - t) * a + 2.0 * (1.0 - t) * t * b + t * t * c
        };
        let dot = Point::new(
            along(from.x, control.x, to.x),
            along(from.y, control.y, to.y),
        );
        frame.fill(&Path::circle(dot, 2.5 * scale), tint);
    }
}

fn shape(node: &Node, at: Point, radius: f32, diamond: f32, corner: f32) -> Path {
    if dibs_job(node) {
        let reach = radius * diamond;
        Path::new(|path| {
            path.move_to(Point::new(at.x, at.y - reach));
            path.line_to(Point::new(at.x + reach, at.y));
            path.line_to(Point::new(at.x, at.y + reach));
            path.line_to(Point::new(at.x - reach, at.y));
            path.close();
        })
    } else if codex(node) {
        let side = radius * 1.8;
        Path::rounded_rectangle(
            Point::new(at.x - side / 2.0, at.y - side / 2.0),
            Size::new(side, side),
            (radius * corner).into(),
        )
    } else {
        Path::circle(at, radius)
    }
}

/// An agent: its shape in its state's colour, rippling while it works and breathing while
/// it waits on the person.
fn body(
    frame: &mut Frame<cosmic::Renderer>,
    palette: &Palette,
    node: &Node,
    at: Point,
    radius: f32,
    now: f64,
    scale: f32,
) {
    let tint = palette.tint(state_tint(node.state));
    match node.state {
        Some(NodeState::Working) => {
            let p = ((now * 0.7 + phase(&node.id)) % 1.0) as f32;
            frame.stroke(
                &Path::circle(at, radius + p * radius),
                Stroke::default()
                    .with_color(faded(tint, 1.0 - p))
                    .with_width(2.0 * (1.0 - p)),
            );
        }
        Some(NodeState::Blocked) => {
            let breath = (0.5 + 0.5 * (now * 3.0).sin()) as f32;
            frame.fill(
                &Path::circle(at, radius + (5.0 + 4.0 * breath) * scale),
                faded(tint, 0.15 + 0.2 * breath),
            );
        }
        _ => {}
    }
    let body = shape(node, at, radius, 1.25, 0.4);
    frame.fill(&body, faded(tint, 0.9));
    frame.stroke(
        &body,
        Stroke::default().with_color(palette.ground).with_width(2.0),
    );
    let mark = match node.state {
        Some(NodeState::Blocked) => "!",
        Some(NodeState::Failed) => "x",
        _ => return,
    };
    frame.fill_text(canvas::Text {
        content: mark.to_string(),
        position: at,
        color: palette.ground,
        size: (16.0 * scale).into(),
        align_x: cosmic::iced::Alignment::Center.into(),
        align_y: cosmic::iced::alignment::Vertical::Center,
        ..Default::default()
    });
}

/// Something an agent started: its kind's shape and colour, filled with a turning arc
/// around it while it runs, an outline while it waits, and outlined red once it failed.
#[allow(clippy::too_many_arguments)]
fn started_body(
    frame: &mut Frame<cosmic::Renderer>,
    palette: &Palette,
    node: &Node,
    at: Point,
    radius: f32,
    now: f64,
    scale: f32,
    label: bool,
) {
    let tint = rgb(made_by(node));
    let (fill, rim, width) = match node.state {
        Some(NodeState::Working) => (tint, palette.ground, 1.5),
        Some(NodeState::Failed) => (palette.ground, palette.error, 2.0),
        _ => (palette.ground, tint, 2.0),
    };
    let body = shape(node, at, radius, 1.3, 0.35);
    frame.fill(&body, fill);
    frame.stroke(&body, Stroke::default().with_color(rim).with_width(width));
    if node.state == Some(NodeState::Working) {
        let turn = (now * 0.35 + phase(&node.id)) as f32 * 2.0 * PI;
        let arc = Path::new(|path| {
            path.arc(Arc {
                center: at,
                radius: radius + 3.5 * scale,
                start_angle: Radians(turn),
                end_angle: Radians(turn + PI * 0.7),
            })
        });
        frame.stroke(
            &arc,
            Stroke::default()
                .with_color(faded(tint, 0.6))
                .with_width(1.2 * scale),
        );
    }
    if label {
        frame.fill_text(canvas::Text {
            content: started_word(node).to_string(),
            position: Point::new(at.x, at.y - radius - 6.0 * scale),
            color: tint,
            size: (9.5 * scale).into(),
            align_x: cosmic::iced::Alignment::Center.into(),
            align_y: cosmic::iced::alignment::Vertical::Bottom,
            ..Default::default()
        });
    }
}

/// Under an agent: its name, what it is doing, its badges, and what it started.
fn caption(
    frame: &mut Frame<cosmic::Renderer>,
    palette: &Palette,
    node: &Node,
    started: Vec<(String, [u8; 3])>,
    at: Point,
    scale: f32,
) {
    let width = 2.0 * CAPTION_REACH * scale;
    let mut top = at.y + (AGENT + 6.0) * scale;
    let name = node.name.as_deref().unwrap_or("unnamed");
    let size = 13.0 * scale;
    centred(
        frame,
        &[(cut(name, size, width), palette.strong)],
        at.x,
        top,
        size,
    );
    top += size * 1.3 + 2.0 * scale;
    if let Some(doing) = &node.doing {
        let size = 11.0 * scale;
        centred(
            frame,
            &[(cut(doing, size, width), palette.weak)],
            at.x,
            top,
            size,
        );
    }
    let badges: Vec<(String, Color)> = badges(node)
        .into_iter()
        .map(|(text, tint)| (text, palette.tint(tint)))
        .collect();
    if !badges.is_empty() {
        centred(frame, &badges, at.x, top + 15.0 * scale, 10.0 * scale);
    }
    if !started.is_empty() {
        let mut parts = Vec::new();
        for (i, (text, tint)) in started.into_iter().enumerate() {
            if i > 0 {
                parts.push(("  ·  ".to_string(), palette.weak));
            }
            parts.push((text, rgb(tint)));
        }
        centred(frame, &parts, at.x, top + 29.0 * scale, 10.5 * scale);
    }
}

/// About how wide `text` is at `size`: the canvas cannot measure it.
fn wide(text: &str, size: f32) -> f32 {
    text.chars().count() as f32 * size * 0.55
}

fn fits(text: &str, size: f32, width: f32) -> bool {
    wide(text, size) <= width
}

/// `text`, its end cut with an ellipsis to fit `width`.
fn cut(text: &str, size: f32, width: f32) -> String {
    if fits(text, size, width) {
        return text.to_string();
    }
    let keep = ((width / (size * 0.55)) as usize).saturating_sub(1);
    format!("{}…", text.chars().take(keep).collect::<String>())
}

/// Coloured parts on one row centred on `x`.
fn centred(
    frame: &mut Frame<cosmic::Renderer>,
    parts: &[(String, Color)],
    x: f32,
    y: f32,
    size: f32,
) {
    let total: f32 = parts.iter().map(|(text, _)| wide(text, size)).sum();
    let mut left = x - total / 2.0;
    for (text, color) in parts {
        frame.fill_text(canvas::Text {
            content: text.clone(),
            position: Point::new(left, y),
            color: *color,
            size: size.into(),
            ..Default::default()
        });
        left += wide(text, size);
    }
}
