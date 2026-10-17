//! What the tasks need from outside the workspace, and how to get what is missing.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::WEB_OUT;

#[derive(Clone, Copy)]
pub enum Need {
    Claude,
    /// libcosmic's Wayland client finds libxkbcommon through pkg-config at build time.
    Xkbcommon,
    CargoHot,
    /// The linker cargo-hot links each patch with.
    Lld,
    WasmTarget,
    WasmBindgen,
    WebBundle,
}

/// What `setup` installs or reports.
pub const TOOLS: [Need; 6] = [
    Need::Claude,
    Need::Xkbcommon,
    Need::CargoHot,
    Need::Lld,
    Need::WasmTarget,
    Need::WasmBindgen,
];

enum Fix {
    /// Needs no root, so `setup` runs it.
    Run(Vec<String>),
    Packages(&'static [Package]),
    Manual(String),
}

/// A system package, by its name in each package manager.
struct Package {
    dnf: &'static str,
    apt: &'static str,
    pacman: &'static str,
}

const PKG_CONFIG: Package = Package {
    dnf: "pkgconf-pkg-config",
    apt: "pkg-config",
    pacman: "pkgconf",
};
const XKBCOMMON: Package = Package {
    dnf: "libxkbcommon-devel",
    apt: "libxkbcommon-dev",
    pacman: "libxkbcommon",
};
const LLD: Package = Package {
    dnf: "lld",
    apt: "lld",
    pacman: "lld",
};

#[derive(Clone, Copy)]
enum Manager {
    Dnf,
    Apt,
    Pacman,
}

impl Need {
    fn label(self) -> String {
        match self {
            Need::Claude => "the Claude Code CLI".to_string(),
            Need::Xkbcommon => "pkg-config and the xkbcommon headers".to_string(),
            Need::CargoHot => format!("cargo-hot {}", cargo_hot_version()),
            Need::Lld => "ld.lld".to_string(),
            Need::WasmTarget => "the wasm32-unknown-unknown target".to_string(),
            Need::WasmBindgen => format!("wasm-bindgen {}", locked_version("wasm-bindgen")),
            Need::WebBundle => format!("the web bundle in {WEB_OUT}"),
        }
    }

    fn met(self) -> bool {
        match self {
            Need::Claude => on_path(&claude_bin()),
            Need::Xkbcommon => {
                output(Command::new("pkg-config").args(["--exists", "xkbcommon"])).is_some()
            }
            Need::CargoHot => output(Command::new("cargo").args(["install", "--list"]))
                .is_some_and(|list| {
                    let installed = format!("cargo-hot v{}:", cargo_hot_version());
                    list.lines().any(|line| line == installed)
                }),
            Need::Lld => bundled_lld().is_some() || on_path("ld.lld"),
            Need::WasmTarget => rustc_print("sysroot").is_some_and(|sysroot| {
                Path::new(&sysroot)
                    .join("lib/rustlib/wasm32-unknown-unknown")
                    .is_dir()
            }),
            Need::WasmBindgen => {
                output(Command::new("wasm-bindgen").arg("--version")).is_some_and(|version| {
                    version == format!("wasm-bindgen {}", locked_version("wasm-bindgen"))
                })
            }
            Need::WebBundle => Path::new(WEB_OUT).join("index.html").is_file(),
        }
    }

    fn fix(self) -> Fix {
        let words =
            |command: &str| -> Vec<String> { command.split(' ').map(str::to_string).collect() };
        match self {
            Need::Claude => Fix::Manual(
                "install it from https://code.claude.com, then run claude auth login".to_string(),
            ),
            Need::Xkbcommon => Fix::Packages(&[PKG_CONFIG, XKBCOMMON]),
            Need::CargoHot => Fix::Run(words(&format!(
                "cargo install cargo-hot --version {} --locked",
                cargo_hot_version()
            ))),
            Need::Lld => Fix::Packages(&[LLD]),
            Need::WasmTarget => Fix::Run(words("rustup target add wasm32-unknown-unknown")),
            Need::WasmBindgen => Fix::Run(words(&format!(
                "cargo install wasm-bindgen-cli --version {} --locked",
                locked_version("wasm-bindgen")
            ))),
            Need::WebBundle => Fix::Manual("cargo xtask web".to_string()),
        }
    }
}

impl Manager {
    fn detect() -> Option<Self> {
        let release = fs::read_to_string("/etc/os-release").ok()?;
        release
            .lines()
            .filter_map(|line| line.strip_prefix("ID=").or(line.strip_prefix("ID_LIKE=")))
            .flat_map(|ids| ids.trim_matches('"').split_whitespace())
            .find_map(|id| match id {
                "fedora" | "rhel" | "centos" => Some(Manager::Dnf),
                "debian" | "ubuntu" => Some(Manager::Apt),
                "arch" => Some(Manager::Pacman),
                _ => None,
            })
    }

