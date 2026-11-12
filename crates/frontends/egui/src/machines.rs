//! The dibs machines, each in a tile of its own: its state, what its CPU, memory and cards
//! are doing, and its jobs, with dibstop's actions and keys, which the daemon runs.

use std::collections::HashMap;

use claudo_protocol::{Command, DibsMachine, DibsWatch};
use dibstop_view::action::Action;
use dibstop_view::dibs_format::{Label, Mode, Span};
use dibstop_view::item::{Item, Progress as _};
use dibstop_view::text::{Tone, agent_hue, cores, plain, state_words};
use egui::{Color32, Key, RichText, Ui};

use crate::meters::{self, History};

#[derive(Default)]
pub struct Machines {
    machines: Vec<DibsMachine>,
    tiles: HashMap<String, Tile>,
    histories: HashMap<String, History>,
    /// The machine whose tile takes dibstop's keys: the one last clicked.
    active: Option<String>,
    /// The machine whose tile asked for what the daemon answers next.
    asking: Option<String>,
    /// How the daemon watches them, once it says.
    pub watch: Option<DibsWatch>,
}

/// What one machine's tile remembers between frames.
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

    pub fn names(&self) -> Vec<String> {
        self.machines.iter().map(|machine| machine.name.clone()).collect()
    }

    pub fn answered(&mut self, title: String, body: String) {
        let asking = self.asking.as_ref();
        if let Some(tile) = asking.and_then(|name| self.tiles.get_mut(name)) {
            tile.busy = None;
            tile.shown = Some((title, body));
        }
    }

    /// Draws the machine `name`, and returns the dibs call the person asked for.
    pub fn ui(&mut self, ui: &mut Ui, name: &str) -> Option<Command> {
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
                meters::host(ui, status, history);
                meters::cards(ui, status, history, &jobs);
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
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let time = Span(it.time).to_string();
                        match it.holding {
                            true => ui.label(format!("for {time}")),
                            false => ui.weak(format!("waited {time}")),
                        };
                        ui.add(
                            egui::Label::new(RichText::new(it.label.as_str()).color(hue).strong())
                                .truncate(),
                        );
                    });
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
