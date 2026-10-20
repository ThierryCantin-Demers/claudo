//! A code review's findings as a list to scan: each one's rank, place and statement, with
//! what makes it go wrong folded under it. A finding with a title takes one line, its
//! statement folded too, and bugs come before the rest.

use super::*;
use claudo_protocol::Review;

pub(super) struct Shown {
    preface: markdown::Content,
    /// In the reviewer's order, which their ranks follow.
    pub findings: Vec<Finding>,
    afterword: markdown::Content,
}

pub(super) struct Finding {
    /// The folder, then the file and line, so the file stands out.
    folder: String,
    file: String,
    title: Option<String>,
    /// Its category, then whether it went unconfirmed and what a fix did.
    tags: Vec<String>,
    bug: bool,
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
                    let unconfirmed =
                        (finding.confirmed == Some(false)).then(|| "unconfirmed".to_string());
                    Finding {
                        folder,
                        file: match finding.line {
                            Some(line) => format!("{file}:{line}"),
                            None => file,
                        },
                        title: finding.title,
                        tags: [finding.category, unconfirmed, finding.outcome]
                            .into_iter()
                            .flatten()
                            .collect(),
                        bug: finding.bug,
                        summary: parsed(&finding.summary),
                        scenario: finding.failure_scenario.as_deref().map(parsed),
                        open: false,
                    }
                })
                .collect(),
            afterword: parsed(&review.afterword),
        }
    }

    /// Opens every finding, or closes them all once they are.
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
        let titled = self.findings.iter().any(|finding| finding.title.is_some());
        let count = match self.findings.len() {
            0 => "No findings".to_string(),
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
        if titled
            || self
                .findings
                .iter()
                .any(|finding| finding.scenario.is_some())
        {
            header.push(
                widget::button::text(match (titled, all_open) {
                    (true, false) => "Show details",
                    (true, true) => "Hide details",
                    (false, false) => "Show scenarios",
                    (false, true) => "Hide scenarios",
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
        let (bugs, others): (Vec<usize>, Vec<usize>) =
            (0..self.findings.len()).partition(|&n| self.findings[n].bug);
        let groups = if bugs.is_empty() || others.is_empty() {
            vec![(None, (0..self.findings.len()).collect())]
        } else {
            vec![
                (Some(("Bugs", true)), bugs),
                (Some(("Other findings", false)), others),
            ]
        };
        for (label, members) in groups {
            if let Some((label, bug)) = label {
                let label = widget::text(label)
                    .size(small)
                    .font(cosmic::font::semibold());
                rows.push(widget::divider::horizontal::light().into());
                rows.push(
                    widget::container(
                        widget::Row::with_children(vec![
                            if bug {
                                label.class(destructive()).into()
                            } else {
                                label.into()
                            },
                            widget::text(members.len().to_string())
                                .size(small)
                                .class(dim())
                                .into(),
                        ])
                        .spacing(6),
                    )
                    .padding([6.0 * zoom, 14.0 * zoom, 0.0, 14.0 * zoom])
                    .into(),
                );
            }
            for (i, &n) in members.iter().enumerate() {
                if label.is_none() || i > 0 {
                    rows.push(widget::divider::horizontal::light().into());
                }
                let finding = &self.findings[n];
                rows.push(finding.view(n, viewer, settings, toggle(Some(n)), size, zoom));
            }
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
            .font(cosmic::font::mono());
        let toggle = (self.title.is_some() || self.scenario.is_some()).then(|| {
            let chevron = if self.open {
                "go-down-symbolic"
            } else {
                "go-next-symbolic"
            };
            let label = if self.title.is_some() {
                "Details"
            } else {
                "Failure scenario"
            };
            widget::button::custom(
                widget::Row::with_children(vec![
                    widget::icon::from_name(chevron).size(14).into(),
                    widget::text(label).size(small).class(dim()).into(),
                ])
                .spacing(4)
                .align_y(Alignment::Center),
            )
            .on_press(on_toggle)
            .padding([2, 6, 2, 0])
            .class(cosmic::theme::Button::Text)
            .into()
        });
        let mut body: Vec<Element<'a, Message>> = Vec::new();
        let mut details: Vec<Element<'a, Message>> = Vec::new();
        match &self.title {
            Some(title) => {
                let mut line: Vec<Element<'a, Message>> = vec![
                    viewer
                        .block(vec![text::Span::new(title.as_str())], size.into())
                        .font(cosmic::font::semibold())
                        .joiner(" ")
                        .into(),
                ];
                line.extend(
                    self.tags
                        .iter()
                        .enumerate()
                        .map(|(i, label)| tag(label, i == 0 && self.bug, small * 0.9)),
                );
                body.push(
                    widget::Row::with_children(line)
                        .spacing(6)
                        .align_y(Alignment::Center)
                        .into(),
                );
                let mut line = vec![place.shrink().into()];
                line.extend(toggle);
                body.push(
                    widget::Row::with_children(line)
                        .spacing(10)
                        .align_y(Alignment::Center)
                        .into(),
                );
                details.push(markdown::view_with(self.summary.items(), settings, viewer));
            }
            None => {
                body.push(place.joiner(" ").into());
                body.push(markdown::view_with(self.summary.items(), settings, viewer));
                body.extend(toggle);
            }
        }
        if let Some(scenario) = &self.scenario {
            details.push(
                widget::container(markdown::view_with(scenario.items(), settings, viewer))
                    .class(cosmic::theme::Container::custom(|_| container::Style {
                        text_color: Some(dim_color()),
                        ..container::Style::default()
                    }))
                    .into(),
            );
        }
        if self.open && !details.is_empty() {
            body.push(
                widget::container(
                    widget::Column::with_children(details).spacing(settings.spacing.0),
                )
                .padding([0.0, 0.0, 0.0, 18.0 * zoom])
                .into(),
            );
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

/// A short label beside a finding's title; a bug's category stands out.
fn tag(label: &str, bug: bool, size: f32) -> Element<'_, Message> {
    widget::container(widget::text(label).size(size))
        .padding([0, 6])
        .class(cosmic::theme::Container::custom(move |theme| {
            let cosmic = theme.cosmic();
            let color: Color = if bug {
                cosmic.destructive_color().into()
            } else {
                dim_color()
            };
            container::Style {
                text_color: Some(color),
                border: Border {
                    radius: cosmic.corner_radii.radius_xl.into(),
                    width: 1.0,
                    color: Color { a: 0.5, ..color },
                },
                ..container::Style::default()
            }
        }))
        .into()
}

fn parsed(text: &str) -> markdown::Content {
    markdown::Content::parse(&furigana::annotate(text, 0..text.len()))
}
