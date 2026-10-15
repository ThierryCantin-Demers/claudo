//! Smoke test: settings persist across backend instances (temp data dir), a model
//! version claudo once saved loads as its family's alias, and the CLI lists its models and
//! reports usage without a turn.
//! Usage: cargo run -p claudo-core --example settings_probe

use claudo_core::backend::Backend;
use claudo_protocol::{Command, Effort, Profile, ServerEvent, Settings};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

fn main() {
    let tmp = std::env::temp_dir().join("claudo_settings_probe");
    let _ = std::fs::remove_dir_all(&tmp);
    // SAFETY: no other thread exists yet.
    unsafe { std::env::set_var("XDG_DATA_HOME", &tmp) };

    let (tx, mut rx) = unbounded_channel::<ServerEvent>();
    let backend = Backend::new(tx);
    backend.handle(Command::GetSettings);
    show(&mut rx, "initial (defaults)");

    backend.handle(Command::UpdateSettings {
        settings: Settings {
            model: "claude-sonnet-5-5".to_string(),
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

    backend2.handle(Command::GetCapabilities);
    if let Some(ServerEvent::Models(models)) = rx2.blocking_recv() {
        for m in models {
            println!("model: {:?} runs {}", m.value, m.label);
        }
    }
    if let Some(ServerEvent::Commands(commands)) = rx2.blocking_recv() {
        let names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
        println!(
            "{} commands, compact among them: {}",
            names.len(),
            names.contains(&"compact")
        );
    }

    backend2.handle(Command::GetUsage);
    if let Some(ServerEvent::Quota(quota)) = rx2.blocking_recv() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        for (name, window) in [
            ("five hour", quota.five_hour),
            ("seven day", quota.seven_day),
        ] {
            match window {
                Some(w) => println!(
                    "{name}: {:.0}% used, resets in {} min",
                    w.used * 100.0,
                    w.resets_at.saturating_sub(now) / 60
                ),
                None => println!("{name}: not reported"),
            }
        }
    }

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
