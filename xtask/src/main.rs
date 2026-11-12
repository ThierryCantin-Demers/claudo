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
const CODEX_APPLET_ID: &str = "com.claudo.CodexUsage";
const APPLET_BIN: &str = "claudo-applet";
const DAEMON_BIN: &str = "claudo-daemon";
const DAEMON_UNIT: &str = "claudo-daemon.service";
const DAEMON_PORT: u16 = 8787;
const TEAM_UNIT: &str = "claudo-team.service";
const TEAM_PORT: u16 = 8790;
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
    /// Install the panel applets showing the Claude and Codex plans' usage; add them to the
    /// panel from Settings, Desktop, Panel, Configure applets.
    InstallApplet,
    /// Install the daemon as a systemd user service, serving the control room and the web
    /// page at a link that survives restarts.
    InstallDaemon {
        /// Also show the agents of the daemon on this computer, as ssh names it, which
        /// needs `install-daemon` there too. Repeat for more.
        #[arg(long, value_name = "HOST")]
        with: Vec<String>,
        /// Leave dibs jobs to the daemon of another computer that shows this one's agents,
        /// since dibs says the same from both.
        #[arg(long)]
        no_dibs: bool,
    },
    /// Install the team page, `claudo-daemon --team`, as a systemd user service: the shared
    /// dibs machines and the benchmarks, for anyone on the network to look at.
    InstallTeam {
        /// Publish the page as `<NAME>.local` over mDNS while it runs, so its link follows
        /// whichever computer serves it. Never on two computers at once.
        #[arg(long)]
        name: Option<String>,
        /// The dibs machines to show, comma separated; every one without it.
        #[arg(long, value_name = "NAMES")]
        machines: Option<String>,
    },
    /// Remove what `install`, `install-applet`, `install-daemon` and `install-team` put in
    /// place.
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
    /// Run the egui app: a window of its own, or, with `--room`, the daemon's control room.
    Egui {
        /// Show the room of the daemon whose page is at this link, or, alone, of the daemon
        /// `install-daemon` set up on this computer.
        #[arg(long, value_name = "LINK", num_args = 0..=1)]
        room: Option<Option<String>>,
        /// Run under cargo-hot, patching the window live as its sources change.
        #[arg(long)]
        hot: bool,
    },
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
            Task::Egui { hot: true, .. } => &[Need::CargoHot, Need::Lld],
            Task::Egui { room: Some(_), .. } => &[],
            Task::Egui { .. } => &[Need::Claude],
            Task::Daemon => &[Need::WebBundle, Need::Claude],
            Task::Web => &[Need::WasmTarget, Need::WasmBindgen],
            Task::InstallDaemon { .. } => &[Need::WasmTarget, Need::WasmBindgen, Need::Claude],
            Task::InstallTeam { .. } => &[Need::WasmTarget, Need::WasmBindgen],
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
        Task::InstallDaemon { with, no_dibs } => install_daemon(&with, no_dibs),
        Task::InstallTeam { name, machines } => install_team(name.as_deref(), machines),
        Task::Uninstall => uninstall(),
        Task::Cosmic { hot: true, .. } => hot(&["-p", COSMIC_BIN, "--features", "hot"], None),
        Task::Cosmic { release, .. } => {
            let mut args = vec!["run", "-p", COSMIC_BIN, "--features", "cosmic"];
            if release {
                args.push("--release");
            }
            cargo(&args)
        }
        Task::Egui { room, hot: wanted } => {
            let room = match room {
                Some(Some(link)) => Some(link),
                Some(None) => Some(format!(
                    "http://127.0.0.1:{DAEMON_PORT}/?token={}",
                    daemon_token()?
                )),
                None => None,
            };
            match wanted {
                true => hot(&["-p", "claudo-egui", "--features", "hot"], room.as_deref()),
                false => {
                    let mut command = Command::new("cargo");
                    command.args(["run", "-p", "claudo-egui", "--release"]);
                    if let Some(room) = room {
                        command.env("CLAUDO_ROOM", room);
                    }
                    status(&mut command)
                }
            }
        }
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
    cargo_install("crates/frontends/cosmic", &["--features", "cosmic"])?;
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
    cargo_install("crates/applet", &["--features", "cosmic"])?;
    let bin = cargo_bin_dir()?.join(APPLET_BIN);
    println!("installed {}", bin.display());
    for (id, name, exec) in [
        (APPLET_ID, "Claude", bin.display().to_string()),
        (
            CODEX_APPLET_ID,
            "Codex",
            format!("{} --codex", bin.display()),
        ),
    ] {
        let desktop = applet_desktop_file(id)?;
        fs::create_dir_all(desktop.parent().unwrap())
            .map_err(|e| format!("cannot create {}: {e}", desktop.parent().unwrap().display()))?;
        // The panel lists an entry only with X-CosmicApplet, and NoDisplay keeps it out of the launcher.
        let entry = format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name={name} usage\n\
             Comment=The {name} plan's usage windows, and a warning when one runs out too fast\n\
             Exec={exec}\n\
             Icon=utilities-system-monitor-symbolic\n\
             Terminal=false\n\
             Categories=COSMIC;\n\
             NoDisplay=true\n\
             X-CosmicApplet=true\n"
        );
        fs::write(&desktop, entry)
            .map_err(|e| format!("cannot write {}: {e}", desktop.display()))?;
        println!("applet entry {}", desktop.display());
    }
    println!("add them to the panel from Settings, Desktop, Panel, Configure applets");
    Ok(())
}

