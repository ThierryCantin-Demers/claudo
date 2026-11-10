use std::marker::PhantomData;
use std::path::PathBuf;
use std::process::Command;
use std::rc::Rc;
use std::time::Duration;

use claudo_protocol::{Standing, span};
use cosmic::app::{Core, Task};
use cosmic::iced::futures::channel::oneshot;
use cosmic::iced::widget::text::Wrapping;
use cosmic::iced::widget::{container, svg};
use cosmic::iced::window::Id;
use cosmic::iced::{Alignment, Border, Color, Length, Rectangle, Subscription};
use cosmic::surface::action::{app_popup, destroy_popup};
use cosmic::widget;
use cosmic::{Application, Element};

use crate::asks::{Asks, now};
use crate::presence;
use crate::usage::{Problem, Source, Usage};
use crate::warned::Warned;

/// Each ask starts a process, `claude` for about three seconds of CPU.
const REFRESH: Duration = Duration::from_secs(120);

pub struct Panel<S> {
    core: Core,
    popup: Option<Id>,
    usage: Option<Usage>,
    /// When the server measured the windows, in Unix seconds.
    usage_at: Option<u64>,
    error: Option<Problem>,
    asking: bool,
    /// Nobody has used this computer for a while, so the timer asks nothing.
    away: bool,
    asks: Asks,
    warned: Warned,
    /// The installed app's icon, telling this applet from the other tool's.
    logo: Option<widget::icon::Handle>,
    source: PhantomData<S>,
}

#[derive(Debug, Clone)]
pub enum Message {
    /// Asks the CLI, unless another applet did recently.
    Refresh,
    /// Asks the CLI anyway.
    RefreshNow,
    /// Whether anyone is at this computer.
    Present(bool),
    /// Redraws the time left between refreshes.
    Tick,
    Usage(Result<Usage, Problem>),
    Toggle(Rectangle),
    PopupClosed(Id),
    Surface(cosmic::surface::Action<Message>),
}

impl<S: Source> Application for Panel<S> {
    type Executor = cosmic::SingleThreadExecutor;
    type Flags = ();
    type Message = Message;
    const APP_ID: &'static str = S::APP_ID;

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn init(core: Core, _flags: ()) -> (Self, Task<Message>) {
        let mut panel = Panel {
            core,
            popup: None,
            usage: None,
            usage_at: None,
            error: None,
            asking: false,
            away: false,
            asks: Asks::shared(S::FILES, REFRESH.as_secs()),
            warned: Warned::shared(S::FILES),
            logo: logo(S::ICONS),
            source: PhantomData,
        };
        let ask = panel.ask(false);
        (panel, ask)
    }

    fn on_close_requested(&self, id: Id) -> Option<Message> {
        Some(Message::PopupClosed(id))
    }

    fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            cosmic::iced::time::every(REFRESH).map(|_| Message::Refresh),
            cosmic::iced::time::every(Duration::from_secs(30)).map(|_| Message::Tick),
            Subscription::run(presence::watch).map(Message::Present),
        ])
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Refresh if self.away => {}
            Message::Refresh => return self.ask(false),
            Message::RefreshNow => return self.ask(true),
            Message::Present(present) => {
                let back = self.away && present;
                self.away = !present;
                if back {
                    return self.ask(false);
                }
            }
            Message::Tick => {}
            Message::Usage(answer) => {
                self.asking = false;
                match answer {
                    Ok(usage) => {
                        let now = now();
                        self.usage_at = Some(usage.measured_at.unwrap_or(now));
                        self.error = None;
                        for (label, out_in) in self.warned.fresh(&usage, now) {
                            notify(S::NAME, label, out_in);
                        }
                        self.usage = Some(usage);
                    }
                    Err(problem) => {
                        // Numbers from before the logout would read as current.
                        if problem.brief().is_some() {
                            self.usage = None;
                            self.usage_at = None;
                        }
                        self.error = Some(problem);
                    }
                }
            }
            Message::Toggle(anchor) => {
                let action = match self.popup {
                    Some(id) => destroy_popup(id),
                    None => app_popup::<Panel<S>>(
                        |_| Default::default(),
                        move |panel: &mut Panel<S>| {
                            let id = Id::unique();
                            panel.popup = Some(id);
                            let mut settings = panel.core.applet.get_popup_settings(
                                panel.core.main_window_id().unwrap(),
                                id,
                                None,
                                None,
                                None,
                            );
                            settings.positioner.anchor_rect = Rectangle {
                                x: anchor.x as i32,
                                y: anchor.y as i32,
                                width: anchor.width as i32,
                                height: anchor.height as i32,
                            };
                            settings
                        },
                        Some(Box::new(|panel: &Panel<S>| {
                            Element::from(panel.core.applet.popup_container(panel.popup_view()))
                                .map(cosmic::Action::App)
                        })),
                    ),
                };
                let surface = surface(action);
                return if self.popup.is_none() {
                    Task::batch([surface, self.ask(false)])
                } else {
                    surface
                };
            }
            Message::PopupClosed(id) => {
                if self.popup == Some(id) {
                    self.popup = None;
                }
            }
            Message::Surface(action) => return surface(action),
        }
        Task::none()
    }

    fn view(&self) -> Element<'_, Message> {
        let now = now();
        let applet = &self.core.applet;
        let mut parts: Vec<Element<Message>> = vec![match &self.logo {
            Some(logo) => widget::icon(logo.clone())
                .size(applet.suggested_size(true).0)
                .into(),
            None => label(applet.text(S::NAME)).class(dim()).into(),
        }];
        let mut meters = 0;
        for meter in self.meters() {
            let standing = meter.standing(now);
            if meters > 0 {
                parts.push(label(applet.text("·")).class(dim()).into());
            }
            meters += 1;
            parts.push(label(applet.text(meter.short())).class(accent()).into());
            let text = label(applet.text(used(&standing)));
            parts.push(match severity(&standing) {
                Some(class) => text.class(class).into(),
                None => text.into(),
            });
            if let Some(left) = standing.left {
                parts.push(glyph("view-refresh-symbolic", Color { a: 0.6, ..on_bg() }));
                parts.push(label(applet.text(brief(left))).class(dim()).into());
            }
            if let Some(out_in) = standing.out_in {
                parts.push(glyph("dialog-warning-symbolic", warning_color()));
                parts.push(label(applet.text(brief(out_in))).class(warning()).into());
            }
        }
        if meters == 0 {
            match &self.error {
                Some(problem) if let Some(brief) = problem.brief() => {
                    parts.push(glyph("dialog-warning-symbolic", warning_color()));
                    parts.push(label(applet.text(brief)).class(warning()).into());
                }
                error => {
                    let waiting = if error.is_some() { "?" } else { "…" };
                    parts.push(label(applet.text(waiting)).class(dim()).into());
                }
            }
        }

        let (major, minor) = applet.suggested_padding(true);
        let (horizontal, vertical) = if applet.is_horizontal() {
            (major, minor)
        } else {
            (minor, major)
        };
        let height = applet.suggested_size(true).1 + 2 * vertical;
        let button = widget::button::custom(
            widget::layer_container(
                widget::Row::with_children(parts)
                    .spacing(4)
                    .align_y(Alignment::Center),
            )
            .center_y(Length::Fixed(f32::from(height))),
        )
        .padding([0, horizontal])
        .class(cosmic::theme::Button::AppletIcon)
        .on_press_with_rectangle(|offset, bounds| {
            Message::Toggle(Rectangle {
                x: bounds.x - offset.x,
                y: bounds.y - offset.y,
                ..bounds
            })
        });

        applet
            .autosize_window(applet.applet_tooltip::<Message>(
                button,
                format!("{} usage", S::NAME),
                self.popup.is_some(),
                Message::Surface,
                None,
            ))
            .into()
    }

    fn view_window(&self, _id: Id) -> Element<'_, Message> {
        widget::text("").into()
    }

    fn style(&self) -> Option<cosmic::iced::theme::Style> {
        Some(cosmic::applet::style())
    }
}

