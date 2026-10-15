//! Compaction in a conversation: the row marking where it happened, and the offer to
//! compact once the context has grown long.

use super::*;
use claudo_protocol::Compaction;

/// Context, in tokens, past which a conversation offers to compact itself.
pub(super) const COMPACT_AT: u64 = 500_000;

/// Where the conversation was summarized, and by how much; expands to the summary.
pub(super) fn compaction_row<'a>(
    on_toggle: Message,
    compaction: &Compaction,
    summary: Option<Selectable<'a>>,
    size: f32,
) -> Element<'a, Message> {
    let chevron = if summary.is_some() {
        "go-down-symbolic"
    } else {
        "go-next-symbolic"
    };
    let mut sizes = format!(
        "{} → {} tokens",
        compact(compaction.before),
        compact(compaction.after)
    );
    if compaction.auto {
        sizes.push_str(" · automatically");
    }
    let header = widget::button::custom(
        widget::Row::with_children(vec![
            widget::icon::from_name(chevron).size(16).into(),
            widget::text("Conversation compacted")
                .size(size)
                .font(cosmic::font::semibold())
                .into(),
            widget::text(sizes)
                .size(size)
                .class(dim())
                .wrapping(text::Wrapping::None)
                .width(Length::Fill)
                .into(),
        ])
        .spacing(8)
        .align_y(Alignment::Center),
    )
    .on_press(on_toggle)
    .class(cosmic::theme::Button::Text)
    .width(Length::Fill);
    let Some(summary) = summary else {
        return header.into();
    };
    widget::Column::with_children(vec![
        header.into(),
        widget::container(summary).padding([4, 12, 8, 32]).into(),
    ])
    .spacing(2)
    .into()
}

impl ClaudoApp {
    /// Above the message box once the context has grown long: what compacting does, and a
    /// button for it.
    pub(super) fn compact_offer<'a>(
        &'a self,
        id: &'a ConversationId,
        view: &ConversationView,
    ) -> Option<Element<'a, Message>> {
        let used = self.info(id)?.context.used;
        if used < COMPACT_AT || view.streaming {
            return None;
        }
        let size = 13.0 * self.zoom;
        let offer = widget::container(
            widget::Row::with_children(vec![
                widget::text(format!(
                    "This conversation's context is {} tokens. Compacting summarizes it to \
                     make room.",
                    compact(used)
                ))
                .size(size)
                .width(Length::Fill)
                .into(),
                widget::button::suggested("Compact")
                    .on_press(Message::Compact(id.clone()))
                    .into(),
            ])
            .spacing(12)
            .align_y(Alignment::Center),
        )
        .padding([8, 14])
        .width(Length::Fill)
        .class(cosmic::theme::Container::Card);
        Some(
            widget::container(widget::container(offer).max_width(COLUMN_WIDTH * self.zoom))
                .center_x(Length::Fill)
                .padding([0, 16])
                .into(),
        )
    }
}
