//! Which workspace each of claudo's windows is on, so a restart puts one left apart from
//! the main window back there. A
//! Wayland app cannot place its own windows; COSMIC lets one read where every window is and
//! move it, through a connection of its own. Windows are known to it by title alone, so
//! each of claudo's has its own.

use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use cosmic::cctk::cosmic_protocols::toplevel_info::v1::client::{
    zcosmic_toplevel_handle_v1::{self, ZcosmicToplevelHandleV1},
    zcosmic_toplevel_info_v1::{self, ZcosmicToplevelInfoV1},
};
use cosmic::cctk::cosmic_protocols::toplevel_management::v1::client::zcosmic_toplevel_manager_v1::{
    self, ZcosmicToplevelManagerV1,
};
use cosmic::cctk::wayland_client::backend::ObjectId;
use cosmic::cctk::wayland_client::globals::{GlobalListContents, registry_queue_init};
use cosmic::cctk::wayland_client::protocol::{wl_output, wl_registry};
use cosmic::cctk::wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, event_created_child,
};
use cosmic::cctk::wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1::{self, ExtForeignToplevelHandleV1},
    ext_foreign_toplevel_list_v1::{self, ExtForeignToplevelListV1},
};
use cosmic::cctk::wayland_protocols::ext::workspace::v1::client::{
    ext_workspace_group_handle_v1::{self, ExtWorkspaceGroupHandleV1},
    ext_workspace_handle_v1::{self, ExtWorkspaceHandleV1},
    ext_workspace_manager_v1::{self, ExtWorkspaceManagerV1},
};
use cosmic::iced::futures::Stream;
use cosmic::iced::futures::channel::mpsc;
use serde::{Deserialize, Serialize};

use super::ClaudoApp;
use cosmic::Application;

/// Where a window is: the output's name and the workspace's name on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spot {
    pub output: String,
    pub workspace: String,
}

/// Where each of claudo's windows is, by title, each time one lands on another workspace,
/// from a thread that waits on the compositor in between.
pub fn watch() -> impl Stream<Item = HashMap<String, Spot>> {
    let (found, spots) = mpsc::unbounded();
    std::thread::spawn(move || {
        let Some((mut desktop, mut queue)) = Desktop::connect() else {
            return;
        };
        let mut last = HashMap::new();
        while queue.blocking_dispatch(&mut desktop).is_ok() {
            let now = desktop.claudo_spots();
            if now != last {
                if found.unbounded_send(now.clone()).is_err() {
                    return;
                }
                last = now;
            }
        }
    });
    spots
}

/// Moves each window titled as a key of `spots` to its spot, once it has shown with that
/// title, waiting for them at most `patience`.
pub fn restore(spots: HashMap<String, Spot>, patience: Duration) {
    let Some((mut desktop, mut queue)) = Desktop::connect() else {
        return;
    };
    let deadline = Instant::now() + patience;
    let _ = desktop.settle(&mut queue, deadline, |desktop| {
        spots.keys().all(|title| desktop.spot(title).is_some())
    });
    for (title, spot) in &spots {
        if desktop.spot(title).as_ref() != Some(spot) {
            desktop.move_to(title, spot);
        }
    }
    let _ = queue.roundtrip(&mut desktop);
}

#[derive(Default)]
struct Window {
    title: String,
    app_id: String,
    cosmic: Option<ZcosmicToplevelHandleV1>,
    workspaces: Vec<ObjectId>,
}

struct Workspace {
    handle: ExtWorkspaceHandleV1,
    name: String,
}

#[derive(Default)]
struct Group {
    outputs: Vec<ObjectId>,
    workspaces: Vec<ObjectId>,
}

struct Desktop {
    info: ZcosmicToplevelInfoV1,
    manager: ZcosmicToplevelManagerV1,
    windows: HashMap<ObjectId, Window>,
    workspaces: HashMap<ObjectId, Workspace>,
    groups: HashMap<ObjectId, Group>,
    outputs: HashMap<ObjectId, (wl_output::WlOutput, String)>,
}

