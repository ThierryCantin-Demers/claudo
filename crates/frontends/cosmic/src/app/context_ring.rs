//! How full a conversation's context is, as a ring by the model it runs on.

use std::f32::consts::PI;

use claudo_protocol::ContextUsage;
use cosmic::iced::widget::canvas::{self, Frame, Path, Stroke, path::Arc};
use cosmic::iced::{Radians, Rectangle, mouse};

use super::*;

/// The share of the context window used, from 0 to 1.
struct Ring(f32);

impl canvas::Program<Message, cosmic::Theme, cosmic::Renderer> for Ring {
    type State = ();

    fn draw(
        &self,
        _state: &(),
        renderer: &cosmic::Renderer,
        theme: &cosmic::Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<canvas::Geometry<cosmic::Renderer>> {
        let cosmic = theme.cosmic();
        let mut frame = Frame::new(renderer, bounds.size());
        let width = (bounds.width / 6.0).max(2.0);
        let radius = (bounds.width.min(bounds.height) - width) / 2.0;
        frame.stroke(
            &Path::circle(frame.center(), radius),
            Stroke::default()
                .with_color(Color::from(cosmic.bg_divider()))
                .with_width(width),
        );
        // Yellow from half full, since every turn resends all of it, and red where Claude
        // Code soon compacts on its own.
        let color = match self.0 {
            share if share >= 0.8 => cosmic.destructive_color(),
            share if share >= 0.5 => cosmic.warning_color(),
            _ => cosmic.success_color(),
        };
        let filled = Path::new(|path| {
            path.arc(Arc {
                center: frame.center(),
                radius,
                start_angle: Radians(-PI / 2.0),
                end_angle: Radians(-PI / 2.0 + 2.0 * PI * self.0.max(0.01)),
            });
        });
        frame.stroke(
            &filled,
            Stroke::default()
                .with_color(Color::from(color))
                .with_width(width)
                .with_line_cap(canvas::LineCap::Round),
        );
        vec![frame.into_geometry()]
    }
}

/// The ring, with the numbers on hover, once a turn has reported the window.
pub(super) fn ring<'a>(context: &ContextUsage, size: f32) -> Option<Element<'a, Message>> {
    let window = context.window.filter(|_| context.used > 0)?;
    let percent = context.percent()?;
    let ring = canvas::Canvas::new(Ring(percent / 100.0))
        .width(Length::Fixed(size))
        .height(Length::Fixed(size));
    Some(
        widget::tooltip(
            widget::container(ring).padding([0, 4]),
            widget::text(format!(
                "Context: {} of {} tokens ({percent:.0}%)",
                compact(context.used),
                compact(window)
            )),
            widget::tooltip::Position::Top,
        )
        .into(),
    )
}
