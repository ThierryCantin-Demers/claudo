//! The subscription's usage windows, at the foot of the sidebar, or in the header bar of a
//! window whose sidebar is closed.

use std::time::{SystemTime, UNIX_EPOCH};

use super::*;
use claudo_protocol::{Quota, QuotaWindow};

/// Usage is fetched this often when no turn reports it.
pub(super) const USAGE_REFRESH_MINUTES: u32 = 10;

/// A turn's report may carry one window; keep the other from before.
pub(super) fn merged(old: Option<Quota>, new: Quota) -> Quota {
    let old = old.unwrap_or_default();
    Quota {
        five_hour: new.five_hour.or(old.five_hour),
        seven_day: new.seven_day.or(old.seven_day),
    }
}

pub(super) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl ClaudoApp {
    pub(super) fn usage(&self) -> Option<Element<'_, Message>> {
        let now = now();
        let size = 12.0 * self.zoom;
        let mut rows: Vec<Element<Message>> = windows(self.quota)
            .into_iter()
            .filter_map(|(label, window, length)| {
                Some(usage_row(label, standing(window?, length, now), size))
            })
            .collect();
        if rows.is_empty() && self.quota_error.is_none() {
            return None;
        }
        rows.push(self.freshness(now, size));
        Some(
            widget::container(widget::Column::with_children(rows).spacing(10))
                .padding([4, 8])
                .into(),
        )
    }

    /// Each window on a line of its own: how much is used, when it resets, and whether
    /// the pace so far would use it up before then.
    pub(super) fn header_usage(&self, w: window::Id) -> Option<Element<'_, Message>> {
        const SIZE: f32 = 11.0;
        let layout = self.window(w);
        // Narrower, they would squeeze the title, or run under the context and buttons.
        let titled = layout.page != Page::Chat || layout.tiles.len() == 1;
        let room = if titled { 760.0 } else { 520.0 };
        if layout.sidebar_open || layout.size.is_some_and(|(width, _)| width < room) {
            return None;
        }
        let now = now();
        let mut tip = vec!["Time left until each resets".to_string()];
        let lines: Vec<Element<Message>> = windows(self.quota)
            .into_iter()
            .filter_map(|(label, window, length)| {
                let standing = standing(window?, length, now);
                let mut line = vec![
                    widget::text(label)
                        .size(SIZE)
                        .width(Length::Fixed(44.0))
                        .into(),
                    widget::container(meter(&standing))
                        .width(Length::Fixed(48.0))
                        .into(),
                    widget::container(
                        widget::text(format!("{:.0}%", standing.used * 100.0))
                            .size(SIZE)
                            .class(dim()),
                    )
                    .align_right(Length::Fixed(32.0))
                    .into(),
                    widget::text(standing.left.map_or("not started".to_string(), span))
                        .size(SIZE)
                        .class(dim())
                        .into(),
                ];
                if let Some(out_in) = standing.out_in {
                    tip.push(format!("{label}: {}", pace(out_in)));
                    line.push(widget::text("slow down").size(SIZE).class(warning()).into());
                }
                Some(
                    widget::Row::with_children(line)
                        .spacing(6)
                        .align_y(Alignment::Center)
                        .into(),
                )
            })
            .collect();
        if lines.is_empty() {
            return None;
        }
        tip.push(self.status(now));
        Some(
            widget::tooltip(
                widget::container(widget::Column::with_children(lines)).padding([0, 8]),
                widget::text(tip.join("\n")),
                widget::tooltip::Position::Bottom,
            )
            .into(),
        )
    }

    fn status(&self, now: u64) -> String {
        match (self.quota_asked, &self.quota_error, self.quota_at) {
            (true, ..) => "Updating…".to_string(),
            (false, Some(_), _) => "Could not update".to_string(),
            (false, None, Some(at)) => format!("Updated {}", time::ago(now.saturating_sub(at))),
            (false, None, None) => String::new(),
        }
    }

    /// When the windows were last reported, and a button to ask again.
    fn freshness(&self, now: u64, size: f32) -> Element<'_, Message> {
        // Fill, so the button keeps its size and a narrow sidebar wraps the text instead.
        let status = widget::text(self.status(now))
            .size(size * 0.9)
            .class(dim())
            .width(Length::Fill);
        let status: Element<Message> = match &self.quota_error {
            Some(error) if !self.quota_asked => widget::tooltip(
                status,
                widget::text(error.as_str()),
                widget::tooltip::Position::Top,
            )
            .into(),
            _ => status.into(),
        };
        widget::Row::with_children(vec![
            status,
            widget::tooltip(
                widget::button::icon(widget::icon::from_name("view-refresh-symbolic"))
                    .extra_small()
                    .on_press_maybe((!self.quota_asked).then_some(Message::RefreshUsage)),
                widget::text("Refresh"),
                widget::tooltip::Position::Top,
            )
            .into(),
        ])
        .align_y(Alignment::Center)
        .into()
    }
}

