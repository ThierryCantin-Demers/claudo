//! The dibs machines, drawn by dibstop's own egui view, whose actions the daemon runs.

use claudo_protocol::{Command, DibsMachine};
use dibstop_view::view::{Machine, View};

#[derive(Default)]
pub struct Machines {
    view: View,
    machines: Vec<Machine>,
}

impl Machines {
    pub fn heard(&mut self, machines: Vec<DibsMachine>) {
        self.machines = machines
            .into_iter()
            .map(|machine| Machine {
                name: machine.name,
                status: machine.status,
                trouble: machine.trouble,
            })
            .collect();
    }

    pub fn answered(&mut self, title: String, body: String) {
        self.view.answered(title, body);
    }

    /// Draws the machines, and returns the dibs call the person asked for.
    pub fn ui(&mut self, ui: &mut egui::Ui) -> Option<Command> {
        if self.machines.is_empty() {
            ui.weak("No dibs machines yet: the daemon looks every few seconds, unless dibs is off here.");
            return None;
        }
        let action = self.view.ui(ui, &self.machines)?;
        Some(Command::Dibs {
            machine: action.machine,
            call: action.call,
            label: action.label,
        })
    }
}
