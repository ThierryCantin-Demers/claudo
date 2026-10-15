//! Drives the COSMIC view headlessly: events go to a real widget tree, the messages
//! they produce go to `update`, and the tree's state carries over between frames as
//! it does in the running app. Run with `cargo test -p claudo-cosmic --features cosmic`.

use super::*;
use claudo_protocol::AuthStatus;
use cosmic::iced::advanced::clipboard;
use cosmic::iced::advanced::renderer::Headless;
use cosmic::iced::runtime::user_interface::{Cache, UserInterface};
use cosmic::iced::{Event, Size, mouse};

const WINDOW: Size = Size::new(1400.0, 900.0);

struct Harness {
    app: ClaudoApp,
    renderer: cosmic::Renderer,
    cache: Cache,
    cursor: Point,
}

impl Harness {
    fn new(app: ClaudoApp) -> Self {
        let renderer =
            cosmic::iced::futures::executor::block_on(<cosmic::Renderer as Headless>::new(
                cosmic::font::default(),
                14.0.into(),
                Some("tiny-skia"),
            ))
            .expect("a headless renderer");
        Self {
            app,
            renderer,
            cache: Cache::default(),
            cursor: Point::ORIGIN,
        }
    }

    /// Builds the app's current view on the kept widget state, as each frame does.
    fn frame<T>(
        &mut self,
        f: impl FnOnce(
            &mut UserInterface<'_, Message, cosmic::Theme, cosmic::Renderer>,
            &mut cosmic::Renderer,
        ) -> T,
    ) -> T {
        let cache = std::mem::take(&mut self.cache);
        let mut ui = UserInterface::build(self.app.view(), WINDOW, cache, &mut self.renderer);
        let out = f(&mut ui, &mut self.renderer);
        self.cache = ui.into_cache();
        out
    }

    fn send(&mut self, events: Vec<Event>) {
        let cursor = mouse::Cursor::Available(self.cursor);
        let messages = self.frame(|ui, renderer| {
            let mut messages = Vec::new();
            let _ = ui.update(
                &events,
                cursor,
                renderer,
                &mut clipboard::Null,
                &mut messages,
            );
            messages
        });
        for message in messages {
            let _ = self.app.update(message);
        }
    }

    fn move_to(&mut self, x: f32, y: f32) {
        self.cursor = Point::new(x, y);
        self.send(vec![Event::Mouse(mouse::Event::CursorMoved {
            position: self.cursor,
        })]);
    }

    /// Renders the window as it stands to `target/snapshots/<name>.png`.
    fn snapshot(&mut self, name: &str) {
        use cosmic::iced::theme::Base;
        let theme = cosmic::theme::active();
        let cursor = mouse::Cursor::Available(self.cursor);
        let size = Size::new(WINDOW.width as u32, WINDOW.height as u32);
        let rgba = self.frame(|ui, renderer| {
            let base = theme.base();
            ui.draw(
                renderer,
                &theme,
                &cosmic::iced::advanced::renderer::Style {
                    text_color: base.text_color,
                    icon_color: base.text_color,
                    scale_factor: 1.0,
                },
                cursor,
            );
            renderer.screenshot(size, 1.0, base.background_color)
        });
        write_png(name, size, &rgba);
    }

    /// Focuses a widget, as the tasks `update` returns would.
    fn focus(&mut self, id: widget::Id) {
        use cosmic::iced::advanced::widget::operation::{black_box, focusable};
        self.frame(|ui, renderer| {
            let mut focus = focusable::focus::<()>(id);
            ui.operate(renderer, &mut black_box(&mut focus));
        });
    }

    /// Renders the app's dialog alone, as libcosmic shows it over the window.
    fn snapshot_dialog(&mut self, name: &str) {
        let dialog = self.app.dialog().expect("a dialog");
        render_alone(&mut self.renderer, dialog, name);
    }

