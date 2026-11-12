//! The agents and each dibs machine, as tiles the person drags into a split or into tabs,
//! and hides or shows again from the Machines menu. The layout is kept with the page's
//! storage.

use claudo_protocol::{Command, DIBS_POLLS, DibsWatch};
use egui_tiles::{
    Behavior, Container, Linear, LinearDir, SimplificationOptions, Tabs, Tile, TileId, Tiles,
    Tree, UiResponse,
};
use serde::{Deserialize, Serialize};

use crate::machines::Machines;
use crate::room::Room;

const KEY: &str = "desk";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Pane {
    Agents,
    Machine(String),
}

pub struct Desk {
    tree: Tree<Pane>,
}

impl Desk {
    pub fn new(storage: Option<&dyn eframe::Storage>) -> Self {
        let tree = storage
            .and_then(|storage| eframe::get_value::<Tree<Pane>>(storage, KEY))
            .filter(|tree| tree.tiles.find_pane(&Pane::Agents).is_some())
            .unwrap_or_else(laid_out);
        Self { tree }
    }

    pub fn save(&self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, KEY, &self.tree);
    }

    /// Draws the tiles, and returns what the person asked of the agents and the machines.
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        room: &mut Room,
        machines: &mut Machines,
    ) -> Vec<Command> {
        let names = machines.names();
        self.follow(&names);
        let mut panes = Panes {
            room,
            machines,
            names,
            toggled: Vec::new(),
            sent: Vec::new(),
        };
        self.tree.ui(&mut panes, ui);
        for (name, shown) in std::mem::take(&mut panes.toggled) {
            if let Some(tile) = self.tree.tiles.find_pane(&Pane::Machine(name)) {
                show(&mut self.tree.tiles, tile, shown);
            }
        }
        panes.sent.extend(panes.room.take_sent());
        panes.sent
    }

    /// A tile for each machine dibs lists, and none for one it no longer does.
    fn follow(&mut self, names: &[String]) {
        if names.is_empty() {
            return;
        }
        let gone: Vec<TileId> = self
            .tree
            .tiles
            .iter()
            .filter_map(|(id, tile)| match tile {
                Tile::Pane(Pane::Machine(name)) if !names.contains(name) => Some(*id),
                _ => None,
            })
            .collect();
        for tile in gone {
            self.tree.remove_recursively(tile);
        }
        for name in names {
            if self.tree.tiles.find_pane(&Pane::Machine(name.clone())).is_none() {
                self.place(name.clone());
            }
        }
    }

    /// Beside the other machines, else in a column of its own right of the agents.
    fn place(&mut self, name: String) {
        let tiles = &mut self.tree.tiles;
        let beside = tiles.iter().find_map(|(id, tile)| match tile {
            Tile::Pane(Pane::Machine(_)) => Some(*id),
            _ => None,
        });
        let pane = tiles.insert_pane(Pane::Machine(name));
        // Each pane sits in tabs of its own, so its neighbours are its tabs' siblings.
        let holder = beside
            .and_then(|tile| tiles.parent_of(tile))
            .and_then(|tabs| tiles.parent_of(tabs));
        if let Some(Tile::Container(container)) = holder.and_then(|id| tiles.get_mut(id)) {
            container.add_child(pane);
            return;
        }
        let column = tiles.insert_vertical_tile(vec![pane]);
        // Two thirds of the agents' width.
        let share = 2.0 / 3.0;
        match self.tree.root.and_then(|root| tiles.get_mut(root)) {
            Some(Tile::Container(Container::Linear(linear)))
                if linear.dir == LinearDir::Horizontal =>
            {
                linear.add_child(column);
                linear.shares.set_share(column, share);
            }
            _ => {
                let children = self.tree.root.into_iter().chain([column]).collect();
                let mut linear = Linear::new(LinearDir::Horizontal, children);
                linear.shares.set_share(column, share);
                self.tree.root = Some(tiles.insert_container(linear));
            }
        }
    }
}

/// The agents alone, until the machines arrive.
fn laid_out() -> Tree<Pane> {
    let mut tiles = Tiles::default();
    let agents = tiles.insert_pane(Pane::Agents);
    let root = tiles.insert_container(Linear::new(LinearDir::Horizontal, vec![agents]));
    Tree::new("desk", root, tiles)
}

