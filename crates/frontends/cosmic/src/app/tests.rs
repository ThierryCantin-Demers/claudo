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

    /// What the view answers `events` with, left for the caller to apply.
    fn messages(&mut self, events: Vec<Event>) -> Vec<Message> {
        let cursor = mouse::Cursor::Available(self.cursor);
        self.frame(|ui, renderer| {
            let mut messages = Vec::new();
            let _ = ui.update(
                &events,
                cursor,
                renderer,
                &mut clipboard::Null,
                &mut messages,
            );
            messages
        })
    }

    fn send(&mut self, events: Vec<Event>) {
        for message in self.messages(events) {
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

    let (mut app, _) = ClaudoApp::init(
        Core::default(),
        Flags {
            ui: UiState::default(),
            beside: false,
        },
    );
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
            agent: false,
            started: None,
            ended: None,
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

    let mut app = with_commands();
    app.layout_mut().sidebar_open = false;
    let _ = app.update(Message::ShowCommands(true));
    let _ = app.update(Message::ToggleFinished(ALPHA.to_string()));
    Harness::new(app).snapshot("commands-finished-folded");

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
    app.quota_asked = false;
    app.quota_at = Some(now - 180);
    let mut h = Harness::new(app);
    h.snapshot("usage");
    h.app.layout_mut().sidebar_width = SIDEBAR_MIN;
    h.app.zoom = 1.3;
    h.snapshot("usage-narrow");

    let mut app = two_tiles();
    let token = "ZVJBbtwwDPxKsfcKybW+Bf1F0QMtcW0iEmWQlDdG0b+Hjq1Ntr0IojhDDof6cxlzw0WITS8".repeat(4);
    app.views
        .get_mut(ALPHA)
        .expect("a view")
        .items
        .push(Entry::User(format!("my current build is {token}")));
    Harness::new(app).snapshot("long-word");

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

    let mut app = two_tiles();
    app.views.get_mut(ALPHA).unwrap().items.push(Entry::Reply(Box::new(Reply::complete(
        "<ruby>今日<rt>きょう</rt></ruby>は<ruby>漢字<rt>かんじ</rt></ruby>を<ruby>勉強<rt>べんきょう</rt></ruby>しましょう。\
         <ruby>日本語<rt>にほんご</rt></ruby>の<ruby>文<rt>ぶん</rt></ruby>は、**<ruby>振<rt>ふ</rt></ruby>り<ruby>仮名<rt>がな</rt></ruby>**があると\
         <ruby>読<rt>よ</rt></ruby>みやすくなります。Every line of a paragraph with readings is spaced for them.\n\n\
         In code it stays as written: `<ruby>断<rt>ことわ</rt></ruby>`\n\n\
         1. Kanji with kana after it: <ruby>断<rt>ことわ</rt></ruby>る\n\
         2. One ruby per kanji: <ruby>機<rt>き</rt>嫌<rt>げん</rt></ruby>\n\
         3. No readings in this one"
            .to_string(),
    ))));
    Harness::new(app).snapshot("furigana");

    let mut app = two_tiles();
    app.views
        .get_mut(ALPHA)
        .unwrap()
        .items
        .push(Entry::Reply(Box::new(Reply::complete(TABLE.to_string()))));
    Harness::new(app).snapshot("table");

    let mut app = with_subagent();
    app.layout_mut().sidebar_open = false;
    let _ = app.update(Message::ShowCommands(true));
    Harness::new(app).snapshot("commands-subagent");

    let mut app = with_subagent();
    app.layout_mut().sidebar_open = false;
    let _ = app.update(Message::ShowCommands(true));
    let idx = app.views[ALPHA].commands().next().unwrap().0;
    let _ = app.update(Message::ToggleCommand(ALPHA.to_string(), idx));
    Harness::new(app).snapshot("commands-subagent-open");
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
fn an_unsent_file_comes_back_with_its_conversation() {
    let mut app = two_tiles();
    app.conversations.push(ConversationInfo {
        id: "c".to_string(),
        title: "Charlie".to_string(),
        cwd: "/tmp".to_string(),
        attached: false,
        archived: false,
        overrides: Overrides::default(),
        context: Default::default(),
    });
    let image = claudo_protocol::Attachment::new("pasted.png".to_string(), "image/png", &[1, 2, 3]);
    let _ = app.update(Message::Attach(ALPHA.to_string(), Ok(vec![image])));
    app.layout_mut().focused = app.tile_of(ALPHA).expect("a tile").1;
    let _ = app.update(Message::Open("c".to_string()));
    assert!(!app.views.contains_key(ALPHA), "the view was let go");
    let _ = app.update(Message::Open(ALPHA.to_string()));
    assert_eq!(app.views[ALPHA].attachments.len(), 1);
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

const TABLE: &str = "A table:\n\n\
    | App | How it gets the flags |\n|---|---|\n\
    | Brave | Your existing Brave launcher script (renamed `28-brave-flags`) adds them beside the \
      password store setting: menu entries and the shell wrappers. |\n\
    | VS Code | New script `35-vscode-flags` rebuilds its two menu entries from the system ones \
      with the flags added. VS Code's `argv.json` refuses these switches. |\n\
    | Claude Desktop | Its own launcher config file sets `CLAUDE_GTK_IM_MODULE=fcitx`. |\n\n\
    The end.";

#[test]
fn a_table_fits_the_reply_and_wraps_its_long_cells() {
    use cosmic::iced::advanced::layout::Limits;
    use cosmic::iced::advanced::widget::Tree;
    let renderer = Harness::new(two_tiles()).renderer;
    let reply = Reply::complete(TABLE.to_string());
    let conversation = ALPHA.to_string();
    let viewer = selectable::Viewer {
        conversation: &conversation,
        entry: 0,
        selection: None,
        next: std::cell::Cell::new(0),
    };
    let mut element = markdown::view_with(reply.markdown.items(), markdown_settings(14.0), &viewer);
    let mut tree = Tree::new(element.as_widget());
    let limits = Limits::new(Size::ZERO, Size::new(600.0, f32::INFINITY));
    let size = element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits)
        .size();
    assert!(size.width <= 600.0, "{size:?}");
    assert!(size.height < 300.0, "{size:?}");
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

const SAMPLE: &str = "## A heading\n\nSome **bold** text and `inline code`, then a list:\n\n\
    - first point\n- second point\n\n```rust\nfn main() {\n    println!(\"hi\");\n}\n```\n\nThe end.";

#[test]
fn a_code_block_is_copied_whole_from_its_button() {
    let mut app = two_tiles();
    app.views
        .get_mut(ALPHA)
        .expect("Alpha")
        .items
        .push(Entry::Reply(Box::new(Reply::complete(SAMPLE.to_string()))));
    let mut h = Harness::new(app);
    h.move_to(795.0, 686.0);
    let cursor = mouse::Cursor::Available(h.cursor);
    let messages = h.frame(|ui, renderer| {
        let mut messages = Vec::new();
        let _ = ui.update(
            &[button(true), button(false)],
            cursor,
            renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        messages
    });
    let code = "fn main() {\n    println!(\"hi\");\n}";
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::In(_, m) if matches!(m.as_ref(),
                Message::Copied { text, primary: false } if text == code))),
        "none copying the block: {messages:?}"
    );
}

#[test]
#[ignore = "writes images; run with --ignored"]
fn selection_snapshot() {
    let mut app = two_tiles();
    let view = app.views.get_mut(ALPHA).expect("Alpha");
    view.items
        .push(Entry::Reply(Box::new(Reply::complete(SAMPLE.to_string()))));
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
fn up_and_down_recall_sent_prompts_from_an_empty_box() {
    use keyboard::key::{Code, Named};
    let long = "A prompt long enough to wrap over several lines of the message box, \
                so going up and down moves through it as it is laid out before going \
                on to the prompt before or after it. "
        .repeat(3);
    let mut app = two_tiles();
    let view = app.views.get_mut(ALPHA).expect("Alpha");
    view.items.push(Entry::User("try again".to_string()));
    view.items.push(Entry::User("try again".to_string()));
    view.items.push(Entry::User(long.clone()));
    view.items.push(Entry::User("first\nsecond".to_string()));
    view.draft = text_editor::Content::with_text(&long);
    view.draft
        .perform(text_editor::Action::Move(text_editor::Motion::DocumentEnd));
    let mut h = Harness::new(app);
    h.focus(composer_id(ALPHA));
    let none = keyboard::Modifiers::empty();
    let up = || key(Named::ArrowUp, Code::ArrowUp, none);
    let down = || key(Named::ArrowDown, Code::ArrowDown, none);
    let draft = |h: &Harness| h.app.views[ALPHA].draft.text();
    let cursor = |h: &Harness| h.app.views[ALPHA].draft.cursor().position;

    let end = cursor(&h);
    for _ in 0..6 {
        h.send(vec![up()]);
    }
    assert_eq!(draft(&h), long, "a draft is never swapped for a prompt");
    assert_ne!(cursor(&h), end, "Up moves up its wrapped lines");
    assert_eq!(h.app.views[ALPHA].recalled, 0);

    h.app.views.get_mut(ALPHA).unwrap().draft = text_editor::Content::new();
    h.send(vec![up()]);
    assert_eq!(draft(&h), "first\nsecond");
    h.send(vec![down()]);
    assert_eq!(
        draft(&h),
        "first\nsecond",
        "Down goes through the prompt's lines first"
    );
    h.send(vec![up()]);
    h.send(vec![up()]);
    assert_eq!(draft(&h), long);
    let top = cursor(&h);
    h.send(vec![down()]);
    assert_eq!(
        draft(&h),
        long,
        "a wrapped prompt is gone through line by line"
    );
    assert_ne!(cursor(&h), top);
    h.send(vec![up()]);
    h.send(vec![up()]);
    assert_eq!(draft(&h), "try again");
    h.send(vec![up()]);
    assert_eq!(draft(&h), "Alpha question 29", "a repeat is recalled once");

    h.send(vec![key(Named::Escape, Code::Escape, none)]);
    assert_eq!(draft(&h), "", "Escape empties the box");
    assert_eq!(h.app.views[ALPHA].recalled, 0);

    h.send(vec![up()]);
    h.send(vec![down()]);
    h.send(vec![down()]);
    assert_eq!(
        draft(&h),
        "",
        "past the newest prompt the box is empty again"
    );

    h.send(vec![up()]);
    h.send(vec![up()]);
    h.send(vec![up()]);
    h.send(vec![key(Named::Enter, Code::Enter, none)]);
    let view = &h.app.views[ALPHA];
    assert!(matches!(view.items.last(), Some(Entry::User(text)) if text == "try again"));
    assert_eq!(view.draft.text(), "");
    assert_eq!(view.recalled, 0);
}

#[test]
#[ignore = "writes images; run with --ignored"]
fn recalled_prompt_snapshot() {
    let mut app = two_tiles();
    app.layout_mut().tiles.truncate(1);
    let view = app.views.get_mut(ALPHA).expect("Alpha");
    view.draft = text_editor::Content::new();
    view.step(true);
    view.step(true);
    Harness::new(app).snapshot("recalled-prompt");
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
        agent: false,
        started: None,
        ended: None,
    }
}

/// Alpha after its turn ended, with a subagent still working in the background: its
/// first calls returned, its last runs.
fn with_subagent() -> ClaudoApp {
    let mut app = two_tiles();
    let view = app.views.get_mut(ALPHA).unwrap();
    let mut agent = bash("agent", "", "");
    agent.name = "Agent".to_string();
    agent.agent = true;
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
                at: None,
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
    let mut running = bash("t4", "rg -n \"fn main\" crates", "Find the entry points");
    let now = time::now_millis();
    (built.started, built.ended) = (Some(now - 75_000), Some(now - 12_400));
    (failed.started, failed.ended) = (Some(now - 12_000), Some(now - 11_580));
    server.started = Some(now - 300_000);
    running.started = Some(now - 3_000);
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
    let _ = app.update(Message::ToggleFinished(ALPHA.to_string()));
    assert!(app.views[ALPHA].finished_collapsed);

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
fn launching_claudo_again_opens_a_window_with_a_new_chat() {
    let mut app = two_tiles();
    let main = app.main_window();
    let _ = app.update(Message::Launched);
    assert_eq!(app.windows.len(), 2);
    let (w, _) = *app
        .pending
        .values()
        .next()
        .expect("a chat is being created");
    assert_ne!(w, main, "for the new window");
    assert_eq!(app.launched, Some(w));
    let _ = app.update(Message::WindowOpened(w));
    assert_eq!(app.launched, None, "brought forward once it exists");
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
    let flags = |beside| Flags {
        ui: ui.clone(),
        beside,
    };
    let (mut beside, _) = ClaudoApp::init(Core::default(), flags(true));
    beside.conversations = conversations().to_vec();
    beside.restore_tiles();
    assert_eq!(
        beside.windows.len(),
        1,
        "another build running keeps the saved windows"
    );

    let (mut app, _) = ClaudoApp::init(Core::default(), flags(false));
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

#[test]
fn alt_click_opens_a_conversation_in_a_new_window() {
    let mut app = two_tiles();
    let main = app.main_window();
    app.conversations.push(ConversationInfo {
        id: "c".to_string(),
        title: "Charlie".to_string(),
        cwd: "/tmp".to_string(),
        attached: false,
        archived: false,
        overrides: Overrides::default(),
        context: Default::default(),
    });
    let _ = app.update(Message::Modifiers(keyboard::Modifiers::ALT));
    let _ = app.update(Message::In(main, Box::new(Message::Open("c".to_string()))));
    assert_eq!(app.windows.len(), 2);
    assert!(app.tile_of("c").is_some_and(|(w, _)| w != main));
    assert!(app.picked.is_empty());
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
fn a_background_subagent_works_on_after_its_turn_as_one_panel_row() {
    let mut app = with_subagent();
    let view = &app.views[ALPHA];
    assert!(!view.streaming, "the turn that started it ended");
    let listed: Vec<&str> = view
        .commands()
        .map(|(_, call)| call.call_id.as_str())
        .collect();
    assert_eq!(
        listed,
        ["agent"],
        "one row in the Commands panel, not one per call"
    );
    assert_eq!(view.running_commands(), 1);
    assert_eq!(
        view.in_background().as_deref(),
        Some("Running 1 subagent in the background"),
        "the end of the conversation says what still works"
    );
    let running: Vec<&str> = view
        .made_by("agent")
        .filter(|(_, call)| view.running(call))
        .map(|(_, call)| call.call_id.as_str())
        .collect();
    assert_eq!(running, ["s2"], "its last command runs on");
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
        at: None,
    }]));
    assert_eq!(
        app.views[ALPHA].running_commands(),
        0,
        "nothing outlives its subagent"
    );
    assert_eq!(app.views[ALPHA].in_background(), None);
}