    /// What Ctrl+C would copy.
    fn selected(&mut self) -> String {
        use cosmic::iced::advanced::widget::operation::{Outcome, black_box};
        self.frame(|ui, renderer| {
            let mut collect = selectable::Collect::default();
            ui.operate(renderer, &mut black_box(&mut collect));
            match Operation::<String>::finish(&collect) {
                Outcome::Some(text) => text,
                _ => String::new(),
            }
        })
    }

    /// The scroll offset of each tile's transcript, left to right.
    fn offsets(&mut self) -> Vec<f32> {
        struct Offsets(Vec<(f32, f32)>);

        impl Operation for Offsets {
            fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
                operate(self);
            }

            fn scrollable(
                &mut self,
                _id: Option<&widget::Id>,
                bounds: cosmic::iced::Rectangle,
                content_bounds: cosmic::iced::Rectangle,
                translation: cosmic::iced::Vector,
                _state: &mut dyn cosmic::iced::advanced::widget::operation::Scrollable,
            ) {
                // Transcripts are the scrollables taller than their viewport.
                if content_bounds.height > bounds.height {
                    self.0.push((bounds.x, translation.y));
                }
            }
        }

        let mut found = self.frame(|ui, renderer| {
            let mut offsets = Offsets(Vec::new());
            ui.operate(renderer, &mut offsets);
            offsets.0
        });
        found.sort_by(|a, b| a.0.total_cmp(&b.0));
        found.into_iter().map(|(_, y)| y).collect()
    }
}

fn render_alone(renderer: &mut cosmic::Renderer, element: Element<'_, Message>, name: &str) {
    use cosmic::iced::theme::Base;
    let mut ui = UserInterface::build(element, WINDOW, Cache::default(), renderer);
    let theme = cosmic::theme::active();
    let size = Size::new(WINDOW.width as u32, WINDOW.height as u32);
    let base = theme.base();
    ui.draw(
        renderer,
        &theme,
        &cosmic::iced::advanced::renderer::Style {
            text_color: base.text_color,
            icon_color: base.text_color,
            scale_factor: 1.0,
        },
        mouse::Cursor::Unavailable,
    );
    let rgba = renderer.screenshot(size, 1.0, base.background_color);
    write_png(name, size, &rgba);
}

fn write_png(name: &str, size: Size<u32>, rgba: &[u8]) {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../target/snapshots");
    std::fs::create_dir_all(&dir).expect("a snapshot directory");
    let file = std::fs::File::create(dir.join(format!("{name}.png"))).expect("a snapshot file");
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), size.width, size.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .and_then(|mut writer| writer.write_image_data(rgba))
        .expect("a written snapshot");
}

fn ctrl(held: bool) -> Event {
    let modifiers = if held {
        keyboard::Modifiers::CTRL
    } else {
        keyboard::Modifiers::empty()
    };
    Event::Keyboard(keyboard::Event::ModifiersChanged(modifiers))
}

fn button(pressed: bool) -> Event {
    Event::Mouse(if pressed {
        mouse::Event::ButtonPressed(mouse::Button::Left)
    } else {
        mouse::Event::ButtonReleased(mouse::Button::Left)
    })
}

fn key(
    named: keyboard::key::Named,
    code: keyboard::key::Code,
    modifiers: keyboard::Modifiers,
) -> Event {
    Event::Keyboard(keyboard::Event::KeyPressed {
        key: keyboard::Key::Named(named),
        modified_key: keyboard::Key::Named(named),
        physical_key: keyboard::key::Physical::Code(code),
        location: keyboard::Location::Standard,
        modifiers,
        text: None,
        repeat: false,
    })
}

fn escape() -> Event {
    use keyboard::key::{Code, Named};
    key(Named::Escape, Code::Escape, keyboard::Modifiers::empty())
}

fn wheel(lines: f32) -> Event {
    Event::Mouse(mouse::Event::WheelScrolled {
        delta: mouse::ScrollDelta::Lines { x: 0.0, y: lines },
    })
}

const ALPHA: &str = "aaaaa111-0000-4000-8000-000000000000";
const BRAVO: &str = "bbbbb222-0000-4000-8000-000000000000";

