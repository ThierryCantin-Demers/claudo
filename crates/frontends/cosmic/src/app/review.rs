//! A code review's findings as a list to scan: each one's rank, place and statement, with
//! what makes it go wrong folded under it.

use super::*;
use claudo_protocol::Review;

pub(super) struct Shown {
    preface: markdown::Content,
    pub findings: Vec<Finding>,
    afterword: markdown::Content,
}

pub(super) struct Finding {
    /// The folder, then the file and line, so the file stands out.
    folder: String,
    file: String,
    summary: markdown::Content,
    scenario: Option<markdown::Content>,
    pub open: bool,
}

impl Shown {
    pub fn new(review: Review) -> Self {
        Self {
            preface: parsed(&review.preface),
            findings: review
                .findings
                .into_iter()
                .map(|finding| {
                    let (folder, file) = match finding.file.rsplit_once('/') {
                        Some((folder, file)) => (format!("{folder}/"), file.to_string()),
                        None => (String::new(), finding.file),
                    };
                    Finding {
                        folder,
                        file: match finding.line {
                            Some(line) => format!("{file}:{line}"),
                            None => file,
                        },
                        summary: parsed(&finding.summary),
                        scenario: finding.failure_scenario.as_deref().map(parsed),
                        open: false,
                    }
                })
                .collect(),
            afterword: parsed(&review.afterword),
        }
    }

    /// Opens every failure scenario, or closes them all once they are.
    pub fn toggle_all(&mut self) {
        let open = self.findings.iter().any(|finding| !finding.open);
        for finding in &mut self.findings {
            finding.open = open;
        }
    }

    pub fn view<'a>(
        &'a self,
        viewer: &selectable::Viewer<'a>,
        settings: markdown::Settings,
        toggle: impl Fn(Option<usize>) -> Message,
        size: f32,
        zoom: f32,
    ) -> Element<'a, Message> {
        let small = size * 0.85;
        let all_open = self.findings.iter().all(|finding| finding.open);
        let count = match self.findings.len() {
            1 => "1 finding".to_string(),
            n => format!("{n} findings"),
        };
        let mut header = vec![
            widget::text(count)
                .size(size)
                .font(cosmic::font::semibold())
                .into(),
        ];
        if self.findings.len() > 1 {
            header.push(
                widget::text("most severe first")
                    .size(small)
                    .class(dim())
                    .into(),
            );
        }
        header.push(widget::space::horizontal().into());
        if self
            .findings
            .iter()
            .any(|finding| finding.scenario.is_some())
        {
            header.push(
                widget::button::text(if all_open {
                    "Hide scenarios"
                } else {
                    "Show scenarios"
                })
                .on_press(toggle(None))
                .into(),
            );
        }
        let mut rows: Vec<Element<'a, Message>> = vec![
            widget::container(
                widget::Row::with_children(header)
                    .spacing(8)
                    .align_y(Alignment::Center),
            )
            .padding([6.0 * zoom, 14.0 * zoom])
            .into(),
        ];
        for (n, finding) in self.findings.iter().enumerate() {
            rows.push(widget::divider::horizontal::light().into());
            rows.push(finding.view(n, viewer, settings, toggle(Some(n)), size, zoom));
        }
        let card = widget::container(widget::Column::with_children(rows))
            .width(Length::Fill)
            .class(cosmic::theme::Container::custom(|theme| {
                let cosmic = theme.cosmic();
                container::Style {
                    border: Border {
                        radius: cosmic.corner_radii.radius_m.into(),
                        width: 1.0,
                        color: cosmic.bg_divider().into(),
                    },
                    ..container::Style::default()
                }
            }));

        let mut column: Vec<Element<'a, Message>> = Vec::new();
        if !self.preface.items().is_empty() {
            column.push(markdown::view_with(self.preface.items(), settings, viewer));
        }
        column.push(card.into());
        if !self.afterword.items().is_empty() {
            column.push(markdown::view_with(
                self.afterword.items(),
                settings,
                viewer,
            ));
        }
        widget::Column::with_children(column)
            .spacing(settings.spacing.0)
            .into()
    }
}

impl Finding {
    fn view<'a>(
        &'a self,
        n: usize,
        viewer: &selectable::Viewer<'a>,
        settings: markdown::Settings,
        on_toggle: Message,
        size: f32,
        zoom: f32,
    ) -> Element<'a, Message> {
        let small = size * 0.85;
        let accent: Color = cosmic::theme::active().cosmic().accent_text_color().into();
        let rank = viewer
            .block(vec![text::Span::new(format!("{}.", n + 1))], size.into())
            .font(cosmic::font::semibold())
            .color(accent)
            .shrink();
        let place = viewer
            .block(
                vec![
                    text::Span::new(self.folder.as_str()).color(dim_color()),
                    text::Span::new(self.file.as_str()),
                ],
                small.into(),
            )
            .font(cosmic::font::mono())
            .joiner(" ");
        let mut body: Vec<Element<'a, Message>> = vec![
            place.into(),
            markdown::view_with(self.summary.items(), settings, viewer),
        ];
        if let Some(scenario) = &self.scenario {
            let chevron = if self.open {
                "go-down-symbolic"
            } else {
                "go-next-symbolic"
            };
            body.push(
                widget::button::custom(
                    widget::Row::with_children(vec![
                        widget::icon::from_name(chevron).size(14).into(),
                        widget::text("Failure scenario")
                            .size(small)
                            .class(dim())
                            .into(),
                    ])
                    .spacing(4)
                    .align_y(Alignment::Center),
                )
                .on_press(on_toggle)
                .padding([2, 6, 2, 0])
                .class(cosmic::theme::Button::Text)
                .into(),
            );
            if self.open {
                body.push(
                    widget::container(markdown::view_with(scenario.items(), settings, viewer))
                        .padding([0.0, 0.0, 0.0, 18.0 * zoom])
                        .class(cosmic::theme::Container::custom(|_| container::Style {
                            text_color: Some(dim_color()),
                            ..container::Style::default()
                        }))
                        .into(),
                );
            }
        }
        widget::Row::with_children(vec![
            widget::container(rank)
                .width(Length::Fixed(28.0 * zoom))
                .into(),
            widget::Column::with_children(body)
                .spacing(6.0 * zoom)
                .width(Length::Fill)
                .into(),
        ])
        .padding([10.0 * zoom, 14.0 * zoom])
        .into()
    }
}

fn parsed(text: &str) -> markdown::Content {
    markdown::Content::parse(&furigana::annotate(text, 0..text.len()))
}
