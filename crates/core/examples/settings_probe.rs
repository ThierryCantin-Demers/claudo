//! Smoke test: settings persist across backend instances (temp data dir).
//! Usage: cargo run -p claudo-core --example settings_probe

use claudo_core::backend::Backend;
use claudo_protocol::{Command, Effort, Profile, ServerEvent, Settings};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

fn main() {
    let tmp = std::env::temp_dir().join("claudo_settings_probe");
    let _ = std::fs::remove_dir_all(&tmp);
    std::env::set_var("XDG_DATA_HOME", &tmp);

    let (tx, mut rx) = unbounded_channel::<ServerEvent>();
    let backend = Backend::new(tx);
    backend.handle(Command::GetSettings);
    show(&mut rx, "initial (defaults)");

    backend.handle(Command::UpdateSettings {
        settings: Settings {
            model: "claude-opus-4-7".to_string(),
            effort: Effort::High,
            profile: Profile::FullAuto,
        },
    });
    show(&mut rx, "after update");

    // A fresh backend must load the saved settings from disk.
    let (tx2, mut rx2) = unbounded_channel::<ServerEvent>();
    let backend2 = Backend::new(tx2);
    backend2.handle(Command::GetSettings);
    show(&mut rx2, "reloaded (new backend)");

    let _ = std::fs::remove_dir_all(&tmp);
}

fn show(rx: &mut UnboundedReceiver<ServerEvent>, label: &str) {
    while let Ok(ev) = rx.try_recv() {
        if let ServerEvent::Settings(s) = ev {
            println!(
                "{label}: model={:?} effort={:?} profile={:?}",
                s.model, s.effort, s.profile
            );
        }
    }
}