/// Two tiles side by side, each with a transcript much taller than the window.
fn two_tiles() -> ClaudoApp {
    // `update` saves the window's state; keep it away from the user's.
    let state = std::env::temp_dir().join("claudo-cosmic-tests");
    // SAFETY: set before anything else reads the environment in this test binary.
    unsafe { std::env::set_var("XDG_STATE_HOME", &state) };

    let (mut app, _) = ClaudoApp::init(Core::default(), UiState::default());
    app.status = Some(SystemStatus {
        claude_installed: true,
        claude_version: None,
        auth: AuthStatus {
            logged_in: true,
            ..AuthStatus::default()
        },
    });
    app.restore = None;
    app.layout_mut().size = Some((WINDOW.width, WINDOW.height));
    for (id, title) in [(ALPHA, "Alpha"), (BRAVO, "Bravo")] {
        app.conversations.push(ConversationInfo {
            id: id.to_string(),
            title: title.to_string(),
            cwd: "/tmp".to_string(),
            attached: false,
            archived: false,
            overrides: Overrides::default(),
            context: Default::default(),
        });
        let mut view = ConversationView::new("");
        for n in 0..30 {
            view.items
                .push(Entry::User(format!("{title} question {n}")));
            view.items
                .push(Entry::Reply(Box::new(Reply::complete(format!(
                    "{title} answer {n}. A paragraph long enough to wrap over a few lines in \
                 a tile, so the transcript is far taller than the window."
                )))));
        }
        app.views.insert(id.to_string(), view);
        app.layout_mut().tiles.push(Tile::new(id.to_string()));
    }
    app
}

#[test]
fn moved_tiles_keep_their_scroll_positions() {
    let mut h = Harness::new(two_tiles());
    let (alpha_x, bravo_x, y) = (560.0, 1120.0, 400.0);

    h.move_to(alpha_x, y);
    h.send(vec![wheel(10.0)]);
    let [alpha, bravo] = h.offsets()[..] else {
        panic!("two transcripts");
    };
    assert_ne!(alpha, bravo, "the wheel scrolled Alpha");

    h.send(vec![ctrl(true), button(true)]);
    for x in [600.0, 800.0, 1000.0, bravo_x] {
        h.move_to(x, y);
    }
    assert!(matches!(
        h.app.layout().drag,
        Some(Drag::Tile { at: Some(_), .. })
    ));
    assert_eq!(
        h.offsets(),
        [bravo, alpha],
        "while dragged, each keeps its own"
    );

    h.send(vec![button(false)]);
    let _ = h.app.update(Message::DragEnd);
    h.send(vec![ctrl(false)]);
    assert_eq!(
        h.app.layout_mut().tiles[1].conversation,
        ALPHA,
        "Alpha moved right"
    );
    assert_eq!(
        h.offsets(),
        [bravo, alpha],
        "after the drop, each keeps its own"
    );
}

/// Renders a tile drag to `target/snapshots`, to look at rather than assert on.
#[test]
#[ignore = "writes images; run with --ignored"]
fn snapshots() {
    let mut h = Harness::new(two_tiles());
    h.move_to(560.0, 400.0);
    h.send(vec![wheel(10.0)]);
    h.snapshot("two-tiles");
    h.send(vec![ctrl(true), button(true)]);
    for x in [600.0, 800.0, 1000.0] {
        h.move_to(x, 400.0);
    }
    h.snapshot("dragging");

    let mut app = two_tiles();
    let command = "cd /tmp && gh issue create --title Probe";
    app.views.get_mut(ALPHA).unwrap().approvals.push(Approval {
        request_id: "r1".to_string(),
        call: ToolCall {
            call_id: "t1".to_string(),
            name: "Bash".to_string(),
            summary: command.to_string(),
            description: String::new(),
            command: Some(command.to_string()),
            input: String::new(),
            result: None,
            is_error: false,
            background: None,
        },
        scope: "gh issue create".to_string(),
    });
    Harness::new(app).snapshot("approval");

    let mut app = with_commands();
    app.layout_mut().sidebar_open = false;
    let _ = app.update(Message::ShowCommands(true));
    let idx = app.views[ALPHA].commands().nth(2).unwrap().0;
    let _ = app.update(Message::ToggleCommand(ALPHA.to_string(), idx));
    Harness::new(app).snapshot("commands");

    let mut app = two_tiles();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let window = |used, hours: u64| claudo_protocol::QuotaWindow {
        used,
        resets_at: now + hours * 3600 + 1800,
    };
    app.quota = Some(claudo_protocol::Quota {
        five_hour: Some(window(0.42, 2)),
        seven_day: Some(window(0.86, 61)),
    });
    Harness::new(app).snapshot("usage");
}

