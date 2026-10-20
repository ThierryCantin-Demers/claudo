//! The project folder a conversation works in: chosen in the message box's controls until its
//! first message, and named there after.

use std::path::Path;

use cosmic::dialog::file_chooser;

use super::*;

#[derive(Debug, Clone, PartialEq)]
pub enum Choice {
    Scratch,
    Folder(String),
    Pick,
}

impl ConversationView {
    /// Its folder can still be chosen: nothing was sent yet.
    pub(super) fn choosing_folder(&self) -> bool {
        self.items.is_empty() && !self.streaming
    }
}

impl ClaudoApp {
    pub(super) fn folder_row<'a>(
        &'a self,
        id: &'a ConversationId,
        view: &ConversationView,
    ) -> Option<Element<'a, Message>> {
        let info = self.info(id)?;
        let size = 13.0 * self.zoom;
        let current = info.attached.then_some(info.cwd.as_str());
        let content: Element<Message> = if view.choosing_folder() {
            let choices = choices(&self.folders, current);
            let labels: Vec<String> = choices.iter().map(label).collect();
            let selected = choices.iter().position(|choice| match (choice, current) {
                (Choice::Scratch, None) => true,
                (Choice::Folder(folder), Some(current)) => folder == current,
                _ => false,
            });
            let id = id.clone();
            widget::dropdown(labels, selected, move |i| {
                Message::MoveTo(id.clone(), choices[i].clone())
            })
            .text_size(size)
            .padding([4, 8])
            .into()
        } else {
            widget::text(home_relative(current?))
                .size(size)
                .class(dim())
                .wrapping(text::Wrapping::None)
                .ellipsize(Ellipsize::End(EllipsizeHeightLimit::Lines(1)))
                .width(Length::Fill)
                .into()
        };
        // Fills the controls' free middle, so a narrow tile shortens the path, not the row.
        Some(
            widget::Row::with_children(vec![
                widget::icon::from_name("folder-symbolic").size(16).into(),
                content,
            ])
            .spacing(6)
            .padding([0, 6])
            .width(Length::Fill)
            .align_y(Alignment::Center)
            .into(),
        )
    }
}

/// The scratch folder, then the folders worked in lately with `current` among them, then
/// the picker.
fn choices(recent: &[String], current: Option<&str>) -> Vec<Choice> {
    let missing = current.filter(|current| !recent.iter().any(|f| f == current));
    std::iter::once(Choice::Scratch)
        .chain(missing.map(|f| Choice::Folder(f.to_string())))
        .chain(recent.iter().cloned().map(Choice::Folder))
        .chain([Choice::Pick])
        .collect()
}

fn label(choice: &Choice) -> String {
    match choice {
        Choice::Scratch => "Scratch folder".to_string(),
        Choice::Folder(folder) => home_relative(folder),
        Choice::Pick => "Choose folder…".to_string(),
    }
}

/// The folder the user picks, `None` if they cancel.
pub(super) async fn pick() -> Result<Option<String>, String> {
    let picked = file_chooser::open::Dialog::new()
        .title("Choose a project folder")
        .open_folder()
        .await;
    match picked {
        Ok(response) => Ok(response
            .url()
            .to_file_path()
            .ok()
            .map(|path| path.display().to_string())),
        Err(file_chooser::Error::Cancelled) => Ok(None),
        Err(error) => Err(format!("could not open the folder picker: {error}")),
    }
}

pub(super) fn home_relative(path: &str) -> String {
    let home = std::env::var_os("HOME").unwrap_or_default();
    match Path::new(path).strip_prefix(&home) {
        Ok(rest) if !home.is_empty() => Path::new("~").join(rest).display().to_string(),
        _ => path.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_chosen_with_the_picker_is_offered_with_the_recent_ones() {
        let recent = ["/a".to_string(), "/b".to_string()];
        let folder = |f: &str| Choice::Folder(f.to_string());
        assert_eq!(
            choices(&recent, Some("/c")),
            [
                Choice::Scratch,
                folder("/c"),
                folder("/a"),
                folder("/b"),
                Choice::Pick
            ]
        );
        assert_eq!(
            choices(&recent, Some("/b")),
            [Choice::Scratch, folder("/a"), folder("/b"), Choice::Pick]
        );
    }
}
