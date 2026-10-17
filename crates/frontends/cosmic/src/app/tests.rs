//! Drives the COSMIC view headlessly: events go to a real widget tree, the messages
//! they produce go to `update`, and the tree's state carries over between frames as
//! it does in the running app. Run with `cargo test -p claudo-cosmic --features cosmic`.

use super::*;
use claudo_protocol::{AuthStatus, Background, TaskStatus};
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

/// Typing one character, as a keyboard sends it.
fn typed(c: char) -> Event {
    let text = c.to_string();
    Event::Keyboard(keyboard::Event::KeyPressed {
        key: keyboard::Key::Character(text.clone().into()),
        modified_key: keyboard::Key::Character(text.clone().into()),
        physical_key: keyboard::key::Physical::Unidentified(
            keyboard::key::NativeCode::Unidentified,
        ),
        location: keyboard::Location::Standard,
        modifiers: keyboard::Modifiers::empty(),
        text: Some(text.into()),
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
    app.restore.clear();
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
            parent: None,
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

    let mut app = two_tiles();
    let main = app.main_window();
    let _ = app.update(Message::In(
        main,
        Box::new(moved(BRAVO, carry::Destination::NewWindow)),
    ));
    let (second, _) = app.tile_of(BRAVO).unwrap();
    let mut h = Harness::new(app);
    let window = h.app.view_window(second);
    render_alone(&mut h.renderer, window, "second-window");

    let (mut app, _, second) = two_windows();
    app.conversations.push(ConversationInfo {
        id: "ccccc333-0000-4000-8000-000000000000".to_string(),
        title: "Charlie".to_string(),
        cwd: "/tmp".to_string(),
        attached: false,
        archived: false,
        overrides: Overrides::default(),
        context: Default::default(),
    });
    let mut h = Harness::new(app);
    h.snapshot("windows-main");
    let window = h.app.view_window(second);
    render_alone(&mut h.renderer, window, "windows-second");

    let mut app = two_tiles();
    app.conversations[0].overrides = Overrides {
        model: Some("haiku".to_string()),
        effort: Some(claudo_protocol::Effort::High),
        profile: None,
    };
    Harness::new(app).snapshot("custom-settings");

    let mut app = two_tiles();
    let main = app.main_window();
    let x = app.tile_room(main).0 + 560.0;
    let _ = app.update(Message::In(main, Box::new(Message::DropHover(x))));
    Harness::new(app).snapshot("drop-marker");

    let mut app = two_tiles();
    let _ = app.update(Message::ShowSessions);
    let home = std::env::var("HOME").unwrap_or_default();
    app.sessions = Some(vec![
        session(
            "Replace QuantLevel with a nested list of scale levels",
            "Claude Desktop",
            &format!("{home}/prog/cubek"),
            now - 3 * 3600,
        ),
        session(
            "nvidia-smi broken after update",
            "Claude Desktop",
            &format!(
                "{home}/.config/Claude/scratch-workspaces/70e96d5e-cde6-4590-aef3-a4258baaa080/scratch-2026-10-04"
            ),
            now - 30 * 3600,
        ),
        session(
            "GitHub CLI signature verification error",
            "Claude Code",
            &format!("{home}/dotfiles"),
            now - 9 * 86_400,
        ),
    ]);
    Harness::new(app).snapshot("sessions");

    Harness::new(with_runs(6, 3)).snapshot("tool-groups");

    let mut app = two_tiles();
    app.folders = ["prog/claudo", "prog/burn", "dotfiles"]
        .map(|folder| format!("{home}/{folder}"))
        .to_vec();
    for (info, folder) in app
        .conversations
        .iter_mut()
        .zip(["prog/cubek", "prog/claudo"])
    {
        info.cwd = format!("{home}/{folder}");
        info.attached = true;
    }
    app.views
        .insert(BRAVO.to_string(), ConversationView::new(""));
    Harness::new(app).snapshot("folders");

    let mut app = with_runs(6, 3);
    let view = app.views.get_mut(ALPHA).unwrap();
    view.queued.push((
        "Also run the tests once you are done".to_string(),
        Vec::new(),
    ));
    view.draft = text_editor::Content::with_text("and the docs");
    Harness::new(app).snapshot("queued");

    Harness::new(with_subagent()).snapshot("subagent");
}

/// Alpha with a finished run of `done` calls, then a running one whose last call is going.
fn with_runs(done: usize, running: usize) -> ClaudoApp {
    let mut app = two_tiles();
    let view = app.views.get_mut(ALPHA).unwrap();
    let commands = [
        "git status",
        "cat crates/cubecl-runtime/src/tune.rs",
        "rg 'fn remove'",
    ];
    for n in 0..done {
        let mut call = bash(&format!("d{n}"), commands[n % 3], "Look");
        call.result = Some("ok".to_string());
        view.items.push(Entry::Tool { call, open: false });
    }
    view.items.push(Entry::Reply(Box::new(Reply::complete(
        "Now the tests.".to_string(),
    ))));
    for n in 0..running {
        let mut call = bash(&format!("r{n}"), commands[n % 3], "Look");
        if n + 1 < running {
            call.result = Some("ok".to_string());
        }
        view.items.push(Entry::Tool { call, open: false });
    }
    view.streaming = true;
    app
}

fn session(title: &str, source: &str, cwd: &str, updated: u64) -> SessionInfo {
    SessionInfo {
        session_id: title.to_string(),
        title: title.to_string(),
        cwd: cwd.to_string(),
        source: source.to_string(),
        updated,
    }
}

#[test]
fn calls_made_one_after_another_fold_into_one_row_that_opens() {
    let app = with_runs(6, 3);
    let first = app.views[ALPHA].items.len() - 3 - 1 - 6;
    let mut h = Harness::new(app);
    h.move_to(560.0, 400.0);
    let _ = h.app.update(Message::ToggleGroup(ALPHA.to_string(), first));
    assert!(h.app.views[ALPHA].groups_open.contains(&first));
    h.move_to(561.0, 400.0);
    let _ = h.app.update(Message::ToggleGroup(ALPHA.to_string(), first));
    assert!(h.app.views[ALPHA].groups_open.is_empty());
    h.move_to(562.0, 400.0);
}

#[test]
fn a_file_attached_above_the_message_box_keeps_the_box_working() {
    let mut h = Harness::new(two_tiles());
    h.move_to(560.0, 400.0);
    let image = claudo_protocol::Attachment::new("pasted.png".to_string(), "image/png", &[1, 2, 3]);
    let _ = h
        .app
        .update(Message::Attach(ALPHA.to_string(), Ok(vec![image])));
    h.move_to(561.0, 400.0);
    h.send(vec![typed('a')]);
    let _ = h.app.update(Message::Detach(ALPHA.to_string(), 0));
    h.move_to(562.0, 400.0);
}

#[test]
fn attached_files_go_with_the_next_message_even_without_text() {
    let mut app = two_tiles();
    let image = claudo_protocol::Attachment::new("pasted.png".to_string(), "image/png", &[1, 2, 3]);
    let _ = app.update(Message::Attach(ALPHA.to_string(), Ok(vec![image])));
    assert!(app.views[ALPHA].can_send(), "an image alone is a message");
    let before = app.views[ALPHA].items.len();
    let _ = app.update(Message::Send(ALPHA.to_string()));
    let view = &app.views[ALPHA];
    assert!(view.attachments.is_empty());
    assert_eq!(
        view.items.len(),
        before + 1,
        "no empty bubble for the missing text"
    );
    assert!(matches!(view.items.last(), Some(Entry::File(_))));

    let _ = app.update(Message::Attach(
        BRAVO.to_string(),
        Err("blob.bin is not an image, a PDF or text".to_string()),
    ));
    assert!(
        app.views[BRAVO].error.is_some(),
        "the reason shows in its tile"
    );
}

#[test]
fn a_session_continued_from_the_sessions_page_opens_with_its_history() {
    let mut app = two_tiles();
    let _ = app.update(Message::ShowSessions);
    assert_eq!(app.layout().page, Page::Sessions);
    assert!(app.sessions.is_none(), "found anew each time");
    let _ = app.update(Message::Events(vec![ServerEvent::Sessions {
        list: vec![session("Quantization review", "Claude Desktop", "/tmp", 0)],
    }]));
    let _ = app.update(Message::Import("Quantization review".to_string()));
    assert_eq!(app.layout().page, Page::Chat);
    let charlie = "ccccc333-0000-4000-8000-000000000000";
    let _ = app.update(Message::Events(vec![ServerEvent::Created {
        conversation_id: charlie.to_string(),
        request: app.next_request - 1,
        imported: true,
    }]));
    assert!(
        app.views[charlie].loading,
        "its history is on its way, not a new chat"
    );
    let main = app.main_window();
    assert_eq!(app.focused(main).map(String::as_str), Some(charlie));
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
        parent: None,
    }
}

/// Alpha after its turn ended, with a subagent still working in the background: its
/// first calls returned, its last runs.
fn with_subagent() -> ClaudoApp {
    let mut app = two_tiles();
    let view = app.views.get_mut(ALPHA).unwrap();
    let mut agent = bash("agent", "", "");
    agent.name = "Agent".to_string();
    agent.summary = "Verify the review's findings".to_string();
    agent.command = None;
    agent.result = Some("Async agent launched".to_string());
    agent.background = Some(Background {
        status: TaskStatus::Running,
        output: None,
    });
    view.items.push(Entry::Tool {
        call: agent,
        open: false,
    });
    view.items.push(Entry::Reply(Box::new(Reply::complete(
        "A subagent is checking the findings; I will report when it is done.".to_string(),
    ))));
    let commands = [
        "git diff main",
        "rg 'fn tune'",
        "cargo test -p cubecl-runtime",
    ];
    let events = commands
        .iter()
        .enumerate()
        .flat_map(|(n, command)| {
            let mut call = bash(&format!("s{n}"), command, "Check");
            call.parent = Some("agent".to_string());
            let started = ServerEvent::ToolStarted {
                conversation_id: ALPHA.to_string(),
                call,
            };
            let returned = (n + 1 < commands.len()).then(|| ServerEvent::ToolResult {
                conversation_id: ALPHA.to_string(),
                call_id: format!("s{n}"),
                content: "ok".to_string(),
                is_error: false,
            });
            std::iter::once(started).chain(returned)
        })
        .collect();
    let _ = app.update(Message::Events(events));
    app
}

/// Alpha, mid-turn, with a command running in the foreground and one in the background,
/// and two that ended.
fn with_commands() -> ClaudoApp {
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
fn a_conversation_moves_to_a_window_of_its_own() {
    let mut app = two_tiles();
    let main = app.main_window();
    let _ = app.update(Message::In(
        main,
        Box::new(moved(BRAVO, carry::Destination::NewWindow)),
    ));
    assert_eq!(app.windows.len(), 2);
    let (second, _) = app.tile_of(BRAVO).expect("Bravo is shown");
    assert_ne!(second, main, "in the new window");
    assert_eq!(
        app.window(main).tiles.len(),
        1,
        "and no longer in the first"
    );

    // Its only tile closing closes it; the agent's view stays while a turn runs.
    app.views.get_mut(BRAVO).unwrap().streaming = true;
    let _ = app.update(Message::In(
        second,
        Box::new(Message::Close(BRAVO.to_string())),
    ));
    assert!(
        !app.windows.contains_key(&second),
        "its last tile closed the window"
    );
    assert!(
        app.views.contains_key(BRAVO),
        "a running agent keeps its view"
    );

    // Closing the main window hands its place to another.
    let _ = app.update(Message::In(
        main,
        Box::new(moved(BRAVO, carry::Destination::NewWindow)),
    ));
    let (second, _) = app.tile_of(BRAVO).unwrap();
    let _ = app.update(Message::WindowClosed(main));
    assert_eq!(
        app.main_window(),
        second,
        "the other window becomes the main one"
    );
    assert!(app.tile_of(ALPHA).is_none() && app.tile_of(BRAVO).is_some());
}

/// A tile's move to another window, as it arrives once its transcript's scroll is read.
fn moved(conversation: &str, to: carry::Destination) -> Message {
    Message::Carried(conversation.to_string(), to, None)
}

/// Alpha in the main window and Bravo in a second one, both the test window's size.
fn two_windows() -> (ClaudoApp, window::Id, window::Id) {
    let mut app = two_tiles();
    let main = app.main_window();
    let _ = app.update(Message::In(
        main,
        Box::new(moved(BRAVO, carry::Destination::NewWindow)),
    ));
    let (second, _) = app.tile_of(BRAVO).unwrap();
    app.windows.get_mut(&second).unwrap().size = Some((WINDOW.width, WINDOW.height));
    (app, main, second)
}

#[test]
fn each_window_has_its_own_color_once_there_are_several() {
    let app = two_tiles();
    assert!(
        app.window_color(app.main_window()).is_none(),
        "one window needs none"
    );
    let (app, main, second) = two_windows();
    let (a, b) = (app.window_color(main), app.window_color(second));
    assert!(a.is_some() && b.is_some() && a != b);
}

#[test]
fn a_tile_keeps_its_scroll_when_it_moves_to_another_window() {
    let mut h = Harness::new(two_tiles());
    h.move_to(560.0, 400.0);
    h.send(vec![wheel(10.0)]);
    let scrolled = h.offsets()[0];

    use cosmic::iced::advanced::widget::operation::{Outcome, black_box};
    let mut read = carry::read_scroll(ALPHA);
    h.frame(|ui, renderer| ui.operate(renderer, &mut black_box(&mut read)));
    let Outcome::Some(Some(from_bottom)) = read.finish() else {
        panic!("Alpha's transcript is found by its tile");
    };
    assert!(from_bottom > 0.0, "scrolled up from the bottom");

    // The new window starts the transcript over at the bottom; the restore puts it back.
    h.send(vec![wheel(-1000.0)]);
    assert_ne!(h.offsets()[0], scrolled);
    let mut back = carry::scroll_back(ALPHA, from_bottom);
    h.frame(|ui, renderer| ui.operate(renderer, &mut black_box(&mut back)));
    assert!((h.offsets()[0] - scrolled).abs() < 1.0, "back where it was");

    // The move itself waits for the restore until the window has drawn the tile.
    let main = h.app.main_window();
    let _ = h.app.update(Message::In(
        main,
        Box::new(Message::Carried(
            ALPHA.to_string(),
            carry::Destination::NewWindow,
            Some(from_bottom),
        )),
    ));
    assert!(h.app.tile_of(ALPHA).is_some_and(|(w, _)| w != main));
    assert_eq!(h.app.carried_scroll.get(ALPHA), Some(&from_bottom));
    let _ = h
        .app
        .update(Message::ScrollRestored(ALPHA.to_string(), true));
    assert!(h.app.carried_scroll.is_empty());
}

#[test]
fn tiles_move_to_windows_already_open() {
    let (mut app, main, second) = two_windows();
    let names: Vec<String> = app
        .other_windows(main)
        .into_iter()
        .map(|(_, n)| n)
        .collect();
    assert_eq!(names, ["window 2 · Bravo"]);

    let _ = app.update(Message::In(
        main,
        Box::new(moved(ALPHA, carry::Destination::Window(second))),
    ));
    let ids = |app: &ClaudoApp, w| -> Vec<String> {
        app.window(w)
            .tiles
            .iter()
            .map(|t| t.conversation.clone())
            .collect()
    };
    assert_eq!(ids(&app, second), [BRAVO, ALPHA]);
    assert!(
        app.window(main).tiles.is_empty(),
        "the main window waits for a new chat"
    );
    assert_eq!(app.pending.values().next().map(|(w, _)| *w), Some(main));
}

#[test]
fn a_tile_dropped_on_another_window_lands_where_it_is_dropped() {
    let (mut app, main, second) = two_windows();
    let _ = app.update(Message::In(
        main,
        Box::new(Message::OpenTile(BRAVO.to_string())),
    ));
    let _ = app.update(Message::In(
        second,
        Box::new(moved(ALPHA, carry::Destination::NewWindow)),
    ));
    // Alpha alone in a third window now; drop it at the left edge of Bravo's window.
    let x = app.tile_room(second).0 + 10.0;
    let _ = app.update(moved(ALPHA, carry::Destination::Drop { window: second, x }));
    let ids: Vec<&str> = app
        .window(second)
        .tiles
        .iter()
        .map(|t| t.conversation.as_str())
        .collect();
    assert_eq!(
        ids,
        [ALPHA, BRAVO],
        "it lands left of the tile it was dropped before"
    );
    assert_eq!(
        app.windows.len(),
        2,
        "the window it left had no other tile, so it closed"
    );

    // Dropped back in its own window past the other tile, it moves right.
    let _ = app.update(Message::In(
        second,
        Box::new(Message::TileDropped {
            conversation: ALPHA.to_string(),
            x: WINDOW.width - 20.0,
        }),
    ));
    let ids: Vec<&str> = app
        .window(second)
        .tiles
        .iter()
        .map(|t| t.conversation.as_str())
        .collect();
    assert_eq!(ids, [BRAVO, ALPHA]);
}

#[test]
fn a_tile_dropped_outside_every_window_gets_its_own() {
    let mut app = two_tiles();
    let main = app.main_window();
    let _ = app.update(Message::In(
        main,
        Box::new(Message::StartMove(BRAVO.to_string())),
    ));
    let _ = app.update(Message::In(main, Box::new(Message::TileAway)));
    assert!(matches!(
        app.window(main).drag,
        Some(Drag::Tile {
            away: true,
            at: None,
            ..
        })
    ));
    let _ = app.update(Message::DragEnd);
    assert!(
        app.window(main).drag.is_some(),
        "a release does not end a drag the compositor carries"
    );
    let _ = app.update(Message::In(
        main,
        Box::new(Message::TileDroppedOutside(BRAVO.to_string())),
    ));
    assert!(app.window(main).drag.is_none());
    let _ = app.update(moved(BRAVO, carry::Destination::NewWindow));
    assert!(app.tile_of(BRAVO).is_some_and(|(w, _)| w != main));
}

#[test]
fn a_new_chat_can_open_in_a_new_window() {
    let mut app = two_tiles();
    let main = app.main_window();
    let _ = app.update(Message::In(main, Box::new(Message::NewWindow)));
    assert_eq!(app.windows.len(), 2);
    let (w, _) = *app
        .pending
        .values()
        .next()
        .expect("a chat is being created");
    assert_ne!(w, main, "for the new window");
}

#[test]
fn windows_reopen_as_they_were_left() {
    let _ = two_tiles();
    let conversations = || {
        [ALPHA, BRAVO].map(|id| ConversationInfo {
            id: id.to_string(),
            title: id.to_string(),
            cwd: "/tmp".to_string(),
            attached: false,
            archived: false,
            overrides: Overrides::default(),
            context: Default::default(),
        })
    };
    let saved = |id: &str| SavedWindow {
        tiles: vec![Tile::new(id.to_string())],
        ..SavedWindow::default()
    };
    let ui = UiState {
        tiles: vec![Tile::new(ALPHA.to_string())],
        windows: vec![saved(BRAVO), saved("gone")],
        ..UiState::default()
    };
    let (mut app, _) = ClaudoApp::init(Core::default(), ui);
    assert_eq!(app.windows.len(), 3, "both saved windows open at launch");
    app.conversations = conversations().to_vec();
    app.restore_tiles();

    let main = app.main_window();
    assert_eq!(app.tile_of(ALPHA).map(|(w, _)| w), Some(main));
    assert!(app.tile_of(BRAVO).is_some_and(|(w, _)| w != main));
    assert_eq!(
        app.windows.len(),
        2,
        "the window whose conversation is gone closes"
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
fn several_conversations_are_picked_and_deleted_at_once() {
    let mut app = two_tiles();
    let charlie = "ccccc333-0000-4000-8000-000000000000";
    app.conversations.push(ConversationInfo {
        id: charlie.to_string(),
        title: "Charlie".to_string(),
        cwd: "/tmp".to_string(),
        attached: false,
        archived: false,
        overrides: Overrides::default(),
        context: Default::default(),
    });
    let mut h = Harness::new(app);

    // Ctrl+click starts picking; then a plain click picks too, and a second one lets go.
    let _ = h.app.update(Message::Modifiers(keyboard::Modifiers::CTRL));
    let _ = h.app.update(Message::Open(BRAVO.to_string()));
    let _ = h
        .app
        .update(Message::Modifiers(keyboard::Modifiers::empty()));
    let _ = h.app.update(Message::Open(charlie.to_string()));
    let _ = h.app.update(Message::Open(ALPHA.to_string()));
    let _ = h.app.update(Message::Open(ALPHA.to_string()));
    assert_eq!(h.app.picked, [BRAVO, charlie]);
    assert!(h.app.tile_of(charlie).is_none(), "picking opens nothing");
    h.snapshot("picked");

    let _ = h.app.update(Message::AskDeletePicked);
    h.snapshot_dialog("delete-picked");
    let _ = h.app.update(Message::CancelDelete);
    assert_eq!(
        h.app.picked.len(),
        2,
        "cancelling the dialog keeps them picked"
    );
    let _ = h.app.update(Message::CancelDelete);
    assert!(h.app.picked.is_empty(), "a second Escape lets go of them");

    let _ = h.app.update(Message::Pick(BRAVO.to_string()));
    let _ = h.app.update(Message::Pick(charlie.to_string()));
    let _ = h.app.update(Message::AskDeletePicked);
    let _ = h.app.update(Message::ConfirmDelete);
    let left: Vec<&str> = h.app.conversations.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(left, [ALPHA]);
    assert!(h.app.picked.is_empty());
}

#[test]
fn shift_picks_every_conversation_up_to_the_one_clicked() {
    let mut app = two_tiles();
    for (id, title) in [("c", "Charlie"), ("d", "Delta")] {
        app.conversations.push(ConversationInfo {
            id: id.to_string(),
            title: title.to_string(),
            cwd: "/tmp".to_string(),
            attached: false,
            archived: false,
            overrides: Overrides::default(),
            context: Default::default(),
        });
    }
    let _ = app.update(Message::Pick("d".to_string()));
    let _ = app.update(Message::Modifiers(keyboard::Modifiers::SHIFT));
    let _ = app.update(Message::Open(BRAVO.to_string()));
    assert_eq!(app.picked, ["d", BRAVO, "c"]);
}

fn cli_command(name: &str, hint: &str) -> CliCommand {
    CliCommand {
        name: name.to_string(),
        description: format!("What /{name} does"),
        argument_hint: hint.to_string(),
    }
}

#[test]
fn a_conversation_being_loaded_is_not_shown_as_a_new_chat() {
    let mut app = two_tiles();
    let charlie = "ccccc333-0000-4000-8000-000000000000";
    app.conversations.push(ConversationInfo {
        id: charlie.to_string(),
        title: "Charlie".to_string(),
        cwd: "/tmp".to_string(),
        attached: false,
        archived: false,
        overrides: Overrides::default(),
        context: Default::default(),
    });
    let _ = app.update(Message::Open(charlie.to_string()));
    assert!(app.views[charlie].loading, "its messages are on their way");
    let _ = app.update(Message::Events(vec![ServerEvent::History {
        conversation_id: charlie.to_string(),
        items: vec![Item::User("hi".to_string())],
    }]));
    assert!(!app.views[charlie].loading);
    assert_eq!(app.views[charlie].items.len(), 1);
}

#[test]
fn the_message_box_keeps_focus_while_typing() {
    for words in ["hello there", "/compact now"] {
        let mut app = two_tiles();
        app.cli_commands = vec![cli_command(
            "compact",
            "<optional custom summarization instructions>",
        )];
        let mut h = Harness::new(app);
        h.focus(composer_id(ALPHA));
        for c in words.chars() {
            h.send(vec![typed(c)]);
        }
        assert_eq!(
            h.app.views[ALPHA].draft.text(),
            words,
            "every key reached the message box"
        );
    }
}

#[test]
fn the_slash_menu_lists_commands_and_runs_claudos_own() {
    use keyboard::key::{Code, Named};
    let mut app = two_tiles();
    app.cli_commands = vec![
        cli_command("compact", "<optional custom summarization instructions>"),
        cli_command("context", ""),
        cli_command("effort", "<low|medium|high|xhigh|max|auto>"),
        cli_command("fast", "[on|off]"),
        cli_command("clear", "[name]"),
    ];
    app.views.get_mut(ALPHA).unwrap().draft = text_editor::Content::with_text("/c");
    let mut h = Harness::new(app);
    h.focus(composer_id(ALPHA));
    let names = |h: &Harness| -> Vec<String> {
        h.app
            .command_matches(&h.app.views[ALPHA])
            .iter()
            .map(|c| c.name.clone())
            .collect()
    };
    assert_eq!(
        names(&h),
        ["compact", "context", "clear"],
        "starting with it first; /fast is hidden"
    );
    h.snapshot("slash-menu");
    h.app.views.get_mut(BRAVO).unwrap().items.clear();
    h.app.views.get_mut(BRAVO).unwrap().draft = text_editor::Content::with_text("/co");
    h.snapshot("slash-menu-new-chat");
    h.app.views.get_mut(BRAVO).unwrap().draft = text_editor::Content::new();

    let none = keyboard::Modifiers::empty();
    h.send(vec![key(Named::ArrowDown, Code::ArrowDown, none)]);
    assert_eq!(h.app.views[ALPHA].menu, 1);
    h.send(vec![key(Named::Tab, Code::Tab, none)]);
    assert_eq!(
        h.app.views[ALPHA].draft.text(),
        "/context ",
        "Tab completes the highlighted one"
    );
    assert!(
        names(&h).is_empty(),
        "the menu closes once the name is complete"
    );

    // A command claudo runs itself never reaches the CLI.
    h.app.views.get_mut(ALPHA).unwrap().draft = text_editor::Content::with_text("/effort high");
    let _ = h.app.update(Message::Send(ALPHA.to_string()));
    assert_eq!(
        h.app.info(ALPHA).unwrap().overrides.effort,
        Some(claudo_protocol::Effort::High)
    );
    assert!(!h.app.views[ALPHA].streaming);
    assert_eq!(h.app.views[ALPHA].draft.text(), "");

    // Escape closes the menu for that command.
    h.app.views.get_mut(ALPHA).unwrap().draft = text_editor::Content::with_text("/co");
    let _ = h.app.update(Message::MenuClose(ALPHA.to_string()));
    assert!(names(&h).is_empty());
}

#[test]
fn a_long_conversation_offers_to_compact_and_shows_where_it_did() {
    let mut app = two_tiles();
    app.conversations[0].context = claudo_protocol::ContextUsage {
        used: 612_000,
        window: Some(1_000_000),
    };
    let view = &app.views[ALPHA];
    assert!(app.compact_offer(&ALPHA.to_string(), view).is_some());
    assert!(
        app.compact_offer(&BRAVO.to_string(), &app.views[BRAVO])
            .is_none()
    );
    Harness::new(app).snapshot("compact-offer");

    let mut app = two_tiles();
    let event = |e| Message::Events(vec![e]);
    let alpha = || ALPHA.to_string();
    let _ = app.update(event(ServerEvent::Compacting {
        conversation_id: alpha(),
    }));
    app.views.get_mut(ALPHA).unwrap().streaming = true;
    assert_eq!(
        app.views[ALPHA].activity().as_deref(),
        Some("Compacting the conversation")
    );
    let _ = app.update(event(ServerEvent::Compacted {
        conversation_id: alpha(),
        compaction: Compaction {
            auto: false,
            before: 612_000,
            after: 9_400,
            summary: String::new(),
        },
    }));
    let _ = app.update(event(ServerEvent::CompactSummary {
        conversation_id: alpha(),
        text: "The user and Claude built a COSMIC client.".to_string(),
    }));
    let Some(Entry::Compacted { compaction, .. }) = app.views[ALPHA].items.last() else {
        panic!("a compaction row");
    };
    assert_eq!(
        compaction.summary,
        "The user and Claude built a COSMIC client."
    );
    let idx = app.views[ALPHA].items.len() - 1;
    let _ = app.update(Message::Toggle(alpha(), idx));
    Harness::new(app).snapshot("compacted");
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

/// Streams replies into three long transcripts and prints what each frame costs.
#[test]
#[ignore = "a profile, not a check; run with --ignored --nocapture"]
fn streaming_frame_cost() {
    use cosmic::iced::theme::Base;
    use std::time::{Duration, Instant};

    const CHARLIE: &str = "ccccc333-0000-4000-8000-000000000000";
    let reply = "Here is what changed:\n\n- **Parsing** moved into `core`, so the frontend only renders.\n\
                 - The `reveal` animation runs per word.\n\n```rust\nfn main() {\n    let x = compute(42);\n    \
                 println!(\"{x}\");\n}\n```\n\nThat keeps the hot path short. ";
    let mut app = two_tiles();
    app.conversations.push(ConversationInfo {
        id: CHARLIE.to_string(),
        title: "Charlie".to_string(),
        cwd: "/tmp".to_string(),
        attached: false,
        archived: false,
        overrides: Overrides::default(),
        context: Default::default(),
    });
    app.views
        .insert(CHARLIE.to_string(), ConversationView::new(""));
    app.layout_mut().tiles.push(Tile::new(CHARLIE.to_string()));
    for view in app.views.values_mut() {
        view.items.clear();
        let turns = std::env::var("TURNS").map_or(40, |t| t.parse().unwrap());
        for n in 0..turns {
            view.items.push(Entry::User(format!("Question {n}")));
            view.items
                .push(Entry::Reply(Box::new(Reply::complete(reply.repeat(3)))));
        }
        view.items.push(Entry::User("Go on".to_string()));
        view.streaming = true;
    }
    let mut h = Harness::new(app);
    let theme = cosmic::theme::active();
    let style = cosmic::iced::advanced::renderer::Style {
        text_color: theme.base().text_color,
        icon_color: theme.base().text_color,
        scale_factor: 1.0,
    };
    let ids = [ALPHA, BRAVO, CHARLIE];
    let (mut view_t, mut build_t, mut draw_t) = (Duration::ZERO, Duration::ZERO, Duration::ZERO);
    let frames = std::env::var("FRAMES").map_or(600, |f| f.parse().unwrap());
    for frame in 0..frames {
        // A reply of about 2.5 KB ends and the next turn starts, as in a conversation.
        if frame % 80 == 79 {
            for id in ids {
                let _ = h.app.update(Message::Events(vec![ServerEvent::Completed {
                    conversation_id: id.to_string(),
                    is_error: false,
                    context: Default::default(),
                    time: None,
                }]));
                let view = h.app.views.get_mut(id).unwrap();
                view.items.push(Entry::User("Go on".to_string()));
                view.streaming = true;
            }
        }
        if frame % 8 == 0 {
            let evs = ids
                .iter()
                .map(|id| ServerEvent::Delta {
                    conversation_id: id.to_string(),
                    text: reply.to_string(),
                })
                .collect();
            let _ = h.app.update(Message::Events(evs));
        }
        let _ = h.app.update(Message::Frame);
        let cache = std::mem::take(&mut h.cache);
        let t0 = Instant::now();
        let element = h.app.view();
        let t1 = Instant::now();
        let mut ui = UserInterface::build(element, WINDOW, cache, &mut h.renderer);
        let t2 = Instant::now();
        ui.draw(&mut h.renderer, &theme, &style, mouse::Cursor::Unavailable);
        let t3 = Instant::now();
        h.cache = ui.into_cache();
        view_t += t1 - t0;
        build_t += t2 - t1;
        draw_t += t3 - t2;
    }
    let per = |d: Duration| d.as_secs_f64() * 1000.0 / frames as f64;
    println!(
        "per frame: view {:.2} ms, build and layout {:.2} ms, draw {:.2} ms",
        per(view_t),
        per(build_t),
        per(draw_t)
    );
}

#[test]
fn a_message_sent_while_claude_works_waits_then_lands_where_claude_took_it() {
    let mut app = two_tiles();
    let send = |app: &mut ClaudoApp, text: &str| {
        app.views.get_mut(ALPHA).unwrap().draft = text_editor::Content::with_text(text);
        let _ = app.update(Message::Send(ALPHA.to_string()));
    };
    app.views.get_mut(ALPHA).unwrap().streaming = true;
    let before = app.views[ALPHA].items.len();
    send(&mut app, "also check the tests");
    assert_eq!(
        app.views[ALPHA].items.len(),
        before,
        "not in the transcript yet"
    );
    assert_eq!(app.views[ALPHA].queued.len(), 1);

    let alpha = || ALPHA.to_string();
    let _ = app.update(Message::Events(vec![ServerEvent::Delivered {
        conversation_id: alpha(),
    }]));
    let view = &app.views[ALPHA];
    assert!(view.queued.is_empty());
    assert!(matches!(view.items.last(), Some(Entry::User(text)) if text == "also check the tests"));
    assert!(view.streaming);

    send(&mut app, "one more");
    let _ = app.update(Message::Events(vec![ServerEvent::Error {
        conversation_id: Some(alpha()),
        detail: "claude exited".to_string(),
    }]));
    let view = &app.views[ALPHA];
    assert!(view.queued.is_empty());
    assert_eq!(
        view.draft.text().trim(),
        "one more",
        "back in the message box"
    );
}

#[test]
fn a_background_subagents_calls_run_after_its_turn_and_nest_under_it() {
    let mut app = with_subagent();
    let view = &app.views[ALPHA];
    assert!(!view.streaming, "the turn that started it ended");
    let running: Vec<&str> = view
        .commands()
        .filter(|(_, call)| view.running(call))
        .map(|(_, call)| call.call_id.as_str())
        .collect();
    assert_eq!(running, ["s2"], "its last command runs on");
    assert_eq!(
        view.commands().count(),
        3,
        "every one is in the Commands panel"
    );
    assert!(
        matches!(&view.items[view.items.len() - 4], Entry::Reply(reply) if reply.text.contains("report")),
        "the reply stays whole"
    );

    let _ = app.update(Message::Events(vec![ServerEvent::Background {
        conversation_id: ALPHA.to_string(),
        call_id: "agent".to_string(),
        background: Background {
            status: TaskStatus::Completed,
            output: None,
        },
    }]));
    assert_eq!(
        app.views[ALPHA].running_commands(),
        0,
        "nothing outlives its subagent"
    );
}