#[test]
fn text_is_selected_across_messages_and_copied() {
    let mut h = Harness::new(two_tiles());

    // From inside "Alpha answer 25" down into the second line of "Alpha answer 27".
    h.move_to(330.0, 293.0);
    h.send(vec![button(true)]);
    h.move_to(450.0, 400.0);
    h.move_to(400.0, 543.0);
    h.send(vec![button(false)]);
    let _ = h.app.update(Message::DragEnd);

    let copied = h.selected();
    println!("{copied:?}");
    assert!(
        copied.starts_with("nswer 25") || copied.starts_with("answer 25"),
        "{copied:?}"
    );
    assert!(
        copied.contains("\n\nAlpha question 26\n\nAlpha answer 26."),
        "{copied:?}"
    );
    assert!(copied.contains("Alpha answer 27."), "{copied:?}");
    assert!(
        !copied.contains("taller than the window.\n\nAlpha question 28"),
        "{copied:?}"
    );
}

#[test]
fn a_double_click_selects_a_word() {
    let mut h = Harness::new(two_tiles());
    h.move_to(450.0, 638.0);
    h.send(vec![
        button(true),
        button(false),
        button(true),
        button(false),
    ]);
    assert_eq!(h.selected(), "paragraph");
}

#[test]
#[ignore = "writes images; run with --ignored"]
fn selection_snapshot() {
    let mut app = two_tiles();
    let view = app.views.get_mut(ALPHA).expect("Alpha");
    view.items.push(Entry::Reply(Box::new(Reply::complete(
        "## A heading\n\nSome **bold** text and `inline code`, then a list:\n\n\
         - first point\n- second point\n\n```rust\nfn main() {\n    println!(\"hi\");\n}\n```\n\nThe end."
            .to_string(),
    ))));
    let mut h = Harness::new(app);
    h.move_to(330.0, 640.0);
    h.send(vec![button(true)]);
    h.move_to(330.0, 780.0);
    h.snapshot("selection");
    h.send(vec![button(false)]);
    let _ = h.app.update(Message::DragEnd);
    println!("{:?}", h.selected());
}

#[test]
fn clicking_a_title_renames_the_conversation() {
    let mut h = Harness::new(two_tiles());
    let title = |h: &Harness| h.app.info(ALPHA).map(|c| c.title.clone()).expect("Alpha");

    h.move_to(300.0, 20.0);
    h.send(vec![button(true), button(false)]);
    assert_eq!(
        h.app.renaming,
        Some(Renaming {
            conversation: ALPHA.to_string(),
            text: "Alpha".to_string(),
            at: TitleAt::Header,
        })
    );

    let _ = h
        .app
        .update(Message::RenameText("  Research agent ".to_string()));
    let _ = h.app.update(Message::CommitRename);
    assert_eq!(h.app.renaming, None);
    assert_eq!(title(&h), "Research agent", "kept, trimmed");

    h.move_to(300.0, 20.0);
    h.send(vec![button(true), button(false)]);
    let _ = h
        .app
        .update(Message::RenameText("Something else".to_string()));
    h.send(vec![escape()]);
    assert_eq!(h.app.renaming, None);
    assert_eq!(title(&h), "Research agent", "Escape leaves it");
}

