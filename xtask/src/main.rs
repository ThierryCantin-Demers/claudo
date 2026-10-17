//! The project's commands: `cargo xtask --help`.

mod need;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use clap::{Parser, Subcommand};
use need::Need;

const APP_ID: &str = "com.claudo.Cosmic";
const COSMIC_BIN: &str = "claudo-cosmic";
const WEB_OUT: &str = "crates/frontends/egui/dist";

#[derive(Parser)]
#[command(about = "claudo's build, run and install commands")]
struct Cli {
    #[command(subcommand)]
    task: Task,
}

#[derive(Subcommand)]
enum Task {
    /// Install the tools the other commands need, and say which system packages are missing.
    Setup,
    /// Install the COSMIC app into cargo's bin directory, with a launcher entry and icon.
    Install,
    /// Remove what `install` put in place.
    Uninstall,
    /// Run the COSMIC app.
    Cosmic {
        #[arg(long)]
        release: bool,
        /// Run under cargo-hot, patching the app live as its sources change. Needs the dev
        /// build: a release one keeps running the old view code the optimizer inlined.
        #[arg(long, conflicts_with = "release")]
        hot: bool,
    },
    /// Run the egui app.
    Egui,
    /// Serve the web bundle and the backend over a WebSocket.
    Daemon,
    /// Build the web bundle into crates/frontends/egui/dist.
    Web,
    /// Clippy on every crate, COSMIC included.
    Lint,
    /// Every crate's tests, COSMIC's headless UI tests included.
    Test {
        /// Also render the COSMIC snapshots into target/snapshots.
        #[arg(long)]
        snapshots: bool,
    },
    /// Run a probe against the real CLI; they cost tokens. Without a name, lists them.
    Probe { name: Option<String> },
}

fn main() -> ExitCode {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    if let Err(error) = env::set_current_dir(root) {
        eprintln!("xtask: cannot enter {}: {error}", root.display());
        return ExitCode::FAILURE;
    }
    match run(Cli::parse().task) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("xtask: {message}");
            ExitCode::FAILURE
        }
    }
}

impl Task {
    fn needs(&self) -> &'static [Need] {
        match self {
            Task::Install | Task::Cosmic { hot: false, .. } => &[Need::Xkbcommon, Need::Claude],
            Task::Cosmic { hot: true, .. } => {
                &[Need::Xkbcommon, Need::Claude, Need::CargoHot, Need::Lld]
            }
            Task::Egui => &[Need::Claude],
            Task::Daemon => &[Need::WebBundle, Need::Claude],
            Task::Web => &[Need::WasmTarget, Need::WasmBindgen],
            Task::Lint | Task::Test { .. } => &[Need::Xkbcommon],
            Task::Probe { name: Some(_) } => &[Need::Claude],
            Task::Setup | Task::Uninstall | Task::Probe { name: None } => &[],
        }
    }
}

fn run(task: Task) -> Result<(), String> {
    need::require(task.needs())?;
    match task {
        Task::Setup => need::setup(),
        Task::Install => install(),
        Task::Uninstall => uninstall(),
        Task::Cosmic { hot: true, .. } => hot(),
        Task::Cosmic { release, .. } => {
            let mut args = vec!["run", "-p", COSMIC_BIN, "--features", "cosmic"];
            if release {
                args.push("--release");
            }
            cargo(&args)
        }
        Task::Egui => cargo(&["run", "-p", "claudo-egui", "--release"]),
        Task::Daemon => cargo(&["run", "-p", "claudo-daemon"]),
        Task::Web => web(),
        Task::Lint => cargo(&[
            "clippy",
            "--workspace",
            "--all-targets",
            "--features",
            "claudo-cosmic/cosmic",
        ]),
        Task::Test { snapshots } => {
            cargo(&["test", "--workspace"])?;
            let mut args = vec!["test", "-p", COSMIC_BIN, "--features", "cosmic"];
            if snapshots {
                args.extend(["--", "--include-ignored"]);
            }
            cargo(&args)
        }
        Task::Probe { name } => {
            let mut args = vec!["run", "-p", "claudo-core", "--example"];
            args.extend(name.as_deref());
            let listed = cargo(&args);
            // Listing is cargo's error for a missing example name.
            if name.is_some() { listed } else { Ok(()) }
        }
    }
}

