//! The agents and the dibs machines side by side, as tiles the person drags into a split or
//! into tabs, each zoomed on its own. The layout and the zooms are kept with the page's
//! storage.

use claudo_protocol::Command;
use egui::emath::TSTransform;
use egui_tiles::{
    Behavior, Linear, LinearDir, SimplificationOptions, Tabs, Tile, TileId, Tiles, Tree, UiResponse,
};
use serde::{Deserialize, Serialize};

use crate::benchmarks::Benchmarks;
use crate::machines::Machines;
use crate::room::Room;

const KEY: &str = "desk";
const ZOOM_KEY: &str = "desk-zoom";

/// Tenths of the page's own size, so steps add up exactly.
const ZOOMS: std::ops::RangeInclusive<u8> = 5..=15;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Pane {
    Agents,
    Machines,
    Benchmarks,
}

pub struct Desk {
    tree: Tree<Pane>,
    zoom: Zoom,
}

/// Each pane's zoom, in tenths.
#[derive(Clone, Copy, Serialize, Deserialize)]
struct Zoom {
    agents: u8,
    machines: u8,
    #[serde(default = "whole")]
    benchmarks: u8,
}

fn whole() -> u8 {
    10
}

impl Default for Zoom {
    fn default() -> Self {
        Self {
            agents: 10,
            machines: 10,
            benchmarks: 10,
        }
    }
}

impl Zoom {
    fn of(&mut self, pane: Pane) -> &mut u8 {
        match pane {
            Pane::Agents => &mut self.agents,
            Pane::Machines => &mut self.machines,
            Pane::Benchmarks => &mut self.benchmarks,
        }
    }
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
            .map(with_benchmarks)
            .unwrap_or_else(laid_out);
        let zoom = storage
            .and_then(|storage| eframe::get_value(storage, ZOOM_KEY))
            .unwrap_or_default();
        Self { tree, zoom }
    }

    pub fn save(&self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, KEY, &self.tree);
        eframe::set_value(storage, ZOOM_KEY, &self.zoom);
    }

    /// Draws both, and returns what the person asked of the agents and the machines.
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        room: &mut Room,
        machines: &mut Machines,
        benchmarks: &mut Benchmarks,
    ) -> Vec<Command> {
        let mut panes = Panes {
            room,
            machines,
            benchmarks,
            zoom: &mut self.zoom,
            sent: Vec::new(),
        };
        self.tree.ui(&mut panes, ui);
        panes.sent.extend(panes.room.take_sent());
        panes.sent
    }
}

/// The agents on the left, wider, and the machines on the right, the benchmarks a tab
/// beside them.
fn laid_out() -> Tree<Pane> {
    let mut tiles = Tiles::default();
    let agents = tiles.insert_pane(Pane::Agents);
    let machines = tiles.insert_pane(Pane::Machines);
    let benchmarks = tiles.insert_pane(Pane::Benchmarks);
    let right = tiles.insert_tab_tile(vec![machines, benchmarks]);
    let mut linear = Linear::new(LinearDir::Horizontal, vec![agents, right]);
    linear.shares.set_share(agents, 3.0);
    linear.shares.set_share(right, 2.0);
    let root = tiles.insert_container(linear);
    Tree::new("desk", root, tiles)
}

/// A layout kept from before there were benchmarks, with them added beside its root.
fn with_benchmarks(mut tree: Tree<Pane>) -> Tree<Pane> {
    if tree.tiles.find_pane(&Pane::Benchmarks).is_some() {
        return tree;
    }
    let benchmarks = tree.tiles.insert_pane(Pane::Benchmarks);
    if let Some(Tile::Container(root)) = tree.root().and_then(|root| tree.tiles.get_mut(root)) {
        root.add_child(benchmarks);
    }
    tree
}

struct Panes<'a> {
    room: &'a mut Room,
    machines: &'a mut Machines,
    benchmarks: &'a mut Benchmarks,
    zoom: &'a mut Zoom,
    sent: Vec<Command>,
}