impl<S: Source> Panel<S> {
    fn meters(&self) -> impl Iterator<Item = crate::usage::Meter> + '_ {
        self.usage
            .iter()
            .flat_map(|usage| usage.meters.iter().copied())
    }

    fn ask(&mut self, force: bool) -> Task<Message> {
        if self.asking {
            return Task::none();
        }
        self.asking = true;
        let (sender, answer) = oneshot::channel();
        let asks = self.asks.clone();
        std::thread::spawn(move || {
            let _ = sender.send(asks.usage(force, S::usage));
        });
        cosmic::task::future(async move {
            Message::Usage(
                answer
                    .await
                    .unwrap_or_else(|_| Err(Problem::Failed("no answer".to_string()))),
            )
        })
    }

    /// Each window: how much is used, a bar marked where an even pace would be, when it
    /// resets, and when it runs out first at the pace so far.
    fn popup_view(&self) -> Element<'_, Message> {
        let now = now();
        let mut rows: Vec<Element<Message>> = Vec::new();
        let help = match &self.error {
            Some(Problem::LoggedOut) => Some(S::LOGGED_OUT),
            Some(Problem::NotInstalled) => Some(S::NOT_INSTALLED),
            _ => None,
        };
        if let Some((heading, body)) = help {
            rows.push(widget::text::heading(heading).class(warning()).into());
            rows.push(widget::text::body(body).into());
            rows.push(widget::space::vertical().height(Length::Fixed(6.0)).into());
        }
        for meter in self.meters() {
            let standing = meter.standing(now);
            let percent = widget::text(used(&standing));
            rows.push(
                widget::Row::with_children(vec![
                    widget::text::heading(meter.label()).into(),
                    widget::space::horizontal().into(),
                    match severity(&standing) {
                        Some(class) => percent.class(class).into(),
                        None => percent.into(),
                    },
                ])
                .into(),
            );
            rows.push(bar(&standing));
            rows.push(
                widget::text::caption(standing.left.map_or_else(
                    || {
                        let ago = span(now.saturating_sub(meter.window.resets_at));
                        format!("Reset {ago} ago, not measured since")
                    },
                    |left| format!("Resets in {}", span(left)),
                ))
                .class(dim())
                .into(),
            );
            if let Some(out_in) = standing.out_in {
                rows.push(
                    widget::text::caption(format!("At this pace, it runs out in {}", span(out_in)))
                        .class(warning())
                        .into(),
                );
            }
            rows.push(widget::space::vertical().height(Length::Fixed(6.0)).into());
        }
        let status = match (self.asking, &self.error, self.usage_at) {
            (true, ..) => "Updating…".to_string(),
            (false, Some(Problem::Failed(error)), _) => format!("Could not update: {error}"),
            (false, Some(_), _) => String::new(),
            (false, None, Some(at)) => match now.saturating_sub(at) / 60 {
                0 => "Updated just now".to_string(),
                minutes => format!("Updated {minutes} min ago"),
            },
            (false, None, None) => String::new(),
        };
        rows.push(
            widget::Row::with_children(vec![
                widget::text::caption(status)
                    .class(dim())
                    .width(Length::Fill)
                    .into(),
                widget::button::icon(widget::icon::from_name("view-refresh-symbolic"))
                    .extra_small()
                    .on_press_maybe((!self.asking).then_some(Message::RefreshNow))
                    .into(),
            ])
            .align_y(Alignment::Center)
            .into(),
        );
        widget::container(widget::Column::with_children(rows).spacing(4))
            .padding(12)
            .width(Length::Fill)
            .into()
    }
}

/// The first of `names` the icon theme or `/usr/share/pixmaps` has.
fn logo(names: &[&str]) -> Option<widget::icon::Handle> {
    names
        .iter()
        .find_map(|&name| {
            let mut named = widget::icon::from_name(name).size(64);
            // Its default would try `claude` for `claude-desktop`, and so on.
            named.fallback = None;
            named.path().or_else(|| {
                ["png", "svg"]
                    .map(|extension| {
                        PathBuf::from(format!("/usr/share/pixmaps/{name}.{extension}"))
                    })
                    .into_iter()
                    .find(|path| path.is_file())
            })
        })
        .map(widget::icon::from_path)
}

/// How much of a window is used, unknown once the last measurement has outlived it.
fn used(standing: &Standing) -> String {
    match standing.left {
        Some(_) => format!("{:.0}%", standing.used * 100.0),
        None => "?".to_string(),
    }
}

/// A duration short enough for the panel: "37m", "3h10", "3d14h".
fn brief(seconds: u64) -> String {
    let minutes = seconds.div_ceil(60);
    let (days, hours, minutes) = (minutes / 1440, minutes / 60 % 24, minutes % 60);
    match (days, hours) {
        (0, 0) => format!("{minutes}m"),
        (0, _) => format!("{hours}h{minutes:02}"),
        _ => format!("{days}d{hours}h"),
    }
}

fn glyph<'a>(name: &'static str, color: Color) -> Element<'a, Message> {
    widget::icon::from_name(name)
        .symbolic(true)
        .size(12)
        .icon()
        .class(cosmic::theme::Svg::Custom(Rc::new(move |_| svg::Style {
            color: Some(color),
        })))
        .into()
}

fn on_bg() -> Color {
    cosmic::theme::active().cosmic().on_bg_color().into()
}