fn install() -> Result<(), String> {
    cargo(&[
        "install",
        "--path",
        "crates/frontends/cosmic",
        "--features",
        "cosmic",
        "--locked",
    ])?;
    let bin = cargo_bin_dir()?.join(COSMIC_BIN);
    let (desktop, icon) = (desktop_file()?, icon_file()?);
    for file in [&desktop, &icon] {
        fs::create_dir_all(file.parent().unwrap())
            .map_err(|e| format!("cannot create {}: {e}", file.parent().unwrap().display()))?;
    }
    fs::copy(format!("assets/icons/{APP_ID}.svg"), &icon)
        .map_err(|e| format!("cannot install the icon: {e}"))?;
    let entry = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Claudo\n\
         Comment=Claude Code agents side by side\n\
         Exec={}\n\
         Icon={APP_ID}\n\
         Terminal=false\n\
         Categories=Development;Utility;\n\
         StartupWMClass={APP_ID}\n",
        bin.display()
    );
    fs::write(&desktop, entry).map_err(|e| format!("cannot write {}: {e}", desktop.display()))?;
    println!("installed {}", bin.display());
    println!("launcher entry {}", desktop.display());
    Ok(())
}

fn uninstall() -> Result<(), String> {
    cargo(&["uninstall", COSMIC_BIN])?;
    for file in [desktop_file()?, icon_file()?] {
        match fs::remove_file(&file) {
            Ok(()) => println!("removed {}", file.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("cannot remove {}: {e}", file.display())),
        }
    }
    Ok(())
}

fn hot() -> Result<(), String> {
    let mut command = Command::new("cargo");
    command.args(["hot", "-p", COSMIC_BIN, "--features", "hot"]);
    // cargo-hot links patches with `cc -fuse-ld=lld` but drops the `-B` rustc passes to point
    // that at its own lld, so without this a system lld must be installed.
    if let Some(lld) = need::bundled_lld() {
        let path = env::var_os("PATH").unwrap_or_default();
        let path = env::join_paths([lld.clone()].into_iter().chain(env::split_paths(&path)))
            .map_err(|e| format!("cannot put {} on PATH: {e}", lld.display()))?;
        command.env("PATH", path);
    }
    status(&mut command)
}

fn web() -> Result<(), String> {
    cargo(&[
        "build",
        "-p",
        "claudo-egui",
        "--target",
        "wasm32-unknown-unknown",
        "--release",
    ])?;
    let status = Command::new("wasm-bindgen")
        .args(["--target", "web", "--no-typescript", "--out-dir", WEB_OUT])
        .args(["--out-name", "claudo"])
        .arg("target/wasm32-unknown-unknown/release/claudo.wasm")
        .status()
        .map_err(|e| format!("cannot run wasm-bindgen: {e}"))?;
    if !status.success() {
        return Err(format!("wasm-bindgen failed: {status}"));
    }
    fs::copy(
        "crates/frontends/egui/web/index.html",
        format!("{WEB_OUT}/index.html"),
    )
    .map_err(|e| format!("cannot copy index.html: {e}"))?;
    println!("web bundle in {WEB_OUT}; serve it with: cargo xtask daemon");
    Ok(())
}

/// Plain `cargo` from PATH rather than `$CARGO`, so a wrapper there still sees the build.
fn cargo(args: &[&str]) -> Result<(), String> {
    status(Command::new("cargo").args(args))
}

fn status(command: &mut Command) -> Result<(), String> {
    let line = [command.get_program()]
        .into_iter()
        .chain(command.get_args())
        .map(|word| word.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");
    let status = command
        .status()
        .map_err(|e| format!("cannot run {line}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{line} failed: {status}"))
    }
}

fn home() -> Result<PathBuf, String> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set".to_string())
}

/// Where `cargo install` puts binaries.
fn cargo_bin_dir() -> Result<PathBuf, String> {
    if let Some(root) = env::var_os("CARGO_INSTALL_ROOT") {
        return Ok(PathBuf::from(root).join("bin"));
    }
    match env::var_os("CARGO_HOME") {
        Some(cargo_home) => Ok(PathBuf::from(cargo_home).join("bin")),
        None => Ok(home()?.join(".cargo/bin")),
    }
}

fn data_home() -> Result<PathBuf, String> {
    match env::var_os("XDG_DATA_HOME") {
        Some(dir) => Ok(PathBuf::from(dir)),
        None => Ok(home()?.join(".local/share")),
    }
}

fn desktop_file() -> Result<PathBuf, String> {
    Ok(data_home()?.join(format!("applications/{APP_ID}.desktop")))
}

fn icon_file() -> Result<PathBuf, String> {
    Ok(data_home()?.join(format!("icons/hicolor/scalable/apps/{APP_ID}.svg")))
}
