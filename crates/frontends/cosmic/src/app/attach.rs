//! Files sent with a message: pasted images and picked files, shown above the message box
//! until it is sent and in the transcript after.

use std::borrow::Cow;
use std::path::PathBuf;

use cosmic::dialog::file_chooser;
use cosmic::iced::ContentFit;
use cosmic::iced::clipboard::mime::AllowedMimeTypes;

use super::*;
use claudo_protocol::Attachment;

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
        Ok(response) => response
            .urls()
            .iter()
            .filter_map(|url| url.to_file_path().ok())
            .map(read)
            .collect(),
        Err(file_chooser::Error::Cancelled) => Ok(Vec::new()),
        Err(error) => Err(format!("could not open the file picker: {error}")),
    }
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
        Some(thumbnail) => widget::image(thumbnail.clone())
            .height(Length::Fixed(height))
            .content_fit(ContentFit::Contain)
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
