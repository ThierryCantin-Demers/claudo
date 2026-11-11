//! The agents and the dibs machines side by side, as tiles the person drags into a split or
//! into tabs. The layout is kept with the page's storage.

use claudo_protocol::Command;
use egui_tiles::{
    Behavior, Linear, LinearDir, SimplificationOptions, TileId, Tiles, Tree, UiResponse,
};
use serde::{Deserialize, Serialize};

use crate::machines::Machines;
use crate::room::Room;

const KEY: &str = "desk";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Pane {
    Agents,
    Machines,
}

pub struct Desk {
    tree: Tree<Pane>,
}

impl Desk {
    pub fn new(storage: Option<&dyn eframe::Storage>) -> Self {
        let tree = storage
            .and_then(|storage| eframe::get_value::<Tree<Pane>>(storage, KEY))
            .filter(|tree| {
                [Pane::Agents, Pane::Machines]
                    .iter()
                    .all(|pane| tree.tiles.find_pane(pane).is_some())
            })
            .unwrap_or_else(laid_out);
        Self { tree }
    }

    pub fn save(&self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, KEY, &self.tree);
    }

    /// Draws both, and returns what the person asked of the agents and the machines.
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        room: &mut Room,
        machines: &mut Machines,
    ) -> Vec<Command> {
        let mut panes = Panes {
            room,
            machines,
            sent: Vec::new(),
        };
        self.tree.ui(&mut panes, ui);
        panes.sent.extend(panes.room.take_sent());
        panes.sent
    }
}

/// The agents on the left, wider, and the machines on the right.
fn laid_out() -> Tree<Pane> {
    let mut tiles = Tiles::default();
    let agents = tiles.insert_pane(Pane::Agents);
    let machines = tiles.insert_pane(Pane::Machines);
    let mut linear = Linear::new(LinearDir::Horizontal, vec![agents, machines]);
    linear.shares.set_share(agents, 3.0);
    linear.shares.set_share(machines, 2.0);
    let root = tiles.insert_container(linear);
    Tree::new("desk", root, tiles)
}

struct Panes<'a> {
    room: &'a mut Room,
    machines: &'a mut Machines,
    sent: Vec<Command>,
}

impl Behavior<Pane> for Panes<'_> {
    fn pane_ui(&mut self, ui: &mut egui::Ui, _tile: TileId, pane: &mut Pane) -> UiResponse {
        match pane {
            Pane::Agents => self.room.ui(ui),
            Pane::Machines => self.sent.extend(self.machines.ui(ui)),
        }
        UiResponse::None
    }

    fn tab_title_for_pane(&mut self, pane: &Pane) -> egui::WidgetText {
        match pane {
            Pane::Agents => "Agents".into(),
            Pane::Machines => "Machines".into(),
        }
    }

    /// Every pane keeps its tab, which is what it is dragged by.
    fn simplification_options(&self) -> SimplificationOptions {
        SimplificationOptions {
            all_panes_must_have_tabs: true,
            ..SimplificationOptions::default()
        }
    }
}
