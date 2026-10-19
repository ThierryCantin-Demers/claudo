//! One claudo per build. Launched while the same build runs, claudo asks it for a new window
//! and exits instead of reopening every saved window a second time. A different build, such
//! as one being developed, runs beside it in a window of its own.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use cosmic::iced::futures::channel::mpsc;
use cosmic::iced::futures::{Stream, StreamExt};

use super::Message;

#[derive(Debug, PartialEq, Eq)]
pub enum Launch {
    /// No claudo runs: this one reopens the windows as they were left.
    First,
    /// The claudo running is this build, and opens a window for this launch.
    Handed,
    /// Another build runs, and keeps the saved windows.
    Beside,
}

/// A running claudo that has not answered within this long counts as hung.
const PATIENCE: Duration = Duration::from_secs(3);

/// Later launches of this build, which the first one hears of before its windows exist.
static LAUNCHES: Mutex<Option<mpsc::UnboundedReceiver<()>>> = Mutex::new(None);

pub fn launch() -> Launch {
    let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) else {
        return Launch::First;
    };
    // Two launches at once, such as a double click, would both find no claudo running.
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("claudo.lock"));
    let _held = lock.as_ref().map(File::lock);
    let build = build();
    let (launch, listener) = launch_at(&dir.join("claudo.sock"), &build);
    if let Some(listener) = listener {
        *LAUNCHES.lock().unwrap() = Some(answer_launches(listener, build));
    }
    launch
}

/// Each later launch of this build, once the app runs.
pub fn launches() -> impl Stream<Item = Message> {
    match LAUNCHES.lock().unwrap().take() {
        Some(launches) => launches.map(|()| Message::Launched).left_stream(),
        None => cosmic::iced::futures::stream::empty().right_stream(),
    }
}

/// A new build is installed where this one was started from: a binary replaced while it
/// runs is read as deleted, and a file is there again.
pub fn replaced() -> bool {
    std::fs::read_link("/proc/self/exe").is_ok_and(|exe| {
        exe.to_str()
            .and_then(|exe| exe.strip_suffix(" (deleted)"))
            .is_some_and(|path| Path::new(path).is_file())
    })
}

/// The executable's path; once a rebuild replaces the file, a running claudo's ends in
/// ` (deleted)`, so the new one counts as another build.
fn build() -> String {
    std::env::current_exe()
        .map(|path| path.display().to_string())
        .unwrap_or_default()
}

fn launch_at(socket: &Path, build: &str) -> (Launch, Option<UnixListener>) {
    match UnixStream::connect(socket) {
        Ok(stream) => match ask(stream, build) {
            true => (Launch::Handed, None),
            false => (Launch::Beside, None),
        },
        Err(err)
            if matches!(
                err.kind(),
                ErrorKind::ConnectionRefused | ErrorKind::NotFound
            ) =>
        {
            // Refused means a claudo that crashed left its socket behind.
            let _ = std::fs::remove_file(socket);
            (Launch::First, UnixListener::bind(socket).ok())
        }
        Err(_) => (Launch::Beside, None),
    }
}

/// Whether the claudo listening on `stream` opens a window for this launch.
fn ask(stream: UnixStream, build: &str) -> bool {
    let _ = stream.set_read_timeout(Some(PATIENCE));
    if (&stream)
        .write_all(format!("{build}\n").as_bytes())
        .is_err()
    {
        return false;
    }
    let mut answer = String::new();
    BufReader::new(&stream).read_line(&mut answer).is_ok() && answer == "open\n"
}

fn answer_launches(listener: UnixListener, build: String) -> mpsc::UnboundedReceiver<()> {
    let (launched, launches) = mpsc::unbounded();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let _ = stream.set_read_timeout(Some(PATIENCE));
            let mut asked = String::new();
            if BufReader::new(&stream).read_line(&mut asked).is_err() {
                continue;
            }
            let same = asked.trim_end() == build;
            if same && launched.unbounded_send(()).is_err() {
                break;
            }
            let _ = (&stream).write_all(if same { b"open\n" } else { b"other\n" });
        }
    });
    launches
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_launch_goes_to_the_claudo_of_its_build() {
        let dir = std::env::temp_dir().join(format!("claudo-instance-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("claudo.sock");
        let _ = std::fs::remove_file(&socket);

        let (first, listener) = launch_at(&socket, "/bin/claudo");
        assert_eq!(first, Launch::First);
        let mut launches = answer_launches(listener.expect("a listener"), "/bin/claudo".into());

        assert_eq!(launch_at(&socket, "/bin/claudo").0, Launch::Handed);
        assert_eq!(
            launch_at(&socket, "/src/target/release/claudo").0,
            Launch::Beside
        );
        assert_eq!(launches.try_recv().ok(), Some(()));
        assert!(
            launches.try_recv().is_err(),
            "one window, for its own build"
        );

        drop(launches);
        let _ = std::fs::remove_file(&socket);
        let stale = UnixListener::bind(&socket).unwrap();
        drop(stale);
        assert_eq!(launch_at(&socket, "/bin/claudo").0, Launch::First);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
