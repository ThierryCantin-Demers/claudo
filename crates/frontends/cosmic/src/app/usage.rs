//! The subscription's usage windows, at the foot of the sidebar.

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

impl ClaudoApp {
    pub(super) fn usage(&self) -> Option<Element<'_, Message>> {
        let quota = self.quota?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let size = 12.0 * self.zoom;
        let rows: Vec<Element<Message>> = [("Session", quota.five_hour), ("Week", quota.seven_day)]
            .into_iter()
            .filter_map(|(label, window)| Some(usage_row(label, window?, now, size)))
            .collect();
        (!rows.is_empty()).then(|| {
            widget::container(widget::Column::with_children(rows).spacing(10))
                .padding([4, 8])
                .into()
        })
    }
}

fn usage_row<'a>(label: &'a str, window: QuotaWindow, now: u64, size: f32) -> Element<'a, Message> {
    // Past its reset the window starts over, though no report has said so yet.
    let (used, left) = match window.resets_at.checked_sub(now) {
        Some(left) if left > 0 => (window.used.clamp(0.0, 1.0), Some(left)),
        _ => (0.0, None),
    };
    let percent = format!("{:.0}%", used * 100.0);
    let resets = left.map_or("Starts on your next message".to_string(), |left| {
        format!("Resets in {}", span(left))
    });
    widget::Column::with_children(vec![
        widget::Row::with_children(vec![
            widget::text(label).size(size).into(),
            widget::space::horizontal().into(),
            widget::text(percent).size(size).class(dim()).into(),
        ])
        .into(),
        meter(used),
        widget::text(resets).size(size * 0.9).class(dim()).into(),
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
