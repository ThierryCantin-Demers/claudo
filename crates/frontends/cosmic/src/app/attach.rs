//! Files sent with a message: pasted images and picked or dropped files, shown above the
//! message box until it is sent and in the transcript after, an image larger when clicked.

use std::borrow::Cow;
use std::path::PathBuf;

use cosmic::dialog::file_chooser;
use cosmic::iced::ContentFit;
use cosmic::iced::clipboard::mime::AllowedMimeTypes;

use super::*;
use claudo_protocol::Attachment;

/// What a file manager offers for the files dragged from it.
pub(super) const URI_LIST: &str = "text/uri-list";

/// A window's tile row as a target for files, apart from the one for tiles.
pub(super) fn drop_target_id(w: window::Id) -> u64 {
    tile_drag::drop_target_id(w).wrapping_add(1)
}

/// An attachment with its thumbnail, made once so the renderer decodes the image once.
pub(super) struct Shown {
    pub(super) attachment: Attachment,
    thumbnail: Option<widget::image::Handle>,
}

impl Shown {
    pub(super) fn new(attachment: Attachment) -> Self {
        let thumbnail = attachment
            .is_image()
            .then(|| attachment.bytes())
            .flatten()
            .map(widget::image::Handle::from_bytes);
        Self {
            attachment,
            thumbnail,
        }
    }
}

/// An image on the clipboard, such as a screenshot.
#[derive(Debug, Clone)]
pub(super) struct ClipboardImage(pub(super) Attachment);

impl AllowedMimeTypes for ClipboardImage {
    fn allowed() -> Cow<'static, [String]> {
        ["image/png", "image/jpeg", "image/webp", "image/gif"]
            .map(String::from)
            .to_vec()
            .into()
    }
}

impl TryFrom<(Vec<u8>, String)> for ClipboardImage {
    type Error = ();

    fn try_from((bytes, mime): (Vec<u8>, String)) -> Result<Self, ()> {
        let extension = mime.strip_prefix("image/").ok_or(())?;
        let name = format!("pasted.{extension}");
        Ok(Self(Attachment::new(name, &mime, &bytes)))
    }
}

/// The files the user picks, read, or why one could not be.
pub(super) async fn pick() -> Result<Vec<Attachment>, String> {
    let picked = file_chooser::open::Dialog::new()
        .title("Attach files")
        .open_files()
        .await;
    match picked {
        Ok(response) => read_all(
            response
                .urls()
                .iter()
                .filter_map(|url| url.to_file_path().ok()),
        ),
        Err(file_chooser::Error::Cancelled) => Ok(Vec::new()),
        Err(error) => Err(format!("could not open the file picker: {error}")),
    }
}

/// The files in a dropped `text/uri-list`; addresses that are not files are left out.
pub(super) fn dropped(list: &[u8]) -> Vec<PathBuf> {
    String::from_utf8_lossy(list)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| url::Url::parse(line).ok()?.to_file_path().ok())
        .collect()
}

pub(super) fn read_all(
    paths: impl IntoIterator<Item = PathBuf>,
) -> Result<Vec<Attachment>, String> {
    paths.into_iter().map(read).collect()
}

fn read(path: PathBuf) -> Result<Attachment, String> {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let bytes = std::fs::read(&path).map_err(|e| format!("cannot read {name}: {e}"))?;
    Attachment::file(name, &bytes)
}

/// The files waiting above the message box, each with a button that takes it off.
pub(super) fn pending<'a>(
    conversation: &'a ConversationId,
    files: &'a [Shown],
    size: f32,
    zoom: f32,
) -> Option<Element<'a, Message>> {
    if files.is_empty() {
        return None;
    }
    let row = files.iter().enumerate().map(|(index, file)| {
        widget::Row::with_children(vec![
            preview(file, 56.0 * zoom, size),
            widget::button::icon(widget::icon::from_name("window-close-symbolic"))
                .extra_small()
                .on_press(Message::Detach(conversation.clone(), index))
                .into(),
        ])
        .align_y(Alignment::Start)
        .into()
    });
    Some(
        widget::Row::with_children(row.collect::<Vec<_>>())
            .spacing(8)
            .into(),
    )
}

/// A sent file, on the user's side of the transcript.
pub(super) fn sent(file: &Shown, size: f32, zoom: f32) -> Element<'_, Message> {
    widget::container(preview(file, 180.0 * zoom, size))
        .align_right(Length::Fill)
        .into()
}

fn preview(file: &Shown, height: f32, size: f32) -> Element<'_, Message> {
    match &file.thumbnail {
        Some(thumbnail) => widget::button::custom(
            widget::image(thumbnail.clone())
                .height(Length::Fixed(height))
                .content_fit(ContentFit::Contain),
        )
        .padding(0)
        .class(cosmic::theme::Button::Image)
        .on_press(Message::ViewImage(Some(Viewed {
            name: file.attachment.name.clone(),
            image: thumbnail.clone(),
        })))
        .into(),
        None => {
            let icon = if file.attachment.media_type == "application/pdf" {
                "x-office-document-symbolic"
            } else {
                "text-x-generic-symbolic"
            };
            widget::container(
                widget::Row::with_children(vec![
                    widget::icon::from_name(icon).size(16).into(),
                    widget::text(file.attachment.name.as_str())
                        .size(size * 0.9)
                        .into(),
                ])
                .spacing(6)
                .align_y(Alignment::Center),
            )
            .padding([6, 10])
            .class(cosmic::theme::Container::Card)
            .into()
        }
    }
}

/// An image opened over its window.
#[derive(Debug, Clone)]
pub struct Viewed {
    pub name: String,
    pub image: widget::image::Handle,
}

/// The image filling the window above everything else, the scroll wheel zooming it around
/// the cursor and a drag moving it.
pub(super) fn viewer(viewed: &Viewed, size: f32) -> Element<'_, Message> {
    let header = widget::Row::with_children(vec![
        widget::text(viewed.name.as_str())
            .size(size)
            .font(cosmic::font::semibold())
            .wrapping(text::Wrapping::None)
            .ellipsize(Ellipsize::End(EllipsizeHeightLimit::Lines(1)))
            .width(Length::Fill)
            .into(),
        widget::text("Scroll to zoom, drag to move")
            .size(size * 0.9)
            .class(dim())
            .into(),
        widget::button::icon(widget::icon::from_name("window-close-symbolic"))
            .on_press(Message::ViewImage(None))
            .into(),
    ])
    .spacing(12)
    .align_y(Alignment::Center);
    let image = cosmic::iced::widget::image::viewer(viewed.image.clone())
        .width(Length::Fill)
        .height(Length::Fill)
        .content_fit(ContentFit::Contain)
        .min_scale(0.1)
        .max_scale(20.0);
    cosmic::iced::widget::opaque(
        widget::container(
            widget::Column::with_children(vec![header.into(), image.into()]).spacing(8),
        )
        .padding([8, 16, 16, 16])
        .width(Length::Fill)
        .height(Length::Fill)
        .class(cosmic::theme::Container::custom(|theme| {
            let background: Color = theme.cosmic().bg_color().into();
            container::Style {
                background: Some(
                    Color {
                        a: 0.96,
                        ..background
                    }
                    .into(),
                ),
                ..container::Style::default()
            }
        })),
    )
}
