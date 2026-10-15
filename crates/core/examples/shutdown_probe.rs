//! Manual probe for `Session::shutdown`: an idle process and one in the middle of a reply
//! have both exited by the time it returns, so deleting a conversation's files after it
//! is safe. Usage: cargo run -p claudo-core --example shutdown_probe

use std::path::Path;
use std::time::{Duration, Instant};

use claudo_core::engine::{EngineEvent, Session, SessionConfig};
use claudo_protocol::Profile;

fn main() {
    let cwd = std::env::temp_dir().join("claudo_shutdown_probe");
    std::fs::create_dir_all(&cwd).unwrap();
    let config = || SessionConfig {
        cwd: cwd.clone(),
        profile: Profile::Safe,
        model: Some("haiku".to_string()),
        effort: None,
    };
    let mut failed = false;
    let mut check = |ok: bool, what: &str| {
        println!("{} {what}", if ok { "PASS" } else { "FAIL" });
        failed |= !ok;
    };
    let alive = |pid: u32| Path::new(&format!("/proc/{pid}")).exists();

    let idle = Session::spawn(config(), None, "claudo shutdown probe", None).expect("spawn");
    let pid = idle.pid();
    let started = Instant::now();
    idle.shutdown(Duration::from_secs(10));
    check(!alive(pid), "an idle process has exited");
    println!("  took {:?}", started.elapsed());

    let mut busy = Session::spawn(config(), None, "claudo shutdown probe", None).expect("spawn");
    let pid = busy.pid();
    let turn = busy
        .send("Write a 500 word essay about rivers.")
        .expect("send");
    for event in &turn {
        if matches!(event, EngineEvent::Delta { .. }) {
            break;
        }
    }
    let started = Instant::now();
    busy.shutdown(Duration::from_secs(10));
    check(!alive(pid), "a process in the middle of a reply has exited");
    println!("  took {:?}", started.elapsed());

    std::process::exit(i32::from(failed));
}
