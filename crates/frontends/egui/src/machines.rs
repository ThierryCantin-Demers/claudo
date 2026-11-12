//! The dibs machines, in a grid of equal cells the person arranges: each machine's state, what
//! its CPU, memory and cards are doing, and its jobs, with dibstop's actions and keys, which
//! the daemon runs.

use std::collections::HashMap;

use claudo_protocol::{Command, DIBS_POLL, DibsMachine, DibsWatch};
use dibstop_view::action::Action;
use dibstop_view::dibs_format::{Label, Mode, Span};
use dibstop_view::item::{Item, Progress as _};
use dibstop_view::text::{Tone, agent_hue, cores, plain, state_words};
use egui::{Color32, Key, Rect, RichText, Ui, vec2};
use serde::{Deserialize, Serialize};

use crate::meters::{self, History};

const KEY: &str = "machines";

/// The narrowest a cell gets when the columns fit the width, and the shortest, below which
/// the cells scroll instead.
const NARROWEST: f32 = 420.0;
const SHORTEST: f32 = 260.0;
const GAP: f32 = 8.0;

#[derive(Default)]
pub struct Machines {
    machines: Vec<DibsMachine>,
    arrangement: Arrangement,
    tiles: HashMap<String, Tile>,
    histories: HashMap<String, History>,
    /// The machine whose tile takes dibstop's keys: the one last clicked.
    active: Option<String>,
    /// The machine whose tile asked for what the daemon answers next.
    asking: Option<String>,
    /// How the daemon watches them, once it says.
    pub watch: Option<DibsWatch>,
    /// The poll the menu's slider is set to while it moves, before the daemon says it took it.
    poll: Option<u64>,
}

/// How the person arranged the machines, kept with the page's storage.
#[derive(Default, Serialize, Deserialize)]
struct Arrangement {
    /// The machines in the order shown; one dibs adds later goes last.
    order: Vec<String>,
    hidden: Vec<String>,
    /// None fits as many as the width takes.
    columns: Option<usize>,
}

/// What one machine's cell remembers between frames.
#[derive(Default)]
struct Tile {
    /// By pid, so the selection follows its job as the cards reorder.
    selected: Option<u32>,
    confirm: Option<(u32, Label)>,
    /// The title and body of what an action printed.
    shown: Option<(String, String)>,
    /// What the action the daemon runs is doing.
    busy: Option<String>,
}

impl Machines {
    pub fn new(storage: Option<&dyn eframe::Storage>) -> Self {
        let arrangement = storage
            .and_then(|storage| eframe::get_value(storage, KEY))
            .unwrap_or_default();
        Self {
            arrangement,
            ..Self::default()
        }
    }