/// Hides or shows `tile`, and each container above it that holds nothing else shown, so a
/// hidden machine leaves no empty space behind.
fn show(tiles: &mut Tiles<Pane>, tile: TileId, shown: bool) {
    tiles.set_visible(tile, shown);
    let mut at = tile;
    while let Some(parent) = tiles.parent_of(at) {
        let any = match tiles.get(parent) {
            Some(Tile::Container(container)) => container
                .children()
                .any(|&child| tiles.is_visible(child)),
            _ => true,
        };
        tiles.set_visible(parent, any);
        at = parent;
    }
}

fn shown(tiles: &Tiles<Pane>, tile: TileId) -> bool {
    tiles.is_visible(tile)
        && std::iter::successors(tiles.parent_of(tile), |&at| tiles.parent_of(at))
            .all(|parent| tiles.is_visible(parent))
}

struct Panes<'a> {
    room: &'a mut Room,
    machines: &'a mut Machines,
    names: Vec<String>,
    /// Machines the menu hid or showed, applied once the tree is drawn.
    toggled: Vec<(String, bool)>,
    sent: Vec<Command>,
}

impl Panes<'_> {
    fn menu(&mut self, tiles: &Tiles<Pane>, ui: &mut egui::Ui) {
        let watch = self.machines.watch.clone();
        egui::Grid::new("machines-menu").show(ui, |ui| {
            ui.weak("");
            ui.weak("shown");
            ui.weak("metrics");
            ui.end_row();
            for name in &self.names {
                let Some(tile) = tiles.find_pane(&Pane::Machine(name.clone())) else {
                    continue;
                };
                ui.label(name);
                let mut on = shown(tiles, tile);
                if ui.checkbox(&mut on, "").changed() {
                    self.toggled.push((name.clone(), on));
                }
                match &watch {
                    Some(watch) => {
                        let mut read = !watch.no_metrics.contains(name);
                        if ui.checkbox(&mut read, "").changed() {
                            let mut next = watch.clone();
                            next.no_metrics.retain(|other| other != name);
                            if !read {
                                next.no_metrics.push(name.clone());
                            }
                            self.sent.push(Command::DibsWatch(next));
                        }
                    }
                    None => {
                        ui.label("");
                    }
                }
                ui.end_row();
            }
        });
        let Some(watch) = watch else {
            return;
        };
        ui.separator();
        ui.label("Look every");
        for seconds in DIBS_POLLS {
            let chosen = watch.poll == seconds;
            if ui.radio(chosen, format!("{seconds} s")).clicked() && !chosen {
                self.sent.push(Command::DibsWatch(DibsWatch {
                    poll: seconds,
                    ..watch.clone()
                }));
            }
        }
    }
}

impl Behavior<Pane> for Panes<'_> {
    fn pane_ui(&mut self, ui: &mut egui::Ui, _tile: TileId, pane: &mut Pane) -> UiResponse {
        match pane {
            Pane::Agents => self.room.ui(ui),
            Pane::Machine(name) => self.sent.extend(self.machines.ui(ui, name)),
        }
        UiResponse::None
    }

    fn tab_title_for_pane(&mut self, pane: &Pane) -> egui::WidgetText {
        match pane {
            Pane::Agents => "Agents".into(),
            Pane::Machine(name) => name.as_str().into(),
        }
    }

    /// The Machines menu, on the agents' tab bar, which is always there.
    fn top_bar_right_ui(
        &mut self,
        tiles: &Tiles<Pane>,
        ui: &mut egui::Ui,
        _tile_id: TileId,
        tabs: &Tabs,
        _scroll_offset: &mut f32,
    ) {
        let agents = tabs
            .children
            .iter()
            .any(|&child| matches!(tiles.get(child), Some(Tile::Pane(Pane::Agents))));
        if !agents || self.names.is_empty() {
            return;
        }
        ui.menu_button("Machines", |ui| self.menu(tiles, ui));
    }

    /// Every pane keeps its tab, which is what it is dragged by.
    fn simplification_options(&self) -> SimplificationOptions {
        SimplificationOptions {
            all_panes_must_have_tabs: true,
            ..SimplificationOptions::default()
        }
    }
}