    fn install(self, packages: &[Package]) -> String {
        let (command, names): (_, Vec<_>) = match self {
            Manager::Dnf => ("sudo dnf install", packages.iter().map(|p| p.dnf).collect()),
            Manager::Apt => ("sudo apt install", packages.iter().map(|p| p.apt).collect()),
            Manager::Pacman => (
                "sudo pacman -S",
                packages.iter().map(|p| p.pacman).collect(),
            ),
        };
        format!("{command} {}", names.join(" "))
    }
}

/// Fails with every need that is not met and how to meet it.
pub fn require(needs: &[Need]) -> Result<(), String> {
    let missing: Vec<Need> = needs.iter().copied().filter(|need| !need.met()).collect();
    if missing.is_empty() {
        return Ok(());
    }
    let manager = Manager::detect();
    let mut runnable = false;
    let mut report = String::from("missing what this needs:");
    for need in missing {
        let how = match need.fix() {
            Fix::Run(command) => {
                runnable = true;
                command.join(" ")
            }
            Fix::Packages(packages) => match manager {
                Some(manager) => manager.install(packages),
                None => "install it with your package manager".to_string(),
            },
            Fix::Manual(how) => how,
        };
        report.push_str(&format!("\n  {}: {how}", need.label()));
    }
    if runnable {
        report.push_str("\n`cargo xtask setup` runs the commands that need no root.");
    }
    Err(report)
}

/// Runs every fix that needs no root, then reports what is still missing.
pub fn setup() -> Result<(), String> {
    for need in TOOLS.into_iter().filter(|need| !need.met()) {
        if let Fix::Run(command) = need.fix() {
            crate::status(Command::new(&command[0]).args(&command[1..]))?;
        }
    }
    require(&TOOLS)?;
    println!("everything claudo builds with is installed");
    Ok(())
}

/// The directory holding rustc's own `ld.lld`, if this toolchain ships one.
pub fn bundled_lld() -> Option<PathBuf> {
    let dir = Path::new(&rustc_print("sysroot")?)
        .join("lib/rustlib")
        .join(rustc_print("host-tuple")?)
        .join("bin/gcc-ld");
    dir.join("ld.lld").is_file().then_some(dir)
}

fn claude_bin() -> String {
    env::var("CLAUDO_CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string())
}

/// The cargo-hot release that speaks the protocol version the `hot` feature locks.
fn cargo_hot_version() -> String {
    locked_version("cargo-hot-protocol")
}

fn locked_version(package: &str) -> String {
    let lock = fs::read_to_string("Cargo.lock").expect("Cargo.lock is checked in");
    let name = format!("name = \"{package}\"");
    let mut lines = lock.lines();
    lines
        .find(|line| *line == name)
        .and_then(|_| lines.next()?.strip_prefix("version = \""))
        .and_then(|version| version.strip_suffix('"'))
        .unwrap_or_else(|| panic!("Cargo.lock has no {package}"))
        .to_string()
}

fn on_path(program: &str) -> bool {
    if program.contains('/') {
        return Path::new(program).is_file();
    }
    env::var_os("PATH")
        .is_some_and(|path| env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

fn rustc_print(what: &str) -> Option<String> {
    output(Command::new("rustc").args(["--print", what]))
}

/// Stdout of a command that ran and succeeded.
fn output(command: &mut Command) -> Option<String> {
    let out = command.stderr(Stdio::null()).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}