    pub fn save(&self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, KEY, &self.arrangement);
    }

    pub fn heard(&mut self, machines: Vec<DibsMachine>) {
        for machine in &machines {
            if let Some(status) = &machine.status {
                self.histories
                    .entry(machine.name.clone())
                    .or_default()
                    .heard(status);
            }
        }
        self.machines = machines;
    }

    /// Every machine dibs lists, in the person's order.
    fn ordered(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .machines
            .iter()
            .map(|machine| machine.name.clone())
            .collect();
        let at = |name: &String| {
            let order = &self.arrangement.order;
            order.iter().position(|other| other == name).unwrap_or(order.len())
        };
        names.sort_by_key(at);
        names
    }

    /// The machines shown, in a grid of equal cells, and the dibs call the person asked for.
    pub fn ui(&mut self, ui: &mut Ui) -> Option<Command> {
        if self.machines.is_empty() {
            ui.weak("No dibs machines yet: the daemon looks every few seconds, unless dibs is off here.");
            return None;
        }
        let shown: Vec<String> = self
            .ordered()
            .into_iter()
            .filter(|name| !self.arrangement.hidden.contains(name))
            .collect();
        if shown.is_empty() {
            ui.weak("Every machine is hidden: show one from Arrange.");
            return None;
        }
        let width = ui.available_width();
        let fit = ((width + GAP) / (NARROWEST + GAP)).floor() as usize;
        let columns = self.arrangement.columns.unwrap_or(fit).clamp(1, shown.len());
        let rows = shown.len().div_ceil(columns);
        let gaps = |count: usize| GAP * count.saturating_sub(1) as f32;
        let cell = vec2(
            (width - gaps(columns)) / columns as f32,
            ((ui.available_height() - gaps(rows)) / rows as f32).max(SHORTEST),
        );
        let mut asked = None;
        egui::ScrollArea::vertical()
            .id_salt("machines")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let height = cell.y * rows as f32 + gaps(rows);
                let (area, _) = ui.allocate_exact_size(vec2(width, height), egui::Sense::hover());
                for (i, name) in shown.iter().enumerate() {
                    let at = vec2((i % columns) as f32, (i / columns) as f32);
                    let rect = Rect::from_min_size(area.min + at * (cell + vec2(GAP, GAP)), cell);
                    ui.painter().rect_stroke(
                        rect,
                        4.0,
                        ui.visuals().widgets.noninteractive.bg_stroke,
                        egui::StrokeKind::Inside,
                    );
                    let inner = rect.shrink(6.0);
                    let mut cell_ui = ui.new_child(
                        egui::UiBuilder::new()
                            .max_rect(inner)
                            .id_salt(("machine", name))
                            .layout(egui::Layout::top_down(egui::Align::Min)),
                    );
                    cell_ui.set_clip_rect(inner.intersect(ui.clip_rect()));
                    asked = asked.take().or(self.cell(&mut cell_ui, name));
                }
            });
        asked
    }

    /// Which machines show, in what order and how many columns, which are read for metrics,
    /// and how often the daemon looks.
    pub fn menu(&mut self, ui: &mut Ui) -> Vec<Command> {
        let mut sent = Vec::new();
        // Several boxes are ticked in one visit, so only a click outside closes it.
        let config = egui::containers::menu::MenuConfig::new()
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside);
        egui::containers::menu::MenuButton::new("Arrange")
            .config(config)
            .ui(ui, |ui| self.arrange(ui, &mut sent));
        sent
    }

    fn arrange(&mut self, ui: &mut Ui, sent: &mut Vec<Command>) {
        let ordered = self.ordered();
        let watch = self.watch.clone();
        let mut moved = None;
        egui::Grid::new("arrange").show(ui, |ui| {
            ui.weak("");
            ui.weak("shown");
            ui.weak("metrics");
            ui.end_row();
            for (i, name) in ordered.iter().enumerate() {
                ui.label(name);
                let hidden = &mut self.arrangement.hidden;
                let mut on = !hidden.contains(name);
                if ui.checkbox(&mut on, "").changed() {
                    hidden.retain(|other| other != name);
                    if !on {
                        hidden.push(name.clone());
                    }
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
                            sent.push(Command::DibsWatch(next));
                        }
                    }
                    None => {
                        ui.label("");
                    }
                }
                ui.horizontal(|ui| {
                    if ui.add_enabled(i > 0, egui::Button::new("↑").small()).clicked() {
                        moved = Some((i, i - 1));
                    }
                    let last = i + 1 < ordered.len();
                    if ui.add_enabled(last, egui::Button::new("↓").small()).clicked() {
                        moved = Some((i, i + 1));
                    }
                });
                ui.end_row();
            }
        });
        if let Some((from, to)) = moved {
            let mut order = ordered.clone();
            order.swap(from, to);
            self.arrangement.order = order;
        }
        ui.separator();
        ui.horizontal(|ui| {
            ui.label("columns");
            let columns = &mut self.arrangement.columns;
            ui.selectable_value(columns, None, "fit");
            for count in 1..=4 {
                ui.selectable_value(columns, Some(count), count.to_string());
            }
        });
        let Some(watch) = watch else {
            return;
        };
        let poll = self.poll.get_or_insert(watch.poll);
        let slider = egui::Slider::new(poll, DIBS_POLL)
            .logarithmic(true)
            .suffix(" s")
            .text("look every");
        let slid = ui.add(slider);
        // Each change makes the daemon look again at once, so a drag sends only where it ends.
        let settled = slid.drag_stopped() || (slid.changed() && !slid.dragged());
        if settled && *poll != watch.poll {
            sent.push(Command::DibsWatch(DibsWatch {
                poll: *poll,
                ..watch.clone()
            }));
        }
        if !slid.dragged() && *poll == watch.poll {
            self.poll = None;
        }
    }

    pub fn answered(&mut self, title: String, body: String) {
        let asking = self.asking.as_ref();
        if let Some(tile) = asking.and_then(|name| self.tiles.get_mut(name)) {
            tile.busy = None;
            tile.shown = Some((title, body));
        }
    }

    /// Draws the machine `name`, and returns the dibs call the person asked for.
    fn cell(&mut self, ui: &mut Ui, name: &str) -> Option<Command> {
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
        let keys = self.active.as_deref() == Some(name);
        let tile = self.tiles.entry(name.to_string()).or_default();
        let history = self.histories.entry(name.to_string()).or_default();
        let action = tile.ui(ui, machine, history, keys)?;
        tile.busy = Some(action.doing.clone());
        self.asking = Some(name.to_string());
        Some(Command::Dibs {
            machine: action.machine,
            call: action.call,
            label: action.label,
        })
    }
}