impl Panes<'_> {
    fn draw(&mut self, ui: &mut egui::Ui, pane: Pane) {
        match pane {
            Pane::Agents => self.room.ui(ui),
            Pane::Machines => self.sent.extend(self.machines.ui(ui)),
            Pane::Benchmarks => self.benchmarks.ui(ui),
        }
    }
}

/// Draws `pane` at `tenths` of its size, on a layer of its own that egui scales, pointer
/// included, so nothing drawn in it needs to know.
fn zoomed(ui: &mut egui::Ui, tenths: u8, pane: Pane, draw: impl FnOnce(&mut egui::Ui)) {
    let zoom = f32::from(tenths) / 10.0;
    let rect = ui.max_rect();
    let inner = egui::Rect::from_min_size(rect.min, rect.size() / zoom);
    let area = egui::Area::new(ui.id().with(("zoomed", pane)))
        .order(egui::Order::Middle)
        .fixed_pos(rect.min)
        .movable(false)
        .constrain(false);
    let layer = area.layer();
    ui.ctx().set_transform_layer(
        layer,
        TSTransform::new(rect.min.to_vec2() * (1.0 - zoom), zoom),
    );
    area.show(ui.ctx(), |ui| {
        ui.set_clip_rect(inner);
        ui.set_min_size(inner.size());
        ui.set_max_size(inner.size());
        draw(ui);
    });
    ui.allocate_rect(rect, egui::Sense::hover());
}

/// Smaller, the zoom as a percentage that resets it, and larger.
fn zoom_buttons(ui: &mut egui::Ui, tenths: &mut u8) {
    // Right to left: the last added is leftmost.
    if ui
        .add_enabled(*tenths < *ZOOMS.end(), egui::Button::new("+").small())
        .clicked()
    {
        *tenths += 1;
    }
    let reset = ui
        .add(
            egui::Button::new(format!("{}%", u32::from(*tenths) * 10))
                .small()
                .frame(false),
        )
        .on_hover_text("This pane's zoom; click for 100%");
    if reset.clicked() {
        *tenths = 10;
    }
    if ui
        .add_enabled(*tenths > *ZOOMS.start(), egui::Button::new("−").small())
        .clicked()
    {
        *tenths -= 1;
    }
}

impl Behavior<Pane> for Panes<'_> {
    fn pane_ui(&mut self, ui: &mut egui::Ui, _tile: TileId, pane: &mut Pane) -> UiResponse {
        let pane = *pane;
        match *self.zoom.of(pane) {
            10 => self.draw(ui, pane),
            tenths => zoomed(ui, tenths, pane, |ui| self.draw(ui, pane)),
        }
        UiResponse::None
    }

    fn tab_title_for_pane(&mut self, pane: &Pane) -> egui::WidgetText {
        match pane {
            Pane::Agents => "Agents".into(),
            Pane::Machines => "Machines".into(),
            Pane::Benchmarks => "Benchmarks".into(),
        }
    }

    /// Each pane's zoom, and how the machines are arranged and watched, on their tab bars.
    fn top_bar_right_ui(
        &mut self,
        tiles: &Tiles<Pane>,
        ui: &mut egui::Ui,
        _tile_id: TileId,
        tabs: &Tabs,
        _scroll_offset: &mut f32,
    ) {
        let Some(&Tile::Pane(pane)) = tabs.active.and_then(|active| tiles.get(active)) else {
            return;
        };
        if pane == Pane::Machines {
            self.sent.extend(self.machines.menu(ui));
        }
        zoom_buttons(ui, self.zoom.of(pane));
    }

    /// Every pane keeps its tab, which is what it is dragged by.
    fn simplification_options(&self) -> SimplificationOptions {
        SimplificationOptions {
            all_panes_must_have_tabs: true,
            ..SimplificationOptions::default()
        }
    }
}
