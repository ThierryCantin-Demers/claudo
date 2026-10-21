//! Ctrl+Z in the message box. iced's editor keeps no history, so each conversation's draft
//! keeps its own: a step per word typed, per paste or other edit, and per file attached.

use super::*;
use cosmic::iced::advanced::text::editor::{Cursor, Edit};

#[derive(Default)]
pub(super) struct History {
    undo: Vec<Step>,
    redo: Vec<Step>,
    run: Option<Run>,
}

enum Step {
    /// The draft as it was.
    Draft { text: String, cursor: Cursor },
    /// Files were attached past this many.
    Attached(usize),
    /// Files an undo took off, to go back at this index.
    Detached(usize, Vec<attach::Shown>),
}

/// The edits a step goes on gathering: a word with the spaces after it, or deletions.
#[derive(Clone, Copy, PartialEq)]
enum Run {
    Word,
    Space,
    Delete,
}

impl History {
    /// Before `edit` changes `draft`.
    pub(super) fn edit(&mut self, draft: &text_editor::Content, edit: &Edit) {
        let run = match edit {
            Edit::Insert(c) if c.is_whitespace() => Some(Run::Space),
            Edit::Insert(_) => Some(Run::Word),
            Edit::Backspace | Edit::Delete => Some(Run::Delete),
            Edit::Paste(_) | Edit::Enter | Edit::Indent | Edit::Unindent => None,
        };
        let goes_on = matches!(
            (self.run, run),
            (Some(Run::Word), Some(Run::Word | Run::Space))
                | (Some(Run::Space), Some(Run::Space))
                | (Some(Run::Delete), Some(Run::Delete))
        );
        if !goes_on || draft.selection().is_some() {
            self.push(snapshot(draft));
        }
        self.run = run;
    }

    /// The cursor moved, so the next edit starts a step of its own.
    pub(super) fn moved(&mut self) {
        self.run = None;
    }

    /// Before something other than an edit replaces the draft.
    pub(super) fn replacing(&mut self, draft: &text_editor::Content) {
        self.push(snapshot(draft));
        self.run = None;
    }

    /// Before files are attached to the `count` already there.
    pub(super) fn attaching(&mut self, count: usize) {
        self.push(Step::Attached(count));
        self.run = None;
    }

    /// The draft was sent, so there is nothing to take back.
    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }

    pub(super) fn undo(
        &mut self,
        draft: &mut text_editor::Content,
        files: &mut Vec<attach::Shown>,
    ) {
        if let Some(step) = self.undo.pop() {
            let reverse = apply(step, draft, files);
            self.redo.push(reverse);
        }
        self.run = None;
    }

    pub(super) fn redo(
        &mut self,
        draft: &mut text_editor::Content,
        files: &mut Vec<attach::Shown>,
    ) {
        if let Some(step) = self.redo.pop() {
            let reverse = apply(step, draft, files);
            self.undo.push(reverse);
        }
        self.run = None;
    }

    fn push(&mut self, step: Step) {
        self.undo.push(step);
        self.redo.clear();
    }
}

fn snapshot(draft: &text_editor::Content) -> Step {
    Step::Draft {
        text: draft.text(),
        cursor: draft.cursor(),
    }
}

/// Puts `step` back, and returns the step that takes it back again.
fn apply(step: Step, draft: &mut text_editor::Content, files: &mut Vec<attach::Shown>) -> Step {
    match step {
        Step::Draft { text, cursor } => {
            let now = snapshot(draft);
            *draft = text_editor::Content::with_text(&text);
            draft.move_to(cursor);
            now
        }
        Step::Attached(count) => {
            let count = count.min(files.len());
            Step::Detached(count, files.split_off(count))
        }
        Step::Detached(at, back) => {
            let at = at.min(files.len());
            files.splice(at..at, back);
            Step::Attached(at)
        }
    }
}