fn install_daemon(others: &[String], no_dibs: bool) -> Result<(), String> {
    if let Some(host) = others
        .iter()
        .find(|host| host.is_empty() || host.contains([',', ' ', '/']))
    {
        return Err(format!("{host:?} is not a host ssh can reach"));
    }
    let (bin, web) = install_page()?;
    let token = daemon_token()?;
    let unit = daemon_unit()?;
    fs::create_dir_all(unit.parent().unwrap())
        .map_err(|e| format!("cannot create {}: {e}", unit.parent().unwrap().display()))?;
    // A user service starts with systemd's PATH, which lacks where `claude` usually is.
    let path = env::var("PATH").unwrap_or_default();
    let mut environment = vec![
        format!("PATH={path}"),
        format!("CLAUDO_PORT={DAEMON_PORT}"),
        format!("CLAUDO_STATIC_DIR={}", web.display()),
    ];
    if no_dibs {
        environment.push("CLAUDO_DIBS=off".to_string());
    }
    // Which benchmarks the page shows, as the installing shell says.
    for name in ["CLAUDO_BENCHMARKS", "CLAUDO_PARLEY_ROOM"] {
        if let Ok(value) = env::var(name) {
            environment.push(format!("{name}={value}"));
        }
    }
    if !others.is_empty() {
        environment.push(format!("CLAUDO_MACHINES={}", others.join(",")));
        // ssh from the service reaches the other computers with the keys of this session's agent.
        if let Ok(agent) = env::var("SSH_AUTH_SOCK") {
            environment.push(format!("SSH_AUTH_SOCK={agent}"));
        }
    }
    let environment: String = environment
        .iter()
        .map(|variable| format!("Environment={}\n", unit_word(variable)))
        .collect();
    let service = format!(
        "[Unit]\n\
         Description=claudo's control room and web page\n\
         \n\
         [Service]\n\
         ExecStart={bin}\n\
         {environment}\
         Restart=on-failure\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        bin = unit_word(&bin.display().to_string()),
    );
    fs::write(&unit, service).map_err(|e| format!("cannot write {}: {e}", unit.display()))?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", DAEMON_UNIT])?;
    systemctl(&["restart", DAEMON_UNIT])?;
    println!("installed {}", bin.display());
    println!("service {}", unit.display());
    println!("the control room: http://127.0.0.1:{DAEMON_PORT}/?token={token}");
    Ok(())
}

/// The web bundle where the daemon finds it, and the daemon in cargo's bin directory.
fn install_page() -> Result<(PathBuf, PathBuf), String> {
    web()?;
    let web = installed_web()?;
    if web.exists() {
        fs::remove_dir_all(&web).map_err(|e| format!("cannot replace {}: {e}", web.display()))?;
    }
    fs::create_dir_all(&web).map_err(|e| format!("cannot create {}: {e}", web.display()))?;
    for file in fs::read_dir(WEB_OUT).map_err(|e| format!("cannot read {WEB_OUT}: {e}"))? {
        let file = file.map_err(|e| format!("cannot read {WEB_OUT}: {e}"))?;
        fs::copy(file.path(), web.join(file.file_name()))
            .map_err(|e| format!("cannot install {}: {e}", file.path().display()))?;
    }
    cargo_install("crates/daemon", &[])?;
    Ok((cargo_bin_dir()?.join(DAEMON_BIN), web))
}

