//! The dibs machines, each drawn in a tile of its own by dibstop's egui view, whose actions
//! the daemon runs.

use std::collections::HashMap;

use claudo_protocol::{Command, DibsMachine, DibsWatch};
use dibstop_view::view::{Machine, View};

use crate::meters::{self, History};

#[derive(Default)]
pub struct Machines {
    machines: Vec<Machine>,
    views: HashMap<String, View>,
    /// The machine whose view takes dibstop's keys: the one last clicked.
    active: Option<String>,
    /// The machine whose view asked for what the daemon answers next.
    asking: Option<String>,
    histories: HashMap<String, History>,
    /// How the daemon watches them, once it says.
    pub watch: Option<DibsWatch>,
}

impl Machines {
    pub fn heard(&mut self, machines: Vec<DibsMachine>) {
        for machine in &machines {
            if let Some(status) = &machine.status {
                self.histories
                    .entry(machine.name.clone())
                    .or_default()
                    .heard(status);
            }
        }
        self.machines = machines
            .into_iter()
            .map(|machine| Machine {
                name: machine.name,
                status: machine.status,
                trouble: machine.trouble,
            })
            .collect();
    }

    pub fn names(&self) -> Vec<String> {
        self.machines.iter().map(|machine| machine.name.clone()).collect()
    }

    pub fn answered(&mut self, title: String, body: String) {
        let asking = self.asking.as_ref();
        if let Some(view) = asking.and_then(|name| self.views.get_mut(name)) {
            view.answered(title, body);
        }
    }

    /// Draws the machine `name`, and returns the dibs call the person asked for.
    pub fn ui(&mut self, ui: &mut egui::Ui, name: &str) -> Option<Command> {
        let Some(machine) = self.machines.iter().find(|machine| machine.name == name) else {
            ui.weak("dibs no longer lists this machine.");
            return None;
        };
        let rect = ui.max_rect();
        let clicked = ui.input(|input| {
            input.pointer.primary_pressed()
                && input.pointer.interact_pos().is_some_and(|at| rect.contains(at))
        });
        if clicked || self.active.is_none() {
            self.active = Some(name.to_string());
        }
        // dibstop reads its keys from the whole page, so the other machines' views see none.
        let held = (self.active.as_deref() != Some(name))
            .then(|| ui.ctx().input_mut(|input| std::mem::take(&mut input.events)));
        if let Some(status) = &machine.status {
            let history = self.histories.entry(name.to_string()).or_default();
            meters::ui(ui, status, history);
        }
        let view = self.views.entry(name.to_string()).or_default();
        let action = view.ui(ui, std::slice::from_ref(machine));
        if let Some(events) = held {
            ui.ctx().input_mut(|input| input.events = events);
        }
        let action = action?;
        self.asking = Some(name.to_string());
        Some(Command::Dibs {
            machine: action.machine,
            call: action.call,
            label: action.label,
        })
    }
}