#[test]
fn the_end_of_a_conversation_counts_its_background_commands() {
    let app = with_commands();
    assert_eq!(
        app.views[ALPHA].in_background().as_deref(),
        Some("Running 1 command in the background")
    );
}

#[test]
fn the_usage_says_when_it_was_updated_and_refreshes_on_demand() {
    let mut app = two_tiles();
    assert!(app.quota_asked, "the first report is asked for at launch");
    let quota = claudo_protocol::Quota {
        five_hour: Some(claudo_protocol::QuotaWindow {
            used: 0.4,
            resets_at: usage::now() + 3600,
        }),
        seven_day: None,
    };
    let _ = app.update(Message::Events(vec![ServerEvent::Quota(quota)]));
    assert!(!app.quota_asked);
    assert!(app.quota_at.is_some());

    let _ = app.update(Message::RefreshUsage);
    assert!(app.quota_asked);
    let _ = app.update(Message::Events(vec![ServerEvent::QuotaUnavailable(
        "claude exited".to_string(),
    )]));
    assert!(!app.quota_asked, "a refresh that failed can be tried again");
    assert_eq!(app.quota_error.as_deref(), Some("claude exited"));
}

#[test]
fn a_message_typed_in_the_claude_app_shows_as_the_users_own() {
    let mut app = two_tiles();
    let _ = app.update(Message::Events(vec![ServerEvent::Typed {
        conversation_id: ALPHA.to_string(),
        text: "how is it going?".to_string(),
    }]));
    let view = &app.views[ALPHA];
    assert!(matches!(view.items.last(), Some(Entry::User(text)) if text == "how is it going?"));
    assert!(view.streaming, "its turn is running");
}