/// Each window with its label and its length in seconds.
fn windows(quota: Option<Quota>) -> [(&'static str, Option<QuotaWindow>, u64); 2] {
    let quota = quota.unwrap_or_default();
    [
        ("Session", quota.five_hour, 5 * 3600),
        ("Week", quota.seven_day, 7 * 86_400),
    ]
}

/// Where a window stands.
#[derive(Debug, PartialEq)]
struct Standing {
    used: f32,
    /// Until it resets, or None once past it, until a report says it started again.
    left: Option<u64>,
    /// How much an even pace would have used by now.
    even: Option<f32>,
    /// Until it runs out at the pace so far, when that comes before its reset.
    out_in: Option<u64>,
}

fn standing(window: QuotaWindow, length: u64, now: u64) -> Standing {
    let Some(left) = window.resets_at.checked_sub(now).filter(|&left| left > 0) else {
        return Standing {
            used: 0.0,
            left: None,
            even: None,
            out_in: None,
        };
    };
    let used = window.used.clamp(0.0, 1.0);
    let elapsed = length.saturating_sub(left);
    let even = elapsed as f32 / length as f32;
    // A turn or two in a window's first minutes says nothing yet of its pace.
    let judged = used >= 0.1 || even >= 0.1;
    let out_in = (judged && elapsed > 0 && used > 0.0 && used < 1.0)
        .then(|| ((1.0 - used) / used * elapsed as f32) as u64)
        .filter(|&out_in| out_in < left);
    Standing {
        used,
        left: Some(left),
        even: Some(even),
        out_in,
    }
}

fn pace(out_in: u64) -> String {
    format!("at this pace, it runs out in {}", span(out_in))
}

fn warning() -> cosmic::theme::Text {
    cosmic::theme::Text::Color(cosmic::theme::active().cosmic().warning_color().into())
}

fn usage_row<'a>(label: &'a str, standing: Standing, size: f32) -> Element<'a, Message> {
    let percent = format!("{:.0}%", standing.used * 100.0);
    let resets = standing
        .left
        .map_or("Starts on your next message".to_string(), |left| {
            format!("Resets in {}", span(left))
        });
    let mut rows = vec![
        widget::Row::with_children(vec![
            widget::text(label).size(size).into(),
            widget::space::horizontal().into(),
            widget::text(percent).size(size).class(dim()).into(),
        ])
        .into(),
        meter(&standing),
        widget::text(resets).size(size * 0.9).class(dim()).into(),
    ];
    if let Some(out_in) = standing.out_in {
        let mut pace = pace(out_in);
        pace[..1].make_ascii_uppercase();
        rows.push(widget::text(pace).size(size * 0.9).class(warning()).into());
    }
    widget::Column::with_children(rows).spacing(4).into()
}

/// A thin bar, filled in the accent color, in the warning color past 80% and the
/// destructive one past 95%, marked where an even pace would be.
fn meter<'a>(standing: &Standing) -> Element<'a, Message> {
    let used = standing.used;
    let filled = (used * 1000.0).round() as u16;
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
            container::Style {
                background: Some(Color::from(color).into()),
                border: Border {
                    radius: cosmic.corner_radii.radius_xl.into(),
                    ..Border::default()
                },
                ..container::Style::default()
            }
        }));
    let mut parts = vec![];
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
        .class(cosmic::theme::Container::custom(|theme| {
            let cosmic = theme.cosmic();
            container::Style {
                background: Some(Color::from(cosmic.bg_divider()).into()),
                border: Border {
                    radius: cosmic.corner_radii.radius_xl.into(),
                    ..Border::default()
                },
                ..container::Style::default()
            }
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
    let at = (even * 1000.0).round() as u16;
    let mut marked = vec![];
    if at > 0 {
        marked.push(
            widget::space::horizontal()
                .width(Length::FillPortion(at))
                .into(),
        );
    }
    marked.push(mark.into());
    if at < 1000 {
        marked.push(
            widget::space::horizontal()
                .width(Length::FillPortion(1000 - at))
                .into(),
        );
    }
    cosmic::iced::widget::Stack::with_children(vec![
        widget::container(bar).center_y(Length::Fixed(10.0)).into(),
        widget::Row::with_children(marked).into(),
    ])
    .width(Length::Fill)
    .into()
}

/// How long until a reset, to the minute: "45 min", "4 h 35 min", "2 d 13 h".
fn span(seconds: u64) -> String {
    let minutes = seconds.div_ceil(60);
    let (days, hours, minutes) = (minutes / 1440, minutes / 60 % 24, minutes % 60);
    match (days, hours) {
        (0, 0) => format!("{minutes} min"),
        (0, _) => format!("{hours} h {minutes} min"),
        _ => format!("{days} d {hours} h"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: u64 = 3600;
    const SESSION: u64 = 5 * HOUR;

    fn session(used: f32, left: u64) -> Standing {
        let now = 1_000_000;
        standing(
            QuotaWindow {
                used,
                resets_at: now + left,
            },
            SESSION,
            now,
        )
    }

    #[test]
    fn a_window_used_faster_than_it_passes_runs_out_before_its_reset() {
        let ahead = session(0.5, 4 * HOUR);
        assert_eq!(ahead.even, Some(0.2));
        assert_eq!(
            ahead.out_in,
            Some(HOUR),
            "half in an hour, the rest in another"
        );
        assert_eq!(
            session(0.4, SESSION / 2).out_in,
            None,
            "behind an even pace"
        );
        assert_eq!(
            session(0.05, SESSION - 120).out_in,
            None,
            "too early to judge"
        );
        assert_eq!(session(1.0, HOUR).out_in, None, "already used up");
        assert_eq!(
            session(0.5, 0),
            Standing {
                used: 0.0,
                left: None,
                even: None,
                out_in: None
            },
            "a window past its reset starts over"
        );
    }

    #[test]
    fn reset_spans_read_at_a_glance() {
        assert_eq!(span(30), "1 min");
        assert_eq!(span(45 * 60), "45 min");
        assert_eq!(span(4 * 3600 + 35 * 60), "4 h 35 min");
        assert_eq!(span(2 * 86_400 + 13 * 3600 + 59), "2 d 13 h");
    }
}
