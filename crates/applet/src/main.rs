//! A COSMIC panel applet showing the subscription's usage windows, which notifies once
//! when a window would run out before it resets. Opt-in behind the `cosmic` feature.
//!
//! It asks the CLI itself rather than a claudo backend, so it depends on `core`: it is a
//! panel item beside claudo, not one of its frontends, and never builds for the web.

mod warned;

#[cfg(not(feature = "cosmic"))]
fn main() {
    eprintln!("claudo-applet was built without the `cosmic` feature.");
    eprintln!("Build/run it with:  cargo run -p claudo-applet --features cosmic");
}

#[cfg(feature = "cosmic")]
mod panel;

#[cfg(feature = "cosmic")]
fn main() -> cosmic::iced::Result {
    cosmic::applet::run::<panel::Panel>(())
}