fn label(text: widget::Text<'_, cosmic::Theme>) -> widget::Text<'_, cosmic::Theme> {
    text.wrapping(Wrapping::None)
}

fn surface(action: cosmic::surface::Action<Message>) -> Task<Message> {
    cosmic::task::message(cosmic::Action::Surface(action))
}

fn notify(name: &'static str, label: String, out_in: u64) {
    std::thread::spawn(move || {
        let sent = Command::new("notify-send")
            .arg(format!("--app-name={name} usage"))
            .arg("--icon=dialog-warning-symbolic")
            .arg(format!("{name}: {label} usage is ahead of pace"))
            .arg(format!("At this pace, it runs out in {}.", span(out_in)))
            .status();
        if let Err(error) = sent {
            eprintln!("claudo-applet: cannot run notify-send: {error}");
        }
    });
}

/// The warning color for a window over pace or past 80%, the destructive one past 95%.
fn severity(standing: &Standing) -> Option<cosmic::theme::Text> {
    if standing.used >= 0.95 {
        Some(cosmic::theme::Text::Color(
            cosmic::theme::active().cosmic().destructive_color().into(),
        ))
    } else if standing.used >= 0.8 || standing.out_in.is_some() {
        Some(warning())
    } else {
        None
    }
}

fn accent() -> cosmic::theme::Text {
    cosmic::theme::Text::Color(cosmic::theme::active().cosmic().accent_color().into())
}

fn warning_color() -> Color {
    cosmic::theme::active().cosmic().warning_color().into()
}

fn warning() -> cosmic::theme::Text {
    cosmic::theme::Text::Color(warning_color())
}

fn dim() -> cosmic::theme::Text {
    cosmic::theme::Text::Color(Color { a: 0.6, ..on_bg() })
}

/// A thin bar, in the accent color, the warning one past 80% and the destructive one past
/// 95%, marked where an even pace would be.
fn bar<'a>(standing: &Standing) -> Element<'a, Message> {
    let used = standing.used;
    let filled = (used * 1000.0).round() as u16;
    let rounded = |theme: &cosmic::Theme, color: Color| container::Style {
        background: Some(color.into()),
        border: Border {
            radius: theme.cosmic().corner_radii.radius_xl.into(),
            ..Border::default()
        },
        ..container::Style::default()
    };
    let fill = widget::container(widget::space::horizontal())
        .width(Length::FillPortion(filled.max(1)))
        .height(Length::Fixed(4.0))
        .class(cosmic::theme::Container::custom(move |theme| {
            let cosmic = theme.cosmic();
            let color = if used >= 0.95 {
                cosmic.destructive_color()
            } else if used >= 0.8 {
                cosmic.warning_color()
            } else {
                cosmic.accent_color()
            };
            rounded(theme, color.into())
        }));
    let portions = |at: u16, middle: Element<'a, Message>| {
        let mut parts = vec![];
        if at > 0 {
            parts.push(
                widget::space::horizontal()
                    .width(Length::FillPortion(at))
                    .into(),
            );
        }
        parts.push(middle);
        if at < 1000 {
            parts.push(
                widget::space::horizontal()
                    .width(Length::FillPortion(1000 - at))
                    .into(),
            );
        }
        parts
    };
    let mut parts: Vec<Element<Message>> = vec![];
    if filled > 0 {
        parts.push(fill.into());
    }
    if filled < 1000 {
        parts.push(
            widget::space::horizontal()
                .width(Length::FillPortion(1000 - filled))
                .into(),
        );
    }
    let bar: Element<Message> = widget::container(widget::Row::with_children(parts))
        .width(Length::Fill)
        .height(Length::Fixed(4.0))
        .class(cosmic::theme::Container::custom(move |theme| {
            rounded(theme, theme.cosmic().bg_divider().into())
        }))
        .into();
    let Some(even) = standing.even else {
        return bar;
    };
    let mark = widget::container(widget::space::horizontal())
        .width(Length::Fixed(2.0))
        .height(Length::Fixed(10.0))
        .class(cosmic::theme::Container::custom(|theme| container::Style {
            background: Some(Color::from(theme.cosmic().on_bg_color()).into()),
            ..container::Style::default()
        }));
    cosmic::iced::widget::Stack::with_children(vec![
        widget::container(bar).center_y(Length::Fixed(10.0)).into(),
        widget::Row::with_children(portions((even * 1000.0).round() as u16, mark.into())).into(),
    ])
    .width(Length::Fill)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_durations_fit_a_few_characters() {
        assert_eq!(brief(30), "1m");
        assert_eq!(brief(37 * 60), "37m");
        assert_eq!(brief(3 * 3600 + 5 * 60), "3h05");
        assert_eq!(brief(3 * 86_400 + 14 * 3600), "3d14h");
    }
}
