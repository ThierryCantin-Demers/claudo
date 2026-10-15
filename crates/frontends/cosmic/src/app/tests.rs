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
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../target/snapshots");
        std::fs::create_dir_all(&dir).expect("a snapshot directory");
        let file = std::fs::File::create(dir.join(format!("{name}.png"))).expect("a snapshot file");
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), size.width, size.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .and_then(|mut writer| writer.write_image_data(&rgba))
            .expect("a written snapshot");
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
    app.layout.size = Some((WINDOW.width, WINDOW.height));
    for (id, title) in [(ALPHA, "Alpha"), (BRAVO, "Bravo")] {
        app.conversations.push(ConversationInfo {
            id: id.to_string(),
            title: title.to_string(),
            cwd: "/tmp".to_string(),
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
        app.layout.tiles.push(Tile::new(id.to_string()));
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
    assert!(matches!(h.app.drag, Some(Drag::Tile { at: Some(_), .. })));
    assert_eq!(
        h.offsets(),
        [bravo, alpha],
        "while dragged, each keeps its own"
    );

    h.send(vec![button(false)]);
    let _ = h.app.update(Message::DragEnd);
    h.send(vec![ctrl(false)]);
    assert_eq!(
        h.app.layout.tiles[1].conversation, ALPHA,
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
}
