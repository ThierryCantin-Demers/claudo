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
        let windows = self
            .quota
            .map_or([None, None], |quota| [quota.five_hour, quota.seven_day]);
        let mut rows: Vec<Element<Message>> = ["Session", "Week"]
            .into_iter()
            .zip(windows)
            .filter_map(|(label, window)| Some(usage_row(label, window?, now, size)))
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

    /// Each window on a line of its own: how much is used and when it resets.
    pub(super) fn header_usage(&self, w: window::Id) -> Option<Element<'_, Message>> {
        const SIZE: f32 = 11.0;
        // Narrower, they would run under the title or the context.
        const ROOM: f32 = 660.0;
        let layout = self.window(w);
        if layout.sidebar_open || layout.size.is_some_and(|(width, _)| width < ROOM) {
            return None;
        }
        let now = now();
        let quota = self.quota?;
        let lines: Vec<Element<Message>> =
            [("Session", quota.five_hour), ("Week", quota.seven_day)]
                .into_iter()
                .filter_map(|(label, window)| {
                    let (used, left) = standing(window?, now);
                    let resets = left.map_or("not started".to_string(), span);
                    Some(
                        widget::Row::with_children(vec![
                            widget::text(label)
                                .size(SIZE)
                                .width(Length::Fixed(44.0))
                                .into(),
                            widget::container(meter(used))
                                .width(Length::Fixed(48.0))
                                .into(),
                            widget::container(
                                widget::text(format!("{:.0}%", used * 100.0))
                                    .size(SIZE)
                                    .class(dim()),
                            )
                            .align_right(Length::Fixed(32.0))
                            .into(),
                            widget::text(resets).size(SIZE).class(dim()).into(),
                        ])
                        .spacing(6)
                        .align_y(Alignment::Center)
                        .into(),
                    )
                })
                .collect();
        if lines.is_empty() {
            return None;
        }
        Some(
            widget::tooltip(
                widget::container(widget::Column::with_children(lines)).padding([0, 8]),
                widget::text(format!("Time left until each resets\n{}", self.status(now))),
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

/// How much of `window` is used, and how long until it resets.
fn standing(window: QuotaWindow, now: u64) -> (f32, Option<u64>) {
    // Past its reset the window starts over, though no report has said so yet.
    match window.resets_at.checked_sub(now) {
        Some(left) if left > 0 => (window.used.clamp(0.0, 1.0), Some(left)),
        _ => (0.0, None),
    }
}

fn resets(left: Option<u64>) -> String {
    left.map_or("Starts on your next message".to_string(), |left| {
        format!("Resets in {}", span(left))
    })
}

fn usage_row<'a>(label: &'a str, window: QuotaWindow, now: u64, size: f32) -> Element<'a, Message> {
    let (used, left) = standing(window, now);
    let percent = format!("{:.0}%", used * 100.0);
    widget::Column::with_children(vec![
        widget::Row::with_children(vec![
            widget::text(label).size(size).into(),
            widget::space::horizontal().into(),
            widget::text(percent).size(size).class(dim()).into(),
        ])
        .into(),
        meter(used),
        widget::text(resets(left))
            .size(size * 0.9)
            .class(dim())
            .into(),
    ])
    .spacing(4)
    .into()
}

/// A thin bar, filled in the accent color, in the warning color past 80% and the
/// destructive one past 95%.
fn meter<'a>(used: f32) -> Element<'a, Message> {
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
    widget::container(widget::Row::with_children(parts))
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

    #[test]
    fn reset_spans_read_at_a_glance() {
        assert_eq!(span(30), "1 min");
        assert_eq!(span(45 * 60), "45 min");
        assert_eq!(span(4 * 3600 + 35 * 60), "4 h 35 min");
        assert_eq!(span(2 * 86_400 + 13 * 3600 + 59), "2 d 13 h");
    }
}