/// The team page as a user service, publishing `name` over mDNS when given.
fn install_team(name: Option<&str>, machines: Option<String>) -> Result<(), String> {
    let name = name.map(|name| match name.ends_with(".local") {
        true => name.to_string(),
        false => format!("{name}.local"),
    });
    if let Some(name) = &name
        && (name.contains(char::is_whitespace) || name.matches('.').count() != 1)
    {
        return Err(format!("{name:?} is not a name like team.local"));
    }
    let (bin, web) = install_page()?;
    let unit = config_home()?.join(format!("systemd/user/{TEAM_UNIT}"));
    fs::create_dir_all(unit.parent().unwrap())
        .map_err(|e| format!("cannot create {}: {e}", unit.parent().unwrap().display()))?;
    // dibs and parley are found on the installing shell's PATH, not systemd's.
    let mut environment = vec![
        format!("PATH={}", env::var("PATH").unwrap_or_default()),
        format!("CLAUDO_PORT={TEAM_PORT}"),
        format!("CLAUDO_STATIC_DIR={}", web.display()),
    ];
    let machines = machines.or_else(|| env::var("CLAUDO_TEAM_MACHINES").ok());
    if let Some(machines) = machines {
        environment.push(format!("CLAUDO_TEAM_MACHINES={machines}"));
    }
    for variable in ["CLAUDO_BENCHMARKS", "CLAUDO_PARLEY_ROOM", "SSH_AUTH_SOCK"] {
        if let Ok(value) = env::var(variable) {
            environment.push(format!("{variable}={value}"));
        }
    }
    if let Some(name) = &name {
        environment.push(format!("CLAUDO_TEAM_NAME={name}"));
    }
    let environment: String = environment
        .iter()
        .map(|variable| format!("Environment={}\n", unit_word(variable)))
        .collect();
    let service = format!(
        "[Unit]\n\
         Description=claudo's team page: the shared machines and the benchmarks\n\
         \n\
         [Service]\n\
         ExecStart={bin} --team\n\
         {environment}\
         Restart=on-failure\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        bin = unit_word(&bin.display().to_string()),
    );
    fs::write(&unit, service).map_err(|e| format!("cannot write {}: {e}", unit.display()))?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", TEAM_UNIT])?;
    systemctl(&["restart", TEAM_UNIT])?;
    println!("installed {}", bin.display());
    println!("service {}", unit.display());
    if env::var("CLAUDO_BENCHMARKS").is_err() || env::var("CLAUDO_PARLEY_ROOM").is_err() {
        println!("no benchmarks: set CLAUDO_BENCHMARKS and CLAUDO_PARLEY_ROOM and install again");
    }
    match &name {
        Some(name) => println!("the team page: http://{name}:{TEAM_PORT}/"),
        None => println!("the team page: port {TEAM_PORT} of this computer"),
    }
    println!(
        "other computers reach it once the firewall lets them: \
         sudo firewall-cmd --add-port={TEAM_PORT}/tcp --permanent; sudo firewall-cmd --reload"
    );
    println!("to keep it up while nobody is logged in: loginctl enable-linger");
    Ok(())
}

/// One word of a unit file, whatever spaces, quotes or `%` it holds.
fn unit_word(text: &str) -> String {
    let escaped = text
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    format!("\"{escaped}\"")
}

fn systemctl(args: &[&str]) -> Result<(), String> {
    status(Command::new("systemctl").arg("--user").args(args))
}

/// The daemon's token, kept where it reads it, made here when it has not started yet.
fn daemon_token() -> Result<String, String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let file = config_home()?.join("claudo/daemon-token");
    if let Ok(kept) = fs::read_to_string(&file)
        && !kept.trim().is_empty()
    {
        return Ok(kept.trim().to_string());
    }
    let token = fs::read_to_string("/proc/sys/kernel/random/uuid")
        .map_err(|e| format!("cannot make a token: {e}"))?
        .trim()
        .to_string();
    fs::create_dir_all(file.parent().unwrap())
        .map_err(|e| format!("cannot create {}: {e}", file.parent().unwrap().display()))?;
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&file)
        .and_then(|mut opened| opened.write_all(token.as_bytes()))
        .map_err(|e| format!("cannot write {}: {e}", file.display()))?;
    Ok(token)
}

