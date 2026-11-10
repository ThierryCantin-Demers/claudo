//! A COSMIC panel applet showing a Claude plan's usage windows, or with `--codex` a Codex
//! plan's, which notifies once when a window would run out before it resets. Opt-in
//! behind the `cosmic` feature.
//!
//! It asks the CLI itself rather than a claudo backend, so it depends on `core`: it is a
//! panel item beside claudo, not one of its frontends, and never builds for the web.

mod asks;
#[cfg_attr(not(feature = "cosmic"), allow(dead_code))]
mod pace;
#[cfg_attr(not(feature = "cosmic"), allow(dead_code))]
mod codex;
#[cfg_attr(not(feature = "cosmic"), allow(dead_code))]
mod usage;
mod warned;

#[cfg(not(feature = "cosmic"))]
fn main() {
    eprintln!("claudo-applet was built without the `cosmic` feature.");
    eprintln!("Build/run it with:  cargo run -p claudo-applet --features cosmic");
}

#[cfg(feature = "cosmic")]
mod panel;
#[cfg(feature = "cosmic")]
mod presence;

#[cfg(feature = "cosmic")]
fn main() -> cosmic::iced::Result {
    if std::env::args().skip(1).any(|arg| arg == "--codex") {
        cosmic::applet::run::<panel::Panel<usage::Codex>>(())
    } else {
        cosmic::applet::run::<panel::Panel<usage::Claude>>(())
    }
}
