//! Claude Code sessions started in Claude Desktop or a terminal, which a click continues
//! as a conversation here.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use super::*;

impl ClaudoApp {
    pub(super) fn sessions_page(&self) -> Element<'_, Message> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let note = |text| widget::text(text).class(dim()).into();
        let list: Vec<Element<Message>> = match &self.sessions {
            None => vec![note("Looking for sessions…")],
            Some(sessions) if sessions.is_empty() => {
                vec![note(
                    "Every Claude Code session already has a conversation here.",
                )]
            }
            Some(sessions) => sessions.iter().map(|s| self.session_row(s, now)).collect(),
        };
        let page = widget::Column::with_children(vec![
            widget::button::text("Back to chat")
                .leading_icon(widget::icon::from_name("go-previous-symbolic"))
                .on_press(Message::SetPage(Page::Chat))
                .into(),
            widget::text::title3("Claude Code sessions").into(),
            note(
                "Sessions started in Claude Desktop or a terminal. One continues here as a \
                 copy: its first message forks it, and the original stays as it was.",
            ),
            widget::Column::with_children(list).spacing(2).into(),
        ])
        .spacing(16)
        .padding([24, 16])
        .max_width(COLUMN_WIDTH * self.zoom);
        widget::scrollable(widget::container(page).center_x(Length::Fill))
            .height(Length::Fill)
            .into()
    }

    fn session_row<'a>(&self, session: &'a SessionInfo, now: u64) -> Element<'a, Message> {
        let line = |text: String, size: f32| {
            widget::text(text)
                .size(size * self.zoom)
                .wrapping(text::Wrapping::None)
                .ellipsize(Ellipsize::End(EllipsizeHeightLimit::Lines(1)))
                .width(Length::Fill)
        };
        let detail = format!(
            "{} · {} · {}",
            session.source,
            ago(now.saturating_sub(session.updated)),
            home_relative(&session.cwd)
        );
        let radius = cosmic::theme::active().cosmic().corner_radii.radius_s;
        widget::button::custom(
            widget::Column::with_children(vec![
                line(session.title.clone(), 14.0)
                    .font(cosmic::font::semibold())
                    .into(),
                line(detail, 12.0).class(dim()).into(),
            ])
            .spacing(2),
        )
        .class(conversation_style(radius, false))
        .on_press(Message::Import(session.session_id.clone()))
        .padding([6, 10])
        .width(Length::Fill)
        .into()
    }
}

fn home_relative(path: &str) -> String {
    let home = std::env::var_os("HOME").unwrap_or_default();
    match Path::new(path).strip_prefix(&home) {
        Ok(rest) if !home.is_empty() => Path::new("~").join(rest).display().to_string(),
        _ => path.to_string(),
    }
}

/// How long ago, in the one unit that reads at a glance.
fn ago(seconds: u64) -> String {
    match (seconds / 86_400, seconds / 3600, seconds / 60) {
        (0, 0, 0) => "just now".to_string(),
        (0, 0, minutes) => format!("{minutes} min ago"),
        (0, hours, _) => format!("{hours} h ago"),
        (1, _, _) => "yesterday".to_string(),
        (days, _, _) => format!("{days} days ago"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages_read_at_a_glance() {
        assert_eq!(ago(30), "just now");
        assert_eq!(ago(5 * 60), "5 min ago");
        assert_eq!(ago(3 * 3600 + 59), "3 h ago");
        assert_eq!(ago(30 * 3600), "yesterday");
        assert_eq!(ago(9 * 86_400), "9 days ago");
    }
}