#[test]
#[ignore = "writes images; run with --ignored"]
fn rename_snapshot() {
    let mut h = Harness::new(two_tiles());
    h.move_to(300.0, 20.0);
    h.send(vec![button(true), button(false)]);
    let _ = h
        .app
        .update(Message::RenameText("Research agent".to_string()));
    h.focus(rename_id());
    h.snapshot("rename");
}

#[test]
fn rename_from_the_sidebar_edits_the_row() {
    let mut h = Harness::new(two_tiles());
    let _ = h
        .app
        .update(Message::StartRename(BRAVO.to_string(), TitleAt::Sidebar));
    h.focus(rename_id());
    h.snapshot("rename-sidebar");
    assert!(h.app.renaming(BRAVO, TitleAt::Sidebar));
    assert!(
        !h.app.renaming(BRAVO, TitleAt::Header),
        "the open tile keeps its title"
    );

    let _ = h.app.update(Message::RenameText("Reviewer".to_string()));
    h.send(vec![button(true)]);
    let _ = h.app.update(Message::CommitRename);
    assert_eq!(
        h.app.info(BRAVO).map(|c| c.title.as_str()),
        Some("Reviewer")
    );
}

#[test]
fn ctrl_backspace_and_delete_remove_whole_words() {
    use keyboard::key::{Code, Named};
    let mut app = two_tiles();
    app.views.get_mut(ALPHA).expect("Alpha").draft =
        text_editor::Content::with_text("hello brave new world");
    let mut h = Harness::new(app);
    h.focus(composer_id(ALPHA));
    let ctrl = keyboard::Modifiers::CTRL;
    let draft = |h: &Harness| h.app.views[ALPHA].draft.text();

    let _ = h.app.update(Message::Edit(
        ALPHA.to_string(),
        text_editor::Action::Move(text_editor::Motion::DocumentEnd),
    ));
    h.send(vec![key(Named::Backspace, Code::Backspace, ctrl)]);
    assert_eq!(draft(&h), "hello brave new ");

    let _ = h.app.update(Message::Edit(
        ALPHA.to_string(),
        text_editor::Action::Move(text_editor::Motion::DocumentStart),
    ));
    h.send(vec![key(Named::Delete, Code::Delete, ctrl)]);
    assert_eq!(draft(&h), " brave new ");

    h.send(vec![key(
        Named::Backspace,
        Code::Backspace,
        keyboard::Modifiers::empty(),
    )]);
    assert_eq!(draft(&h), " brave new ", "nothing before the cursor");
}

#[test]
fn up_and_down_recall_sent_prompts() {
    use keyboard::key::{Code, Named};
    let mut app = two_tiles();
    let view = app.views.get_mut(ALPHA).expect("Alpha");
    view.items.push(Entry::User("try again".to_string()));
    view.items.push(Entry::User("try again".to_string()));
    view.draft = text_editor::Content::with_text("half\ntyped");
    view.draft
        .perform(text_editor::Action::Move(text_editor::Motion::DocumentEnd));
    let mut h = Harness::new(app);
    h.focus(composer_id(ALPHA));
    let none = keyboard::Modifiers::empty();
    let up = || key(Named::ArrowUp, Code::ArrowUp, none);
    let down = || key(Named::ArrowDown, Code::ArrowDown, none);
    let draft = |h: &Harness| h.app.views[ALPHA].draft.text();

    h.send(vec![up()]);
    assert_eq!(draft(&h), "half\ntyped", "Up first moves within the draft");
    h.send(vec![up()]);
    assert_eq!(draft(&h), "try again");
    h.send(vec![up()]);
    assert_eq!(draft(&h), "Alpha question 29", "a repeat is recalled once");
    h.send(vec![down()]);
    assert_eq!(draft(&h), "try again");
    h.send(vec![down()]);
    assert_eq!(
        draft(&h),
        "half\ntyped",
        "past the newest is the user's draft"
    );
    h.send(vec![down()]);
    assert_eq!(draft(&h), "half\ntyped");

    h.send(vec![up()]);
    h.send(vec![up()]);
    h.send(vec![key(Named::Enter, Code::Enter, none)]);
    let view = &h.app.views[ALPHA];
    assert!(matches!(view.items.last(), Some(Entry::User(text)) if text == "try again"));
    assert_eq!(view.draft.text(), "");
    assert_eq!(view.recalled, 0);
}

