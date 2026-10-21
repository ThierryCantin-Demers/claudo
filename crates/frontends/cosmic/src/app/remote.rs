//! claudo showing the conversations of claudo on another computer, whose agents run there:
//! `claudo-cosmic --on <host>` reaches it over ssh. It keeps its own window layout and
//! instance, apart from a claudo running agents here.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use super::*;

static HOST: OnceLock<String> = OnceLock::new();

/// Whether this claudo's backend is served to other computers'.
static SERVE: AtomicBool = AtomicBool::new(true);

/// The computer whose claudo this one shows, as ssh names it; `None` when the agents run
/// here.
pub fn host() -> Option<&'static str> {
    HOST.get().map(String::as_str)
}

/// `host` as the app names it: a name mDNS gives ends in `.local`, which says nothing.
pub fn shown(host: &str) -> &str {
    host.strip_suffix(".local").unwrap_or(host)
}

pub fn set_host(host: String) {
    let _ = HOST.set(host);
}

pub fn set_serving(serve: bool) {
    SERVE.store(serve, Ordering::Relaxed);
}

pub(super) fn serving() -> bool {
    SERVE.load(Ordering::Relaxed)
}

/// `name`, made this claudo's own when it shows another computer's.
pub fn scoped(name: &str) -> String {
    match host() {
        Some(host) => format!("{name}@{host}"),
        None => name.to_string(),
    }
}

/// What a frontend asks once it reaches the backend; the transport asks for the status
/// itself when the backend is on another computer.
pub(super) fn hello(commands: &Commands) {
    for command in [
        Command::GetSettings,
        Command::GetCapabilities,
        Command::GetUsage,
        Command::ListConversations,
        Command::ListFolders,
    ] {
        commands.send(command);
    }
}

/// Starts this build again in place of the running one, with the same arguments. Nothing
/// runs here to hand over: the agents are on the other computer. Returns only if that
/// failed.
pub(super) fn relaunch() -> std::io::Error {
    use std::os::unix::process::CommandExt;
    let exe = match std::fs::read_link("/proc/self/exe") {
        Ok(exe) => exe,
        Err(error) => return error,
    };
    let exe = exe
        .to_str()
        .and_then(|path| path.strip_suffix(" (deleted)"))
        .map_or(exe.clone(), std::path::PathBuf::from);
    std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
        .exec()
}

impl ClaudoApp {
    /// The other computer was reached, or lost. Once reached again, everything shown is
    /// asked for again, and the backend says anew what its agents are doing.
    pub(super) fn connection(&mut self, connected: bool, detail: String) {
        if !connected {
            self.lost = Some(detail);
            return;
        }
        self.lost = None;
        if let Some(commands) = &self.commands {
            hello(commands);
        }
        self.on_screen = None;
        let open: Vec<ConversationId> = self.views.keys().cloned().collect();
        for id in open {
            if let Some(view) = self.views.get_mut(&id) {
                view.reconnected();
            }
            self.cmd(Command::OpenConversation { id });
        }
    }

    /// While the other computer cannot be reached, nothing can be sent to it.
    pub(super) fn reachable(&self) -> bool {
        self.lost.is_none()
    }

    /// Before the first answer: whom this claudo is reaching, and why it has not yet.
    pub(super) fn connecting(&self) -> String {
        match (self.remote, &self.lost) {
            (Some(host), Some(detail)) => {
                format!("Could not reach claudo on {host}: {detail}. Trying again…")
            }
            (Some(host), None) => format!("Connecting to claudo on {host}…"),
            (None, _) => "Connecting to backend…".to_string(),
        }
    }

    /// In every header bar of claudo showing another computer's, so it is never taken for
    /// the one running agents here; in the warning color while that computer is lost.
    pub(super) fn remote_chip(&self) -> Option<Element<'_, Message>> {
        let host = self.remote?;
        let lost = self.lost.is_some();
        let chip = widget::container(
            widget::Row::with_children(vec![
                widget::icon::from_name("computer-symbolic").size(14).into(),
                widget::text(format!("On {host}")).size(12.0).into(),
            ])
            .spacing(6)
            .align_y(Alignment::Center),
        )
        .padding([3, 10])
        .class(cosmic::theme::Container::custom(move |theme| {
            let cosmic = theme.cosmic();
            let color = Color::from(if lost {
                cosmic.warning_color()
            } else {
                cosmic.accent_color()
            });
            container::Style {
                text_color: Some(color),
                icon_color: Some(color),
                background: Some(color.scale_alpha(0.15).into()),
                border: Border {
                    color,
                    width: 1.0,
                    radius: cosmic.corner_radii.radius_xl.into(),
                },
                ..container::Style::default()
            }
        }));
        let tip = if lost {
            format!("{host} cannot be reached; claudo is trying again")
        } else {
            format!("The agents run on {host}: this window shows its claudo, over ssh")
        };
        Some(widget::tooltip(chip, widget::text(tip), widget::tooltip::Position::Bottom).into())
    }

    /// Over a window's content while the other computer cannot be reached.
    pub(super) fn lost_banner(&self) -> Option<Element<'_, Message>> {
        let detail = self.lost.as_ref()?;
        let host = self.remote.unwrap_or("the other computer");
        Some(
            widget::container(
                widget::text(format!(
                    "Lost claudo on {host}: {detail}. Trying again; messages wait in their box \
                     until it is back."
                ))
                .size(13.0 * self.zoom),
            )
            .padding([6, 12])
            .width(Length::Fill)
            .class(cosmic::theme::Container::custom(|theme| {
                let cosmic = theme.cosmic();
                container::Style {
                    background: Some(Color::from(cosmic.warning_color()).scale_alpha(0.2).into()),
                    ..container::Style::default()
                }
            }))
            .into(),
        )
    }
}

impl ConversationView {
    /// Forgets what it knew of its agent, which the backend says anew once reached.
    fn reconnected(&mut self) {
        self.streaming = false;
        self.partial = false;
        self.approvals.clear();
        self.queued.clear();
        self.undelivered = false;
        self.waiting_for = None;
        self.compacting = false;
        self.retrying = None;
        self.since = None;
        self.loading = true;
    }
}