/// The links a press and release near the top left corner of `element` opens.
fn clicked(renderer: &mut cosmic::Renderer, element: Element<'_, Message>) -> Vec<String> {
    let mut ui = UserInterface::build(element, WINDOW, Cache::default(), renderer);
    let mut messages = Vec::new();
    let _ = ui.update(
        &[button(true), button(false)],
        mouse::Cursor::Available(Point::new(12.0, 9.0)),
        renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    messages
        .into_iter()
        .filter_map(|message| match message {
            Message::Link(url) => Some(url),
            _ => None,
        })
        .collect()
}

#[test]
fn addresses_and_markdown_links_open_when_clicked() {
    let mut renderer = Harness::new(two_tiles()).renderer;
    let conversation = ALPHA.to_string();
    let mut reply = |text: &str| {
        let reply = Reply::complete(text.to_string());
        let viewer = selectable::Viewer {
            conversation: &conversation,
            entry: 0,
            selection: None,
            next: std::cell::Cell::new(0),
        };
        let element = markdown::view_with(reply.markdown.items(), markdown_settings(14.0), &viewer);
        clicked(&mut renderer, element)
    };

    assert_eq!(
        reply("https://github.com/tracel-ai/burn/pull/6111, as asked."),
        ["https://github.com/tracel-ai/burn/pull/6111"]
    );
    assert_eq!(reply("[docs](https://docs.rs) here"), ["https://docs.rs"]);
    assert!(reply("Opened https://x.io/p").is_empty());

    let typed = Selectable::plain("https://x.io/typed", 14.0, &conversation, (0, 0), None);
    assert_eq!(clicked(&mut renderer, typed.into()), ["https://x.io/typed"]);
}

fn reviewed() -> ClaudoApp {
    let mut app = two_tiles();
    let finding = |file: &str, line: Option<&str>, summary: &str, scenario: Option<&str>| {
        claudo_protocol::Finding {
            file: file.to_string(),
            line: line.map(str::to_string),
            summary: summary.to_string(),
            failure_scenario: scenario.map(str::to_string),
        }
    };
    let review = claudo_protocol::Review {
        preface: "I reviewed PR #1848 at head `15db4cc99`. The most serious is the lost device."
            .to_string(),
        findings: vec![
            finding(
                "crates/cubecl-runtime/src/tune/tuner.rs",
                Some("480"),
                "When a device is lost, the tune is unmarked, so every later launch of that \
                 key runs the whole tune again.",
                Some(
                    "A GPU poisoned mid-forward: each key misses, re-registers and walks \
                     every candidate until `sync` reports the poison.",
                ),
            ),
            finding(
                "crates/cubecl-runtime/src/tune/local.rs",
                Some("55"),
                "`get_or_open` returns `by_id[id]` whatever `operations` is.",
                Some("Two candidate lists on one part never share a tuner."),
            ),
            finding("README.md", None, "A stale line.", None),
        ],
        afterword: String::new(),
    };
    let view = app.views.get_mut(ALPHA).expect("a view");
    view.items
        .push(Entry::User("/code-review xhigh".to_string()));
    view.items
        .push(Entry::Review(Box::new(review::Shown::new(review))));
    app
}

#[test]
fn a_review_lists_its_findings_and_unfolds_their_scenarios() {
    let mut h = Harness::new(reviewed());
    let idx = h.app.views[ALPHA].items.len() - 1;
    let whole = Message::SelectRange(
        ALPHA.to_string(),
        Mark {
            block: (idx, 0),
            offset: 0,
        },
        Mark {
            block: (idx + 1, 0),
            offset: 0,
        },
    );
    let _ = h.app.update(whole.clone());
    let folded = h.selected();
    assert!(
        folded.starts_with("I reviewed PR #1848 at head 15db4cc99.")
            && folded.contains(
                "\n\n1. crates/cubecl-runtime/src/tune/tuner.rs:480\n\nWhen a device is lost"
            )
            && folded.contains("\n\n3. README.md\n\nA stale line."),
        "{folded:?}"
    );
    assert!(!folded.contains("A GPU poisoned"), "{folded:?}");

    let _ = h
        .app
        .update(Message::ToggleFinding(ALPHA.to_string(), idx, None));
    let _ = h.app.update(whole);
    let unfolded = h.selected();
    assert!(
        unfolded.contains("whole tune again.\n\nA GPU poisoned mid-forward")
            && unfolded.contains("Two candidate lists"),
        "{unfolded:?}"
    );
}

#[test]
#[ignore = "writes images; run with --ignored"]
fn review_snapshot() {
    let mut app = reviewed();
    app.layout_mut().tiles.truncate(1);
    let mut h = Harness::new(app);
    h.snapshot("review");
    let idx = h.app.views[ALPHA].items.len() - 1;
    let _ = h
        .app
        .update(Message::ToggleFinding(ALPHA.to_string(), idx, Some(0)));
    h.snapshot("review-one-open");
}

#[test]
fn page_keys_and_home_and_end_move_the_focused_transcript() {
    use cosmic::iced::advanced::widget::operation::black_box;
    use keyboard::key::{Code, Named};
    let mut h = Harness::new(two_tiles());
    h.focus(composer_id(ALPHA));
    let none = keyboard::Modifiers::empty();
    let jumps = |h: &mut Harness, named, code, modifiers| -> Vec<scroll::Jump> {
        h.messages(vec![key(named, code, modifiers)])
            .into_iter()
            .filter_map(|message| match message {
                Message::In(_, message) => match *message {
                    Message::Jump(id, jump) if id == ALPHA => Some(jump),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    };
    assert_eq!(
        jumps(&mut h, Named::End, Code::End, none),
        [scroll::Jump::Bottom]
    );
    h.app.views.get_mut(ALPHA).unwrap().draft = text_editor::Content::with_text("typed");
    assert!(
        jumps(&mut h, Named::Home, Code::Home, none).is_empty(),
        "Home moves in the text there is"
    );
    let ctrl = keyboard::Modifiers::CTRL;
    assert_eq!(
        jumps(&mut h, Named::Home, Code::Home, ctrl),
        [scroll::Jump::Top]
    );
    assert_eq!(
        jumps(&mut h, Named::PageUp, Code::PageUp, none),
        [scroll::Jump::PageUp]
    );

    let jump = |h: &mut Harness, to| {
        let mut op = scroll::jump_in(ALPHA, to);
        h.frame(|ui, renderer| ui.operate(renderer, &mut black_box(&mut op)));
        h.offsets()[0]
    };
    let bottom = h.offsets()[0];
    let page = bottom - jump(&mut h, scroll::Jump::PageUp);
    assert!(
        page > 300.0 && page < 900.0,
        "most of a tile's height: {page}"
    );
    assert_eq!(jump(&mut h, scroll::Jump::Top), 0.0);
    assert!((jump(&mut h, scroll::Jump::PageDown) - page).abs() <= 1.0);
    assert_eq!(jump(&mut h, scroll::Jump::Bottom), bottom);
}

#[test]
fn a_middle_click_scrolls_toward_the_pointer_until_the_next_click() {
    use cosmic::iced::window::Event as Window;
    let start = std::time::Instant::now();
    let frame = |ms| {
        Event::Window(Window::RedrawRequested(
            start + std::time::Duration::from_millis(ms),
        ))
    };
    let middle = |pressed| {
        Event::Mouse(if pressed {
            mouse::Event::ButtonPressed(mouse::Button::Middle)
        } else {
            mouse::Event::ButtonReleased(mouse::Button::Middle)
        })
    };
    let mut h = Harness::new(two_tiles());
    h.move_to(560.0, 400.0);
    h.send(vec![wheel(30.0)]);
    let up = h.offsets()[0];

    h.send(vec![middle(true), middle(false)]);
    h.move_to(560.0, 600.0);
    h.send(vec![frame(0), frame(100), frame(200)]);
    let down = h.offsets()[0];
    assert!(
        down > up + 100.0,
        "toward the pointer, below: {up} -> {down}"
    );

    h.move_to(560.0, 300.0);
    h.send(vec![frame(300), frame(400)]);
    assert!(h.offsets()[0] < down, "and back up once it is above");

    h.send(vec![button(true), button(false)]);
    let stopped = h.offsets()[0];
    h.send(vec![frame(500), frame(600)]);
    assert_eq!(h.offsets()[0], stopped, "a click stops it");

    h.send(vec![middle(true)]);
    h.move_to(560.0, 700.0);
    h.send(vec![frame(700), frame(800), middle(false)]);
    let released = h.offsets()[0];
    assert!(released > stopped, "held and moved, it scrolls");
    h.send(vec![frame(900), frame(1000)]);
    assert_eq!(h.offsets()[0], released, "and letting go stops it");
}

#[test]
#[ignore = "writes images; run with --ignored"]
fn back_to_bottom_snapshot() {
    let mut h = Harness::new(two_tiles());
    h.move_to(560.0, 400.0);
    h.send(vec![wheel(30.0)]);
    h.move_to(549.0, 766.0);
    h.snapshot("back-to-bottom");
}

#[test]
#[ignore = "writes images; run with --ignored"]
fn autoscroll_snapshot() {
    let mut h = Harness::new(two_tiles());
    h.move_to(560.0, 400.0);
    h.send(vec![Event::Mouse(mouse::Event::ButtonPressed(
        mouse::Button::Middle,
    ))]);
    h.move_to(560.0, 520.0);
    h.snapshot("autoscroll");
}

#[test]
fn a_button_back_to_the_bottom_shows_once_scrolled_up() {
    use cosmic::iced::advanced::widget::operation::black_box;
    let button_at = (548.0, 774.0);
    let mut h = Harness::new(two_tiles());
    let bottom = h.offsets()[0];
    let jumps = |h: &mut Harness| -> Vec<scroll::Jump> {
        h.move_to(button_at.0, button_at.1);
        h.messages(vec![button(true), button(false)])
            .into_iter()
            .filter_map(|message| match message {
                Message::In(_, message) => match *message {
                    Message::Jump(id, jump) if id == ALPHA => Some(jump),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    };
    assert!(jumps(&mut h).is_empty(), "none at the bottom");

    h.move_to(560.0, 400.0);
    h.send(vec![wheel(1.0)]);
    assert_eq!(jumps(&mut h), [scroll::Jump::Bottom], "one notch up");

    let mut op = scroll::jump_in(ALPHA, scroll::Jump::Bottom);
    h.frame(|ui, renderer| ui.operate(renderer, &mut black_box(&mut op)));
    assert_eq!(h.offsets()[0], bottom);
    h.send(vec![]);
    assert!(jumps(&mut h).is_empty(), "gone once back down");
}

#[test]
fn a_turn_handed_over_from_the_replaced_app_shows_as_working() {
    let charlie = "ccccc333-0000-4000-8000-000000000000";
    let mut app = two_tiles();
    app.conversations.push(ConversationInfo {
        id: charlie.to_string(),
        title: "Charlie".to_string(),
        cwd: "/tmp".to_string(),
        attached: false,
        archived: false,
        overrides: Overrides::default(),
        context: Default::default(),
    });
    let working = |id: &str| {
        Message::Events(vec![ServerEvent::Working {
            conversation_id: id.to_string(),
        }])
    };
    let _ = app.update(working(charlie));
    let view = &app.views[charlie];
    assert!(
        view.streaming && view.loading,
        "no tile shows it, yet it works"
    );

    let _ = app.update(working(ALPHA));
    assert!(app.views[ALPHA].streaming);
    let _ = app.update(Message::Events(vec![ServerEvent::Completed {
        conversation_id: ALPHA.to_string(),
        is_error: false,
        context: Default::default(),
        time: None,
    }]));
    let view = &app.views[ALPHA];
    assert!(
        !view.streaming && !view.partial,
        "done, and its history reloaded"
    );
}

#[test]
#[ignore = "writes images; run with --ignored"]
fn restart_snapshot() {
    let mut app = two_tiles();
    app.update_ready = true;
    app.quota = Some(claudo_protocol::Quota {
        five_hour: Some(claudo_protocol::QuotaWindow {
            used: 0.42,
            resets_at: usage::now() + 7200,
        }),
        seven_day: None,
    });
    app.quota_asked = false;
    let mut h = Harness::new(app);
    h.snapshot("restart-to-update");
    h.app.layout_mut().sidebar_width = SIDEBAR_MIN;
    h.snapshot("restart-to-update-narrow");
}

#[test]
fn each_window_is_titled_apart_and_remembers_its_workspace() {
    let (mut app, main, second) = two_windows();
    let _ = app.update(Message::CheckUpdate);
    assert_eq!(app.titles[&main], "Alpha · claudo");
    assert_eq!(app.titles[&second], "Bravo · claudo");

    let spot = |workspace: &str| placement::Spot {
        output: "DP-1".to_string(),
        workspace: workspace.to_string(),
    };
    let _ = app.update(Message::Placed(HashMap::from([
        (app.titles[&main].clone(), spot("1")),
        (app.titles[&second].clone(), spot("3")),
    ])));
    assert_eq!(app.window(main).spot, Some(spot("1")));
    let saved = app.window(second).saved();
    assert_eq!(WindowLayout::restored(&saved).spot, Some(spot("3")));

    app.conversations[1].title = "Alpha".to_string();
    let _ = app.update(Message::CheckUpdate);
    assert_eq!(
        app.titles[&second], "Alpha · claudo (2)",
        "two windows never share a title"
    );
}

#[test]
fn a_message_waits_for_another_agent_and_what_follows_stays_queued() {
    let mut app = two_tiles();
    let send = |app: &mut ClaudoApp, text: &str| {
        app.views.get_mut(ALPHA).unwrap().draft = text_editor::Content::with_text(text);
        let _ = app.update(Message::Send(ALPHA.to_string()));
    };
    let event = |ev| Message::Events(vec![ev]);
    let alpha = || ALPHA.to_string();
    let waiting = |idle| {
        event(ServerEvent::Waiting {
            conversation_id: alpha(),
            pid: 655265,
            idle,
        })
    };
    send(&mut app, "run the tests");
    let _ = app.update(waiting(false));
    assert_eq!(
        app.views[ALPHA].waiting_for,
        Some(Holder {
            pid: 655265,
            idle: false
        })
    );
    send(&mut app, "and the benches");
    assert_eq!(app.views[ALPHA].queued.len(), 1);

    let mut h = Harness::new(app);
    let says = |h: &mut Harness, what: &str| shown_texts(h).iter().any(|text| text.contains(what));
    assert!(says(&mut h, "process 655265"), "says what it waits for");
    assert!(says(&mut h, "still working"));
    let _ = h.app.update(waiting(true));
    assert!(
        says(&mut h, "idle agent"),
        "says when stopping it loses nothing"
    );

    let stop = h.where_is("Stop it").expect("a button to stop it");
    h.move_to(stop.x, stop.y);
    let clicked = h.messages(vec![button(true), button(false)]);
    assert!(
        clicked.iter().any(|message| matches!(
            message,
            Message::In(_, inner) if matches!(&**inner, Message::StopStray(id) if id == ALPHA)
        )),
        "{clicked:?}"
    );

    let _ = h.app.update(event(ServerEvent::Delivered {
        conversation_id: alpha(),
    }));
    let view = &h.app.views[ALPHA];
    assert_eq!(view.waiting_for, None);
    assert_eq!(
        view.queued.len(),
        1,
        "the first message's delivery is its own, not the queued one's"
    );
    let _ = h.app.update(event(ServerEvent::Delivered {
        conversation_id: alpha(),
    }));
    assert!(h.app.views[ALPHA].queued.is_empty());
}

fn shown_texts(h: &mut Harness) -> Vec<String> {
    h.frame(|ui, renderer| {
        use cosmic::iced::advanced::widget::operation::{Outcome, black_box};
        let mut texts = ShownTexts::default();
        ui.operate(renderer, &mut black_box(&mut texts));
        match Operation::<Vec<String>>::finish(&texts) {
            Outcome::Some(texts) => texts,
            _ => Vec::new(),
        }
    })
}

impl Harness {
    /// Where the first text that is `text` shows in the window, by its center.
    fn where_is(&mut self, text: &str) -> Option<Point> {
        use cosmic::iced::advanced::widget::operation::{Outcome, black_box};
        struct Find<'a> {
            text: &'a str,
            translation: cosmic::iced::Vector,
            found: Option<Point>,
        }
        impl Operation<Option<Point>> for Find<'_> {
            fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation<Option<Point>>)) {
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
                if content_bounds.height > bounds.height {
                    self.translation = translation;
                }
            }
            fn text(
                &mut self,
                _id: Option<&widget::Id>,
                bounds: cosmic::iced::Rectangle,
                text: &str,
            ) {
                if text == self.text && self.found.is_none() {
                    self.found = Some(bounds.center() - self.translation);
                }
            }
            fn finish(&self) -> Outcome<Option<Point>> {
                Outcome::Some(self.found)
            }
        }
        self.frame(|ui, renderer| {
            let mut find = Find {
                text,
                translation: cosmic::iced::Vector::ZERO,
                found: None,
            };
            ui.operate(renderer, &mut black_box(&mut find));
            find.found
        })
    }
}

/// Every text the view shows, in order.
#[derive(Default)]
struct ShownTexts(Vec<String>);

impl Operation<Vec<String>> for ShownTexts {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation<Vec<String>>)) {
        operate(self);
    }

    fn text(&mut self, _id: Option<&widget::Id>, _bounds: cosmic::iced::Rectangle, text: &str) {
        self.0.push(text.to_string());
    }

    fn finish(&self) -> cosmic::iced::advanced::widget::operation::Outcome<Vec<String>> {
        cosmic::iced::advanced::widget::operation::Outcome::Some(self.0.clone())
    }
}

#[test]
fn messages_not_yet_taken_at_an_update_stay_queued_in_the_new_build() {
    let (charlie, delta) = (
        "ccccc333-0000-4000-8000-000000000000",
        "ddddd444-0000-4000-8000-000000000000",
    );
    let mut app = two_tiles();
    for (id, title) in [(charlie, "Charlie"), (delta, "Delta")] {
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
    let image = claudo_protocol::Attachment::new("pasted.png".to_string(), "image/png", &[1, 2, 3]);
    let _ = app.update(Message::Events(vec![
        ServerEvent::Working {
            conversation_id: charlie.to_string(),
        },
        ServerEvent::Queued {
            conversation_id: charlie.to_string(),
            text: "and the benches".to_string(),
            attachments: vec![image],
        },
        ServerEvent::History {
            conversation_id: charlie.to_string(),
            items: vec![Item::User("run the tests".to_string())],
        },
    ]));
    assert_eq!(
        app.views[charlie].queued.len(),
        1,
        "the history does not drop it"
    );
    let _ = app.update(Message::Events(vec![ServerEvent::Delivered {
        conversation_id: charlie.to_string(),
    }]));
    let view = &app.views[charlie];
    assert!(view.queued.is_empty());
    assert!(matches!(
        &view.items[..],
        [Entry::User(first), Entry::User(second), Entry::File(_)]
            if first == "run the tests" && second == "and the benches"
    ));

    let _ = app.update(Message::Events(vec![
        ServerEvent::Queued {
            conversation_id: delta.to_string(),
            text: "check the logs".to_string(),
            attachments: Vec::new(),
        },
        ServerEvent::Waiting {
            conversation_id: delta.to_string(),
            pid: 655265,
            idle: true,
        },
    ]));
    let view = &app.views[delta];
    assert!(
        view.loading && view.queued.len() == 1 && view.waiting_for.is_some(),
        "a message that waited for another agent still waits"
    );
}

#[test]
fn a_closed_sidebar_shows_an_update_is_ready_and_still_opens() {
    let mut app = two_tiles();
    app.layout_mut().sidebar_open = false;
    app.update_ready = true;
    let mut renderer = Harness::new(two_tiles()).renderer;
    let header =
        Element::from(widget::Row::with_children(app.header_start())).map(
            |message| match message {
                Message::In(_, inner) => *inner,
                other => other,
            },
        );
    let mut ui = UserInterface::build(header, WINDOW, Cache::default(), &mut renderer);
    let mut messages = Vec::new();
    let _ = ui.update(
        &[button(true), button(false)],
        mouse::Cursor::Available(Point::new(12.0, 12.0)),
        &mut renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    assert!(
        matches!(messages[..], [Message::ToggleSidebar]),
        "the dot leaves the toggle clickable: {messages:?}"
    );
}

#[test]
#[ignore = "writes images; run with --ignored"]
fn update_dot_snapshot() {
    let mut app = two_tiles();
    app.layout_mut().sidebar_open = false;
    app.update_ready = true;
    let mut renderer = Harness::new(two_tiles()).renderer;
    let header = widget::container(widget::Row::with_children(app.header_start()))
        .padding(8)
        .into();
    render_alone(&mut renderer, header, "update-dot");
}

#[test]
fn files_attached_to_an_unsent_message_are_saved_and_come_back() {
    let mut app = two_tiles();
    let saved = app.unsent_saved;
    let image = claudo_protocol::Attachment::new("pasted.png".to_string(), "image/png", &[1, 2, 3]);
    let _ = app.update(Message::Attach(ALPHA.to_string(), Ok(vec![image.clone()])));
    assert_ne!(app.unsent_saved, saved, "saved once attached");
    assert_eq!(app.unsent().0[ALPHA], [&image]);

    let (mut relaunched, _) = ClaudoApp::init(
        Core::default(),
        Flags {
            ui: UiState {
                unsent: HashMap::from([(ALPHA.to_string(), vec![image])]),
                ..UiState::default()
            },
            beside: false,
        },
    );
    assert_eq!(
        relaunched.revived(ALPHA).attachments.len(),
        1,
        "back with its conversation"
    );
}

#[test]
#[ignore = "writes images; run with --ignored"]
fn context_ring_snapshot() {
    let mut app = two_tiles();
    for (conversation, used, window) in [(0, 300_000, 1_000_000), (1, 180_000, 200_000)] {
        app.conversations[conversation].context = claudo_protocol::ContextUsage {
            used,
            window: Some(window),
        };
    }
    Harness::new(app).snapshot("context-ring");
}

#[test]
fn tab_takes_the_suggested_next_message_into_an_empty_box() {
    let suggested = |text: &str| {
        Message::Events(vec![ServerEvent::Suggested {
            conversation_id: ALPHA.to_string(),
            text: text.to_string(),
        }])
    };
    let mut app = two_tiles();
    app.views.get_mut(ALPHA).unwrap().streaming = true;
    let _ = app.update(suggested("too late"));
    assert_eq!(
        app.views[ALPHA].suggestion, None,
        "a turn already running makes it stale"
    );
    app.views.get_mut(ALPHA).unwrap().streaming = false;
    let _ = app.update(suggested("run the tests"));

    let mut h = Harness::new(app);
    h.focus(composer_id(ALPHA));
    use keyboard::key::{Code, Named};
    let tab = key(Named::Tab, Code::Tab, keyboard::Modifiers::empty());
    let taken = h.messages(vec![tab.clone()]);
    assert!(
        taken
            .iter()
            .any(|m| matches!(m, Message::In(_, inner) if matches!(&**inner, Message::TakeSuggestion(id) if id == ALPHA))),
        "{taken:?}"
    );
    for message in taken {
        let _ = h.app.update(message);
    }
    let view = &h.app.views[ALPHA];
    assert_eq!(view.draft.text().trim(), "run the tests");
    assert_eq!(view.suggestion, None);

    let _ = h.app.update(suggested("commit it"));
    h.app.views.get_mut(ALPHA).unwrap().draft = text_editor::Content::with_text("my own");
    let typed = h.messages(vec![tab]);
    assert!(
        !typed.iter().any(
            |m| matches!(m, Message::In(_, inner) if matches!(&**inner, Message::TakeSuggestion(_)))
        ),
        "only an empty box takes it"
    );
    let _ = h.app.update(Message::Send(ALPHA.to_string()));
    assert_eq!(h.app.views[ALPHA].suggestion, None, "sending drops it");
}

#[test]
#[ignore = "writes images; run with --ignored"]
fn suggestion_snapshot() {
    let mut app = two_tiles();
    app.views.get_mut(ALPHA).unwrap().suggestion = Some("run the tests".to_string());
    Harness::new(app).snapshot("suggestion");
}

#[test]
fn suggestions_turn_off_from_settings() {
    let mut app = two_tiles();
    assert!(app.settings.prompt_suggestions, "on unless turned off");
    app.layout_mut().page = Page::Settings;
    let mut h = Harness::new(app);
    assert!(
        shown_texts(&mut h)
            .iter()
            .any(|text| text == "Suggest what to send next"),
        "the settings page has the switch"
    );
    let _ = h.app.update(Message::SetPromptSuggestions(false));
    assert!(!h.app.settings.prompt_suggestions);
}

#[test]
#[ignore = "writes images; run with --ignored"]
fn settings_snapshot() {
    let mut app = two_tiles();
    app.layout_mut().page = Page::Settings;
    Harness::new(app).snapshot("settings");
}