fn bash(id: &str, command: &str, description: &str) -> ToolCall {
    ToolCall {
        call_id: id.to_string(),
        name: "Bash".to_string(),
        summary: command.lines().next().unwrap_or_default().to_string(),
        description: description.to_string(),
        command: Some(command.to_string()),
        input: String::new(),
        result: None,
        is_error: false,
        background: None,
    }
}

/// Alpha, mid-turn, with a command running in the foreground and one in the background,
/// and two that ended.
fn with_commands() -> ClaudoApp {
    use claudo_protocol::{Background, TaskStatus};
    let mut app = two_tiles();
    let view = app.views.get_mut(ALPHA).expect("Alpha");
    let mut built = bash("t1", "cargo build --release", "Build the release binary");
    built.result = Some("   Compiling claudo v0.0.0\n    Finished `release` profile".to_string());
    let mut failed = bash("t2", "cargo test -p claudo-core", "Run the core tests");
    failed.result = Some("test result: FAILED. 9 passed; 1 failed".to_string());
    failed.is_error = true;
    let mut server = bash("t3", "cargo xtask daemon", "Serve the web client");
    server.result = Some("Command running in background with ID: b1.".to_string());
    server.background = Some(Background {
        status: TaskStatus::Running,
        output: None,
    });
    let running = bash("t4", "rg -n \"fn main\" crates", "Find the entry points");
    for call in [built, failed, server, running] {
        view.items.push(Entry::Tool { call, open: false });
    }
    view.streaming = true;
    app
}