impl Desktop {
    fn connect() -> Option<(Self, EventQueue<Self>)> {
        let conn = Connection::connect_to_env().ok()?;
        let (globals, queue) = registry_queue_init::<Self>(&conn).ok()?;
        let qh = queue.handle();
        let mut desktop = Self {
            info: globals.bind(&qh, 3..=3, ()).ok()?,
            manager: globals.bind(&qh, 4..=4, ()).ok()?,
            windows: HashMap::new(),
            workspaces: HashMap::new(),
            groups: HashMap::new(),
            outputs: HashMap::new(),
        };
        let _: ExtForeignToplevelListV1 = globals.bind(&qh, 1..=1, ()).ok()?;
        let _: ExtWorkspaceManagerV1 = globals.bind(&qh, 1..=1, ()).ok()?;
        for global in globals.contents().clone_list() {
            if global.interface == wl_output::WlOutput::interface().name && global.version >= 4 {
                let output: wl_output::WlOutput = globals.registry().bind(global.name, 4, &qh, ());
                desktop.outputs.insert(output.id(), (output, String::new()));
            }
        }
        Some((desktop, queue))
    }

    /// Handles events until `done` says enough is known, or `deadline` passes.
    fn settle(
        &mut self,
        queue: &mut EventQueue<Self>,
        deadline: Instant,
        done: impl Fn(&Self) -> bool,
    ) -> Option<()> {
        queue.roundtrip(self).ok()?;
        queue.roundtrip(self).ok()?;
        while !done(self) {
            let left = deadline.checked_duration_since(Instant::now())?;
            queue.flush().ok()?;
            let guard = queue.prepare_read()?;
            let mut poll = libc::pollfd {
                fd: guard.connection_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid `pollfd`, for the duration of the call.
            let ready = unsafe { libc::poll(&mut poll, 1, left.as_millis() as libc::c_int) };
            if ready > 0 {
                guard.read().ok()?;
            } else {
                drop(guard);
            }
            queue.dispatch_pending(self).ok()?;
        }
        Some(())
    }

    fn window(&self, title: &str) -> Option<&Window> {
        self.windows
            .values()
            .find(|window| window.app_id == ClaudoApp::APP_ID && window.title == title)
    }

    fn claudo_spots(&self) -> HashMap<String, Spot> {
        self.windows
            .values()
            .filter(|window| window.app_id == ClaudoApp::APP_ID && !window.title.is_empty())
            .filter_map(|window| Some((window.title.clone(), self.spot(&window.title)?)))
            .collect()
    }

    fn spot(&self, title: &str) -> Option<Spot> {
        let workspace = self.window(title)?.workspaces.first()?;
        let (_, group) = self
            .groups
            .iter()
            .find(|(_, group)| group.workspaces.contains(workspace))?;
        let (_, output) = self.outputs.get(group.outputs.first()?)?;
        Some(Spot {
            output: output.clone(),
            workspace: self.workspaces.get(workspace)?.name.clone(),
        })
    }

    fn move_to(&self, title: &str, spot: &Spot) {
        let Some(cosmic) = self.window(title).and_then(|window| window.cosmic.as_ref()) else {
            return;
        };
        let Some((output, _)) = self.outputs.values().find(|(_, name)| *name == spot.output) else {
            return;
        };
        let workspace = self.groups.values().find_map(|group| {
            group
                .outputs
                .contains(&output.id())
                .then(|| {
                    group
                        .workspaces
                        .iter()
                        .filter_map(|id| self.workspaces.get(id))
                        .find(|workspace| workspace.name == spot.workspace)
                })
                .flatten()
        });
        if let Some(workspace) = workspace {
            self.manager
                .move_to_ext_workspace(cosmic, &workspace.handle, output);
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Desktop {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_output::WlOutput, ()> for Desktop {
    fn event(
        desktop: &mut Self,
        output: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event
            && let Some((_, known)) = desktop.outputs.get_mut(&output.id())
        {
            *known = name;
        }
    }
}

impl Dispatch<ExtForeignToplevelListV1, ()> for Desktop {
    fn event(
        desktop: &mut Self,
        _: &ExtForeignToplevelListV1,
        event: ext_foreign_toplevel_list_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } = event {
            let cosmic = desktop
                .info
                .get_cosmic_toplevel(&toplevel, qh, toplevel.id());
            desktop.windows.insert(
                toplevel.id(),
                Window {
                    cosmic: Some(cosmic),
                    ..Window::default()
                },
            );
        }
    }

    event_created_child!(Desktop, ExtForeignToplevelListV1, [
        ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => (ExtForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ExtForeignToplevelHandleV1, ()> for Desktop {
    fn event(
        desktop: &mut Self,
        toplevel: &ExtForeignToplevelHandleV1,
        event: ext_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let id = toplevel.id();
        match event {
            ext_foreign_toplevel_handle_v1::Event::Closed => {
                desktop.windows.remove(&id);
            }
            ext_foreign_toplevel_handle_v1::Event::Title { title } => {
                desktop.windows.entry(id).or_default().title = title;
            }
            ext_foreign_toplevel_handle_v1::Event::AppId { app_id } => {
                desktop.windows.entry(id).or_default().app_id = app_id;
            }
            _ => {}
        }
    }
}

impl Dispatch<ZcosmicToplevelInfoV1, ()> for Desktop {
    fn event(
        _: &mut Self,
        _: &ZcosmicToplevelInfoV1,
        _: zcosmic_toplevel_info_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// Carries the id of the foreign toplevel it extends.
impl Dispatch<ZcosmicToplevelHandleV1, ObjectId> for Desktop {
    fn event(
        desktop: &mut Self,
        _: &ZcosmicToplevelHandleV1,
        event: zcosmic_toplevel_handle_v1::Event,
        toplevel: &ObjectId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(window) = desktop.windows.get_mut(toplevel) else {
            return;
        };
        match event {
            zcosmic_toplevel_handle_v1::Event::ExtWorkspaceEnter { workspace } => {
                window.workspaces.push(workspace.id());
            }
            zcosmic_toplevel_handle_v1::Event::ExtWorkspaceLeave { workspace } => {
                window.workspaces.retain(|id| *id != workspace.id());
            }
            _ => {}
        }
    }
}

impl Dispatch<ZcosmicToplevelManagerV1, ()> for Desktop {
    fn event(
        _: &mut Self,
        _: &ZcosmicToplevelManagerV1,
        _: zcosmic_toplevel_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtWorkspaceManagerV1, ()> for Desktop {
    fn event(
        desktop: &mut Self,
        _: &ExtWorkspaceManagerV1,
        event: ext_workspace_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_workspace_manager_v1::Event::WorkspaceGroup { workspace_group } => {
                desktop
                    .groups
                    .insert(workspace_group.id(), Group::default());
            }
            ext_workspace_manager_v1::Event::Workspace { workspace } => {
                desktop.workspaces.insert(
                    workspace.id(),
                    Workspace {
                        handle: workspace,
                        name: String::new(),
                    },
                );
            }
            _ => {}
        }
    }

    event_created_child!(Desktop, ExtWorkspaceManagerV1, [
        ext_workspace_manager_v1::EVT_WORKSPACE_GROUP_OPCODE => (ExtWorkspaceGroupHandleV1, ()),
        ext_workspace_manager_v1::EVT_WORKSPACE_OPCODE => (ExtWorkspaceHandleV1, ()),
    ]);
}

impl Dispatch<ExtWorkspaceGroupHandleV1, ()> for Desktop {
    fn event(
        desktop: &mut Self,
        group: &ExtWorkspaceGroupHandleV1,
        event: ext_workspace_group_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let group = desktop.groups.entry(group.id()).or_default();
        match event {
            ext_workspace_group_handle_v1::Event::OutputEnter { output } => {
                group.outputs.push(output.id());
            }
            ext_workspace_group_handle_v1::Event::WorkspaceEnter { workspace } => {
                group.workspaces.push(workspace.id());
            }
            ext_workspace_group_handle_v1::Event::WorkspaceLeave { workspace } => {
                group.workspaces.retain(|id| *id != workspace.id());
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtWorkspaceHandleV1, ()> for Desktop {
    fn event(
        desktop: &mut Self,
        workspace: &ExtWorkspaceHandleV1,
        event: ext_workspace_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_workspace_handle_v1::Event::Name { name } => {
                if let Some(known) = desktop.workspaces.get_mut(&workspace.id()) {
                    known.name = name;
                }
            }
            ext_workspace_handle_v1::Event::Removed => {
                desktop.workspaces.remove(&workspace.id());
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads, without moving anything, where the running desktop's claudo windows are.
    #[test]
    #[ignore = "reads the running desktop; run with --ignored --nocapture"]
    fn where_claudo_windows_are() {
        let Some((mut desktop, mut queue)) = Desktop::connect() else {
            panic!("a COSMIC session");
        };
        let _ = desktop.settle(&mut queue, Instant::now() + Duration::from_secs(1), |_| {
            false
        });
        for window in desktop.windows.values() {
            if window.app_id == ClaudoApp::APP_ID {
                println!("{:?} -> {:?}", window.title, desktop.spot(&window.title));
            }
        }
        assert!(!desktop.workspaces.is_empty(), "workspaces are listed");
    }

    #[test]
    #[ignore = "reads the running desktop; run with --ignored --nocapture"]
    fn the_watch_reports_where_claudo_windows_are() {
        use cosmic::iced::futures::StreamExt;
        let first = cosmic::iced::futures::executor::block_on(watch().next());
        println!("{first:#?}");
        assert!(first.is_some_and(|spots| !spots.is_empty()));
    }
}
