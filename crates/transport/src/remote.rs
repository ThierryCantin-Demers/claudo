//! claudo on another computer. The app that runs the agents serves its backend on a socket
//! only its user can open; `--bridge` joins that socket to its own stdin and stdout; and
//! the other computer's claudo runs the bridge over ssh, which brings its own login and
//! encryption, so nothing new listens on the network.

use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{ChildStdin, Command as Process, ExitCode, Stdio};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use claudo_core::clients::ClientId;
use claudo_protocol::{Command, ServerEvent};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

use super::{Commands, Events, Input};

/// Runs the bridge on the other computer, wherever `cargo xtask install` put claudo; sh
/// reads it the same under any login shell.
const BRIDGE: &str = r#"sh -c 'exec "${CARGO_HOME:-$HOME/.cargo}/bin/claudo-cosmic" --bridge'"#;

/// Where this computer's claudo serves its backend.
pub fn socket() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("claudo-backend.sock")
}

/// Serves the backend to other computers' claudo, unless another claudo here already does.
pub(crate) fn serve(inputs: Sender<Input>) {
    let path = socket();
    if UnixStream::connect(&path).is_ok() {
        return;
    }
    let _ = std::fs::remove_file(&path);
    let Ok(listener) = UnixListener::bind(&path) else {
        return;
    };
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    std::thread::spawn(move || {
        for (id, stream) in (1..).zip(listener.incoming()) {
            if let Ok(stream) = stream {
                client(id, stream, inputs.clone());
            }
        }
    });
}

/// One JSON line per command in, one per event out.
fn client(id: ClientId, stream: UnixStream, inputs: Sender<Input>) {
    let Ok(writer) = stream.try_clone() else {
        return;
    };
    let (events, mut sent) = unbounded_channel();
    if inputs.send(Input::Connect(id, events)).is_err() {
        return;
    }
    std::thread::spawn(move || {
        let mut writer = BufWriter::new(writer);
        while let Some(event) = sent.blocking_recv() {
            let Ok(line) = serde_json::to_string(&event) else {
                continue;
            };
            if writeln!(writer, "{line}")
                .and_then(|()| writer.flush())
                .is_err()
            {
                break;
            }
        }
    });
    std::thread::spawn(move || {
        for line in BufReader::new(stream).lines() {
            let Ok(line) = line else {
                break;
            };
            if let Ok(command) = serde_json::from_str(&line) {
                let _ = inputs.send(Input::Command(id, command));
            }
        }
        let _ = inputs.send(Input::Disconnect(id));
    });
}

/// Joins stdin and stdout to the backend this computer's claudo serves. Says on stderr
/// when none does.
pub fn bridge() -> ExitCode {
    let Ok(stream) = UnixStream::connect(socket()) else {
        let host = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
        eprintln!("claudo is not running on {}", host.trim());
        return ExitCode::FAILURE;
    };
    let Ok(mut writer) = stream.try_clone() else {
        return ExitCode::FAILURE;
    };
    std::thread::spawn(move || {
        relay(&mut io::stdin().lock(), &mut writer);
        let _ = writer.shutdown(std::net::Shutdown::Write);
    });
    relay(&mut &stream, &mut io::stdout().lock());
    ExitCode::SUCCESS
}

/// Passes on whatever arrives as it does. Not `io::copy`: through the bridge, its kernel
/// copy now and then held a reply back for good.
fn relay(from: &mut impl Read, to: &mut impl Write) {
    let mut buffer = [0; 64 * 1024];
    loop {
        match from.read(&mut buffer) {
            Ok(0) => return,
            Ok(n) => {
                if to
                    .write_all(&buffer[..n])
                    .and_then(|()| to.flush())
                    .is_err()
                {
                    return;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

/// The backend of the claudo running on `host`, reached over ssh and reached again
/// whenever it is lost. Commands sent meanwhile are dropped: the frontend asks again for
/// what it shows once `Connection` says it is back.
pub fn connect_remote(host: String) -> (Commands, Events) {
    connect_with(move || {
        let mut ssh = Process::new("ssh");
        ssh.args([
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "ServerAliveInterval=10",
            "-o",
            "ServerAliveCountMax=3",
            &host,
            BRIDGE,
        ]);
        ssh
    })
}

/// As `connect_remote`, through whatever `bridge` starts.
pub fn connect_with(bridge: impl Fn() -> Process + Send + 'static) -> (Commands, Events) {
    let (commands, inputs) = channel();
    let (events, received) = unbounded_channel();
    let stdin: Link = Arc::default();
    let written = stdin.clone();
    std::thread::spawn(move || write(inputs, written));
    std::thread::spawn(move || {
        let mut wait = Duration::from_secs(1);
        loop {
            let started = Instant::now();
            let detail = run(bridge(), &stdin, &events);
            if events
                .send(ServerEvent::Connection {
                    connected: false,
                    detail,
                })
                .is_err()
            {
                return;
            }
            wait = if started.elapsed() > Duration::from_secs(60) {
                Duration::from_secs(1)
            } else {
                (wait * 2).min(Duration::from_secs(30))
            };
            std::thread::sleep(wait);
        }
    });
    (Commands(commands), Events(received))
}

type Link = Arc<Mutex<Option<ChildStdin>>>;

fn write(inputs: Receiver<Input>, link: Link) {
    for input in inputs {
        let Input::Command(_, command) = input else {
            continue;
        };
        let Ok(line) = serde_json::to_string(&command) else {
            continue;
        };
        let mut link = link.lock().unwrap();
        if let Some(stdin) = link.as_mut()
            && writeln!(stdin, "{line}")
                .and_then(|()| stdin.flush())
                .is_err()
        {
            *link = None;
        }
    }
}

/// Runs the bridge until it ends, passing its events on. Returns why it ended.
fn run(mut bridge: Process, link: &Link, events: &UnboundedSender<ServerEvent>) -> String {
    let child = bridge
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(error) => return format!("could not start ssh: {error}"),
    };
    let (Some(mut stdin), Some(stdout), Some(mut stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return "could not start ssh".to_string();
    };
    // Its first answer says the other end is there.
    let _ = writeln!(
        stdin,
        "{}",
        serde_json::to_string(&Command::GetStatus).unwrap_or_default()
    );
    let _ = stdin.flush();
    let mut stdin = Some(stdin);
    let said = std::thread::spawn(move || {
        let mut said = String::new();
        let _ = stderr.read_to_string(&mut said);
        said
    });
    let mut unreadable = false;
    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else {
            break;
        };
        if let Some(stdin) = stdin.take() {
            *link.lock().unwrap() = Some(stdin);
            let _ = events.send(ServerEvent::Connection {
                connected: true,
                detail: String::new(),
            });
        }
        match serde_json::from_str(&line) {
            Ok(event) => {
                if events.send(event).is_err() {
                    break;
                }
            }
            Err(_) if !unreadable => {
                unreadable = true;
                let _ = events.send(ServerEvent::Error {
                    conversation_id: None,
                    detail: "The other computer runs another build of claudo, and some of \
                             what it says cannot be read: install the same one on both."
                        .to_string(),
                });
            }
            Err(_) => {}
        }
    }
    *link.lock().unwrap() = None;
    let _ = child.kill();
    let status = child.wait();
    let said = said.join().unwrap_or_default();
    match said.lines().rev().find(|line| !line.trim().is_empty()) {
        Some(line) => line.trim().to_string(),
        None => match status {
            Ok(status) if !status.success() => format!("ssh ended: {status}"),
            _ => "the connection closed".to_string(),
        },
    }
}