impl Tile {
    fn ui(
        &mut self,
        ui: &mut Ui,
        machine: &DibsMachine,
        history: &History,
        keys: bool,
    ) -> Option<Action> {
        let name = &machine.name;
        let jobs = machine
            .status
            .as_ref()
            .map(|status| Item::all(name, status))
            .unwrap_or_default();
        let mut asked = keys.then(|| self.keys(ui, name, &jobs)).flatten();
        asked = asked.or(self.header(ui, machine));
        match &machine.status {
            Some(status) => {
                meters::ui(ui, status, history, &jobs);
                ui.separator();
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| asked = asked.take().or(self.jobs(ui, &jobs)));
            }
            None => {
                if let Some(trouble) = &machine.trouble {
                    ui.label(RichText::new(plain(trouble)).monospace().weak());
                }
            }
        }
        asked.or(self.overlays(ui, name))
    }

    /// Its state, and the calls about the whole machine.
    fn header(&mut self, ui: &mut Ui, machine: &DibsMachine) -> Option<Action> {
        let mut asked = None;
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(&machine.name).strong().heading());
            match &machine.status {
                Some(status) => {
                    let (words, tone) = state_words(status.state);
                    let colour = match tone {
                        Tone::Free => green(ui),
                        Tone::InUse => ui.visuals().warn_fg_color,
                        Tone::Alarm => ui.visuals().error_fg_color,
                    };
                    ui.label(RichText::new(words).color(colour).strong());
                    if !status.queue.is_empty() {
                        ui.weak(format!("{} queued", status.queue.len()));
                    }
                    if let Some(closed) = &status.closed {
                        ui.colored_label(
                            ui.visuals().error_fg_color,
                            format!("closed by {}", closed.by),
                        );
                    }
                }
                None if machine.trouble.is_some() => {
                    ui.colored_label(ui.visuals().error_fg_color, "down");
                }
                None => {
                    ui.weak("connecting…");
                }
            }
            if let Some(busy) = &self.busy {
                ui.colored_label(Color32::from_rgb(80, 180, 200), format!("{busy}…"));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let name = machine.name.clone();
                if ui.small_button("refresh").on_hover_text("r").clicked() {
                    asked = Some(Action::status(name.clone()));
                }
                if ui.small_button("gpu").on_hover_text("n").clicked() {
                    asked = Some(Action::gpu(name.clone()));
                }
                if ui.small_button("log").on_hover_text("L").clicked() {
                    asked = Some(Action::log(name));
                }
            });
        });
        asked
    }

    fn jobs(&mut self, ui: &mut Ui, jobs: &[Item]) -> Option<Action> {
        if jobs.is_empty() {
            ui.colored_label(green(ui), "nothing holding it, nothing queued");
            return None;
        }
        let mut asked = None;
        for running in [true, false] {
            let group: Vec<&Item> = jobs.iter().filter(|it| it.holding == running).collect();
            if group.is_empty() {
                continue;
            }
            let heading = match running {
                true => RichText::new(format!("Running now ({})", group.len()))
                    .color(green(ui))
                    .strong(),
                false => RichText::new(format!("Waiting ({})", group.len())).weak(),
            };
            ui.label(heading);
            for it in group {
                asked = asked.or(self.card(ui, it));
                ui.add_space(3.0);
            }
            ui.add_space(4.0);
        }
        asked
    }

    /// One job: how it holds the machine, its label and for how long, who and on which card,
    /// and its note; selected, what it runs and what can be done about it.
    fn card(&mut self, ui: &mut Ui, it: &Item) -> Option<Action> {
        let selected = self.selected == Some(it.pid);
        let hue = xterm(agent_hue(&it.agent));
        let mode = match it.mode {
            Mode::Bench => ui.visuals().error_fg_color,
            _ => ui.visuals().warn_fg_color,
        };
        let fill = match (selected, it.holding) {
            (true, _) => ui.visuals().selection.bg_fill.gamma_multiply(0.35),
            (false, true) => green(ui).gamma_multiply(0.1),
            (false, false) => ui.visuals().faint_bg_color,
        };
        let mut asked = None;
        let card = egui::Frame::new()
            .fill(fill)
            .corner_radius(4.0)
            .inner_margin(egui::Margin {
                left: 8,
                right: 6,
                top: 3,
                bottom: 3,
            })
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(RichText::new(it.mode.to_string()).color(mode).strong());
                    let frozen = it.holding && it.slot != "HOLDING";
                    if frozen {
                        ui.label(
                            RichText::new(it.slot.to_lowercase()).color(ui.visuals().warn_fg_color),
                        );
                    }
                    if !it.holding {
                        ui.weak(it.slot.replace("queued ", "#"));
                    }
                    ui.add(
                        egui::Label::new(RichText::new(it.label.as_str()).color(hue).strong())
                            .truncate(),
                    );
                });
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.label(RichText::new(&it.agent).color(hue));
                    if let Some(device) = &it.device {
                        ui.label(
                            RichText::new(device.as_str()).color(Color32::from_rgb(200, 110, 200)),
                        );
                    }
                    if let Some(batch) = &it.batch {
                        ui.weak(batch.progress());
                    } else if let Some(eta) = it.eta.filter(|_| it.holding) {
                        ui.weak(format!("~{} left", Span(eta)));
                    }
                    if let Some(rate) = it.rate {
                        ui.weak(format!("{} cpu", cores(rate)));
                    }
                    let time = Span(it.time).to_string();
                    match it.holding {
                        true => ui.label(format!("for {time}")),
                        false => ui.weak(format!("waited {time}")),
                    };
                });
                if !it.note.is_empty() && !selected {
                    match it.alarm {
                        true => ui.colored_label(ui.visuals().warn_fg_color, &it.note),
                        false => ui.weak(&it.note),
                    };
                }
                if selected {
                    asked = self.detail(ui, it);
                }
            });
        if it.holding {
            let rect = card.response.rect;
            let stripe = egui::Rect::from_min_size(rect.min, egui::vec2(3.0, rect.height()));
            ui.painter().rect_filled(stripe, 2.0, mode);
        }
        if card.response.interact(egui::Sense::click()).clicked() {
            self.selected = (!selected).then_some(it.pid);
        }
        asked
    }

    fn detail(&mut self, ui: &mut Ui, it: &Item) -> Option<Action> {
        let mut asked = None;
        ui.add_space(2.0);
        ui.label(RichText::new(&it.cmd).monospace().small());
        ui.horizontal_wrapped(|ui| {
            ui.weak(format!("pid {}", it.pid));
            if let Some(cpu) = it.cpu {
                ui.weak(format!("{} cpu in all", Span(cpu)));
            }
            if let Some(out) = &it.output {
                ui.weak(format!("writing {out}"));
            }
            if let Some(batch) = &it.batch {
                ui.weak(format!("batch {}: {}", batch.id, batch.step));
                if !batch.next.is_empty() {
                    ui.weak(format!("then {}", batch.next));
                }
            }
        });
        match it.alarm {
            true => ui.colored_label(ui.visuals().warn_fg_color, format!("\u{26a0} {}", it.long)),
            false => ui.weak(&it.long),
        };
        ui.horizontal(|ui| {
            if ui.small_button("process tree").on_hover_text("p").clicked() {
                asked = Some(Action::process_tree(it));
            }
            if it.output.is_some() && ui.small_button("output").on_hover_text("o").clicked() {
                asked = Some(Action::output(it));
            }
            let kill = RichText::new("kill").color(ui.visuals().error_fg_color);
            if ui.small_button(kill).on_hover_text("Shift+K").clicked() {
                self.confirm = Some((it.pid, it.label.clone()));
            }
        });
        asked
    }

    /// dibstop's keys, for the tile last clicked.
    fn keys(&mut self, ui: &Ui, machine: &str, jobs: &[Item]) -> Option<Action> {
        if ui.ctx().text_edit_focused() {
            return None;
        }
        let (pressed, shift) = ui.input(|input| {
            let keys = [
                Key::J,
                Key::K,
                Key::ArrowDown,
                Key::ArrowUp,
                Key::P,
                Key::O,
                Key::N,
                Key::L,
                Key::R,
                Key::Y,
                Key::Escape,
            ];
            let pressed = keys.into_iter().find(|key| input.key_pressed(*key));
            (pressed, input.modifiers.shift)
        });
        let key = pressed?;
        if key == Key::Escape {
            self.confirm = None;
            self.shown = None;
            return None;
        }
        if let Some((pid, label)) = self.confirm.take() {
            return (key == Key::Y).then(|| Action::kill(machine.to_string(), pid, &label));
        }
        if self.shown.is_some() {
            return None;
        }
        let at = jobs.iter().position(|it| Some(it.pid) == self.selected);
        let it = at.and_then(|at| jobs.get(at));
        match key {
            Key::K if shift => {
                self.confirm = it.map(|it| (it.pid, it.label.clone()));
                None
            }
            Key::J | Key::ArrowDown | Key::K | Key::ArrowUp => {
                let down = matches!(key, Key::J | Key::ArrowDown);
                let next = match at {
                    Some(at) if down => (at + 1).min(jobs.len().saturating_sub(1)),
                    Some(at) => at.saturating_sub(1),
                    None => 0,
                };
                self.selected = jobs.get(next).map(|it| it.pid);
                None
            }
            Key::P => it.map(Action::process_tree),
            Key::O => it.filter(|it| it.output.is_some()).map(Action::output),
            Key::N => Some(Action::gpu(machine.to_string())),
            Key::L => Some(Action::log(machine.to_string())),
            Key::R => Some(Action::status(machine.to_string())),
            _ => None,
        }
    }

    fn overlays(&mut self, ui: &Ui, machine: &str) -> Option<Action> {
        let mut asked = None;
        if let Some((pid, label)) = &self.confirm {
            let mut answer = None;
            egui::Modal::new(egui::Id::new(("dibs-kill", machine))).show(ui.ctx(), |ui| {
                ui.heading("Stop this job?");
                ui.label(format!(
                    "Kill {pid} ({label}) on {machine}, whoever started it. It runs dibs --kill {pid} --anyone."
                ));
                ui.horizontal(|ui| {
                    if ui.button("y  kill it").clicked() {
                        answer = Some(true);
                    }
                    if ui.button("Esc  leave it").clicked() {
                        answer = Some(false);
                    }
                });
            });
            if let Some(yes) = answer {
                let (pid, label) = self.confirm.take().expect("shown above");
                asked = yes.then(|| Action::kill(machine.to_string(), pid, &label));
            }
        }
        if let Some((title, body)) = &self.shown {
            let mut close = false;
            egui::Modal::new(egui::Id::new(("dibs-shown", machine))).show(ui.ctx(), |ui| {
                ui.set_max_width(ui.ctx().content_rect().width() * 0.8);
                ui.heading(title);
                egui::ScrollArea::both()
                    .max_height(ui.ctx().content_rect().height() * 0.7)
                    .show(ui, |ui| ui.label(RichText::new(body).monospace()));
                close = ui.button("Esc  close").clicked();
            });
            if close {
                self.shown = None;
            }
        }
        asked
    }
}

fn green(ui: &Ui) -> Color32 {
    match ui.visuals().dark_mode {
        true => Color32::from_rgb(90, 190, 110),
        false => Color32::from_rgb(40, 140, 60),
    }
}

/// An xterm 256-colour index as the colour a terminal shows it in, as dibstop colours agents.
fn xterm(index: u8) -> Color32 {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    match index {
        16..=231 => {
            let cube = index - 16;
            Color32::from_rgb(
                LEVELS[usize::from(cube / 36)],
                LEVELS[usize::from(cube / 6 % 6)],
                LEVELS[usize::from(cube % 6)],
            )
        }
        232.. => {
            let grey = 8 + (index - 232) * 10;
            Color32::from_rgb(grey, grey, grey)
        }
        _ => Color32::GRAY,
    }
}
