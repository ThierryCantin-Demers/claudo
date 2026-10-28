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
const APPLET_ID: &str = "com.claudo.Usage";
const APPLET_BIN: &str = "claudo-applet";
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
    Install {
        /// Also add a launcher for claudo showing the conversations of the claudo running on
        /// this computer, as ssh names it; their agents run there.
        #[arg(long, value_name = "HOST")]
        on: Option<String>,
    },
    /// Install the panel applet showing the subscription's usage; add it to the panel from
    /// Settings, Desktop, Panel, Configure applets.
    InstallApplet,
    /// Remove what `install` and `install-applet` put in place.
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
            Task::Install { on: None } | Task::InstallApplet | Task::Cosmic { hot: false, .. } => {
                &[Need::PkgConfig, Need::Xkbcommon, Need::Claude]
            }
            Task::Install { on: Some(_) } => &[Need::PkgConfig, Need::Xkbcommon],
            Task::Cosmic { hot: true, .. } => &[
                Need::PkgConfig,
                Need::Xkbcommon,
                Need::Claude,
                Need::CargoHot,
                Need::Lld,
            ],
            Task::Egui => &[Need::Claude],
            Task::Daemon => &[Need::WebBundle, Need::Claude],
            Task::Web => &[Need::WasmTarget, Need::WasmBindgen],
            Task::Lint | Task::Test { .. } => &[Need::PkgConfig, Need::Xkbcommon],
            Task::Probe { name: Some(_) } => &[Need::Claude],
            Task::Setup | Task::Uninstall | Task::Probe { name: None } => &[],
        }
    }
}

fn run(task: Task) -> Result<(), String> {
    need::require(task.needs())?;
    match task {
        Task::Setup => need::setup(),
        Task::Install { on } => install(on.as_deref()),
        Task::InstallApplet => install_applet(),
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
            let mut args = vec!["run", "--example"];
            args.extend(name.as_deref());
            let listed = cargo(&args);
            // Listing is cargo's error for a missing example name.
            if name.is_some() { listed } else { Ok(()) }
        }
    }
}

fn install(on: Option<&str>) -> Result<(), String> {
    if let Some(host) = on
        && (host.is_empty() || host.chars().any(|c| c.is_whitespace() || c == '/'))
    {
        return Err(format!("{host:?} is not a host ssh can reach"));
    }
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
    let entry = |name: &str, comment: &str, exec: &str| {
        format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name={name}\n\
             Comment={comment}\n\
             Exec={exec}\n\
             Icon={APP_ID}\n\
             Terminal=false\n\
             Categories=Development;Utility;\n\
             StartupWMClass={APP_ID}\n"
        )
    };
    let exec = bin.display().to_string();
    let write = |file: &Path, entry: String| {
        fs::write(file, entry).map_err(|e| format!("cannot write {}: {e}", file.display()))
    };
    write(
        &desktop,
        entry("Claudo", "Claude Code agents side by side", &exec),
    )?;
    println!("installed {}", bin.display());
    println!("launcher entry {}", desktop.display());
    if let Some(host) = on {
        let remote = remote_desktop_file(host)?;
        write(
            &remote,
            entry(
                &format!("Claudo on {host}"),
                &format!("The Claude Code agents claudo runs on {host}"),
                &format!("{exec} --on {host}"),
            ),
        )?;
        println!("launcher entry {}", remote.display());
    }
    Ok(())
}

fn install_applet() -> Result<(), String> {
    cargo(&[
        "install",
        "--path",
        "crates/applet",
        "--features",
        "cosmic",
        "--locked",
    ])?;
    let bin = cargo_bin_dir()?.join(APPLET_BIN);
    let desktop = applet_desktop_file()?;
    fs::create_dir_all(desktop.parent().unwrap())
        .map_err(|e| format!("cannot create {}: {e}", desktop.parent().unwrap().display()))?;
    // The panel lists an entry only with X-CosmicApplet, and NoDisplay keeps it out of the launcher.
    let entry = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Claude usage\n\
         Comment=The Claude subscription's session and week, and a warning when either runs out too fast\n\
         Exec={}\n\
         Icon=utilities-system-monitor-symbolic\n\
         Terminal=false\n\
         Categories=COSMIC;\n\
         NoDisplay=true\n\
         X-CosmicApplet=true\n",
        bin.display()
    );
    fs::write(&desktop, entry).map_err(|e| format!("cannot write {}: {e}", desktop.display()))?;
    println!("installed {}", bin.display());
    println!("applet entry {}", desktop.display());
    println!("add it to the panel from Settings, Desktop, Panel, Configure applets");
    Ok(())
}

fn uninstall() -> Result<(), String> {
    cargo(&["uninstall", COSMIC_BIN])?;
    if cargo_bin_dir()?.join(APPLET_BIN).exists() {
        cargo(&["uninstall", APPLET_BIN])?;
    }
    let remote: Vec<PathBuf> = fs::read_dir(desktop_file()?.parent().unwrap())
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(&format!("{APP_ID}.on-")))
        })
        .collect();
    for file in [desktop_file()?, icon_file()?, applet_desktop_file()?]
        .into_iter()
        .chain(remote)
    {
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

fn applet_desktop_file() -> Result<PathBuf, String> {
    Ok(data_home()?.join(format!("applications/{APPLET_ID}.desktop")))
}

fn remote_desktop_file(host: &str) -> Result<PathBuf, String> {
    Ok(data_home()?.join(format!("applications/{APP_ID}.on-{host}.desktop")))
}

fn icon_file() -> Result<PathBuf, String> {
    Ok(data_home()?.join(format!("icons/hicolor/scalable/apps/{APP_ID}.svg")))
}