fn uninstall() -> Result<(), String> {
    if cargo_bin_dir()?.join(COSMIC_BIN).exists() {
        cargo(&["uninstall", COSMIC_BIN])?;
    }
    if cargo_bin_dir()?.join(APPLET_BIN).exists() {
        cargo(&["uninstall", APPLET_BIN])?;
    }
    let unit = daemon_unit()?;
    if unit.exists() {
        systemctl(&["disable", "--now", DAEMON_UNIT])?;
    }
    let team = config_home()?.join(format!("systemd/user/{TEAM_UNIT}"));
    if team.exists() {
        systemctl(&["disable", "--now", TEAM_UNIT])?;
    }
    if cargo_bin_dir()?.join(DAEMON_BIN).exists() {
        cargo(&["uninstall", DAEMON_BIN])?;
    }
    let web = installed_web()?;
    if web.exists() {
        fs::remove_dir_all(&web).map_err(|e| format!("cannot remove {}: {e}", web.display()))?;
        println!("removed {}", web.display());
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
    for file in [
        desktop_file()?,
        icon_file()?,
        applet_desktop_file(APPLET_ID)?,
        applet_desktop_file(CODEX_APPLET_ID)?,
        unit,
        team,
    ]
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

/// `cargo hot` with `args`, the app given `room` as its `CLAUDO_ROOM`.
fn hot(args: &[&str], room: Option<&str>) -> Result<(), String> {
    let mut command = Command::new("cargo");
    command.arg("hot").args(args);
    if let Some(room) = room {
        command.env("CLAUDO_ROOM", room);
    }
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
        "--profile",
        "web",
    ])?;
    let target =
        env::var_os("CARGO_TARGET_DIR").map_or_else(|| PathBuf::from("target"), PathBuf::from);
    let status = Command::new("wasm-bindgen")
        .args(["--target", "web", "--no-typescript", "--out-dir", WEB_OUT])
        .args(["--remove-name-section", "--remove-producers-section"])
        .args(["--out-name", "claudo"])
        .arg(target.join("wasm32-unknown-unknown/web/claudo.wasm"))
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
    fs::copy(
        "assets/fonts/BIZUDPGothic-Regular.ttf",
        format!("{WEB_OUT}/BIZUDPGothic-Regular.ttf"),
    )
    .map_err(|e| format!("cannot copy the font: {e}"))?;
    compress_web();
    println!("web bundle in {WEB_OUT}; serve it with: cargo xtask daemon");
    Ok(())
}

/// Brotli and gzip copies of the bundle beside it, which the daemon serves to a browser that
/// takes them: the bundle is tens of megabytes, and a tunnel over Wi-Fi is slow. A compressor
/// that is missing only costs the copy, and an old copy is removed first so none is stale.
fn compress_web() {
    for file in ["claudo_bg.wasm", "claudo.js", "BIZUDPGothic-Regular.ttf"] {
        let path = format!("{WEB_OUT}/{file}");
        for (tool, args, out) in [
            ("brotli", &["-q", "9", "-k", "-f"][..], format!("{path}.br")),
            ("gzip", &["-9", "-k", "-f"][..], format!("{path}.gz")),
        ] {
            let _ = fs::remove_file(&out);
            let done = Command::new(tool).args(args).arg(&path).status();
            if !done.is_ok_and(|status| status.success()) {
                let _ = fs::remove_file(&out);
                eprintln!("{tool} did not compress {file}; it is served as it is");
            }
        }
    }
}

/// Plain `cargo` from PATH rather than `$CARGO`, so a wrapper there still sees the build.
fn cargo(args: &[&str]) -> Result<(), String> {
    status(Command::new("cargo").args(args))
}

fn cargo_install(path: &str, features: &[&str]) -> Result<(), String> {
    let mut command = Command::new("cargo");
    // `cargo install` warns of a toolchain it takes from the environment, which is
    // where the cargo running this task left its own.
    if let Some(toolchain) = env::var_os("RUSTUP_TOOLCHAIN") {
        command.arg(format!("+{}", toolchain.to_string_lossy()));
    }
    status(
        command
            .args(["install", "--path", path, "--locked"])
            .args(features),
    )
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

fn config_home() -> Result<PathBuf, String> {
    match env::var_os("XDG_CONFIG_HOME") {
        Some(dir) => Ok(PathBuf::from(dir)),
        None => Ok(home()?.join(".config")),
    }
}

/// Where the daemon finds the web bundle when not run from the repo.
fn installed_web() -> Result<PathBuf, String> {
    Ok(data_home()?.join("claudo/web"))
}

fn daemon_unit() -> Result<PathBuf, String> {
    Ok(config_home()?.join(format!("systemd/user/{DAEMON_UNIT}")))
}

fn desktop_file() -> Result<PathBuf, String> {
    Ok(data_home()?.join(format!("applications/{APP_ID}.desktop")))
}

fn applet_desktop_file(id: &str) -> Result<PathBuf, String> {
    Ok(data_home()?.join(format!("applications/{id}.desktop")))
}

fn remote_desktop_file(host: &str) -> Result<PathBuf, String> {
    Ok(data_home()?.join(format!("applications/{APP_ID}.on-{host}.desktop")))
}

fn icon_file() -> Result<PathBuf, String> {
    Ok(data_home()?.join(format!("icons/hicolor/scalable/apps/{APP_ID}.svg")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_unit_word_keeps_its_spaces_quotes_and_percent_signs() {
        assert_eq!(
            unit_word(r#"/opt/cargo installs/bin/say "100%""#),
            r#""/opt/cargo installs/bin/say \"100%%\"""#
        );
    }
}