#[test]
fn the_commands_panel_lists_every_agent() {
    use claudo_protocol::{Background, TaskStatus};
    let mut app = with_commands();
    let mut server = bash("b1", "npm run dev", "Serve the site");
    server.background = Some(Background {
        status: TaskStatus::Running,
        output: None,
    });
    let bravo = app.views.get_mut(BRAVO).expect("Bravo");
    bravo.items.push(Entry::Tool {
        call: server,
        open: false,
    });
    let groups = |app: &ClaudoApp| {
        app.command_groups(app.main_window())
            .iter()
            .map(|(id, _)| id.as_str().to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(groups(&app), [ALPHA, BRAVO], "the tiles, left to right");
    let running: usize = app
        .views
        .values()
        .map(ConversationView::running_commands)
        .sum();
    assert_eq!(running, 3, "the header counts every agent's");

    let _ = app.update(Message::ToggleCommandGroup(BRAVO.to_string()));
    assert!(app.views[BRAVO].commands_collapsed);

    app.layout_mut().tiles.retain(|t| t.conversation == ALPHA);
    assert_eq!(
        groups(&app),
        [ALPHA, BRAVO],
        "an untiled agent still running stays"
    );
    if let Some(Entry::Tool { call, .. }) = app.views.get_mut(BRAVO).unwrap().items.last_mut() {
        call.background.as_mut().unwrap().status = TaskStatus::Completed;
    }
    assert_eq!(groups(&app), [ALPHA], "and goes once nothing runs");
}

#[test]
fn the_commands_panel_lists_running_commands_first() {
    let mut app = with_commands();
    app.layout_mut().tiles.retain(|t| t.conversation == ALPHA);
    let view = &app.views[ALPHA];
    assert_eq!(view.running_commands(), 2);
    let order: Vec<&str> = view.commands().map(|(_, c)| c.call_id.as_str()).collect();
    assert_eq!(order, ["t4", "t3", "t2", "t1"]);

    let _ = app.update(Message::ShowCommands(true));
    assert!(app.commands_panel(app.main_window()).is_some());
    let idx = app.views[ALPHA].commands().next().unwrap().0;
    let _ = app.update(Message::ToggleCommand(ALPHA.to_string(), idx));
    assert!(app.views[ALPHA].commands_open.contains(&idx));
}

#[test]
fn the_commands_panel_resizes_from_its_edge_and_closes() {
    let mut app = with_commands();
    app.layout_mut().tiles.retain(|t| t.conversation == ALPHA);
    app.layout_mut().commands_open = true;
    let mut h = Harness::new(app);
    let edge = WINDOW.width - COMMANDS_WIDTH - HANDLE_WIDTH / 2.0;

    h.move_to(edge, 400.0);
    h.send(vec![button(true)]);
    assert!(
        matches!(h.app.layout().drag, Some(Drag::Commands { .. })),
        "pressing the edge grabs it"
    );
    let _ = h.app.update(Message::DragTo(edge));
    let _ = h.app.update(Message::DragTo(edge - 100.0));
    let _ = h.app.update(Message::DragEnd);
    assert_eq!(h.app.layout_mut().commands_width, COMMANDS_WIDTH + 100.0);

    // The close button sits at the panel's top right.
    h.move_to(WINDOW.width - 24.0, 24.0);
    h.send(vec![button(true), button(false)]);
    assert!(
        !h.app.layout_mut().commands_open,
        "the close button closes it"
    );

    h.app.layout_mut().commands_open = true;
    let _ = h.app.update(Message::StartCommandsDrag);
    let _ = h.app.update(Message::DragTo(edge - 100.0));
    let _ = h.app.update(Message::DragTo(WINDOW.width - 140.0));
    let _ = h.app.update(Message::DragEnd);
    assert!(
        !h.app.layout_mut().commands_open,
        "dragged nearly shut, it closes"
    );
}

#[test]
fn deleting_asks_first_then_drops_the_conversation_everywhere() {
    let mut h = Harness::new(two_tiles());
    let _ = h.app.update(Message::AskDelete(BRAVO.to_string()));
    assert!(h.app.dialog().is_some(), "it asks first");
    h.snapshot_dialog("delete-dialog");
    let _ = h.app.update(Message::CancelDelete);
    assert!(h.app.dialog().is_none());
    assert!(h.app.info(BRAVO).is_some(), "cancelling keeps it");

    let _ = h.app.update(Message::AskDelete(BRAVO.to_string()));
    let _ = h.app.update(Message::ConfirmDelete);
    assert!(h.app.info(BRAVO).is_none());
    assert!(!h.app.views.contains_key(BRAVO));
    assert_eq!(h.app.layout_mut().tiles.len(), 1, "its tile closed");

    // The last tile takes the next conversation rather than closing.
    let _ = h.app.update(Message::AskDelete(ALPHA.to_string()));
    let _ = h.app.update(Message::ConfirmDelete);
    assert!(h.app.conversations.is_empty());
    assert!(!h.app.views.contains_key(ALPHA));
}

#[test]
fn archiving_puts_a_conversation_away_without_deleting_it() {
    let mut h = Harness::new(two_tiles());
    let _ = h.app.update(Message::Archive(BRAVO.to_string(), true));
    assert!(
        h.app.info(BRAVO).is_some_and(|c| c.archived),
        "kept, marked"
    );
    assert_eq!(h.app.layout_mut().tiles.len(), 1, "its tile closed");

    let _ = h.app.update(Message::ToggleArchived);
    h.snapshot("archived");

    // With nothing else left unarchived, the last tile asks for a new conversation.
    let _ = h.app.update(Message::Archive(ALPHA.to_string(), true));
    assert!(
        !h.app.pending.is_empty(),
        "a new conversation was requested"
    );

    let _ = h.app.update(Message::Archive(BRAVO.to_string(), false));
    assert!(h.app.info(BRAVO).is_some_and(|c| !c.archived));
}
