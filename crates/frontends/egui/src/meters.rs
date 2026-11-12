//! What a machine's CPU, memory and cards are doing, as meters with a trace of the last looks,
//! as btop shows them.

use std::collections::{HashMap, VecDeque};

use dibstop_view::dibs_format::status::{GpuMetrics, Metrics, Status};
use egui::{Color32, Rect, RichText, Sense, Stroke, Ui, vec2};

/// Looks kept for each trace: ten minutes at the default poll.
const KEPT: usize = 60;

const BAR: f32 = 120.0;
const TRACE: f32 = 72.0;
const ROW: f32 = 14.0;

/// One machine's last looks, for the traces.
#[derive(Default)]
pub struct History {
    cpu: VecDeque<f32>,
    /// By the card's slot.
    gpus: HashMap<String, VecDeque<f32>>,
    /// When the last look was read, so a page redrawn without a new one keeps its trace.
    at: u64,
}

impl History {
    pub fn heard(&mut self, status: &Status) {
        let Some(metrics) = &status.metrics else {
            return;
        };
        if status.t == self.at {
            return;
        }
        self.at = status.t;
        if let Some(busy) = metrics.cpu_busy_permille {
            push(&mut self.cpu, busy as f32 / 1000.0);
        }
        for gpu in &metrics.gpus {
            if let Some(util) = gpu.util_pct {
                push(
                    self.gpus.entry(gpu.pci.clone()).or_default(),
                    util as f32 / 100.0,
                );
            }
        }
    }
}

fn push(trace: &mut VecDeque<f32>, value: f32) {
    if trace.len() == KEPT {
        trace.pop_front();
    }
    trace.push_back(value.clamp(0.0, 1.0));
}

/// The meters, when the machine's metrics were read.
pub fn ui(ui: &mut Ui, status: &Status, history: &History) {
    let Some(metrics) = &status.metrics else {
        return;
    };
    egui::Grid::new("meters")
        .num_columns(4)
        .spacing(vec2(8.0, 2.0))
        .show(ui, |ui| {
            cpu(ui, status, metrics, history);
            memory(ui, metrics);
            for gpu in &metrics.gpus {
                card(ui, gpu, history.gpus.get(&gpu.pci));
            }
        });
    ui.add_space(4.0);
}

fn cpu(ui: &mut Ui, status: &Status, metrics: &Metrics, history: &History) {
    ui.label(RichText::new("CPU").strong());
    let busy = metrics.cpu_busy_permille.map(|busy| busy as f32 / 1000.0);
    bar(ui, busy, accent(ui));
    let load = format!("load {:.1} of {}", status.load as f32 / 100.0, status.cores);
    match busy {
        Some(busy) => ui.label(format!("{:.0}%  {load}", busy * 100.0)),
        None => ui.weak(load),
    };
    trace(ui, &history.cpu, accent(ui));
    ui.end_row();
}

fn memory(ui: &mut Ui, metrics: &Metrics) {
    let (Some(used), Some(total)) = (metrics.mem_used, metrics.mem_total) else {
        return;
    };
    ui.label(RichText::new("Mem").strong());
    let share = used as f32 / total.max(1) as f32;
    bar(ui, Some(share), filling(ui, share));
    ui.label(format!("{} of {}", gib(used), gib(total)));
    ui.label("");
    ui.end_row();
}

fn card(ui: &mut Ui, gpu: &GpuMetrics, trace_of: Option<&VecDeque<f32>>) {
    let name = gpu.name.clone().unwrap_or_else(|| {
        let slot = gpu.pci.split_once(':').map_or(gpu.pci.as_str(), |(_, slot)| slot);
        format!("{:?} {slot}", gpu.vendor).to_lowercase()
    });
    ui.label(RichText::new("GPU").strong())
        .on_hover_text(format!("{name}\n{}", gpu.pci));
    if gpu.suspended {
        ui.weak("asleep");
        ui.weak(short(&name));
        ui.label("");
        ui.end_row();
        return;
    }
    let util = gpu.util_pct.map(|util| util as f32 / 100.0);
    bar(ui, util, accent(ui));
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        if let Some(util) = gpu.util_pct {
            ui.label(format!("{util}%"));
        }
        if let (Some(used), Some(total)) = (gpu.mem_used, gpu.mem_total) {
            let share = used as f32 / total.max(1) as f32;
            ui.label(RichText::new(format!("{} of {}", gib(used), gib(total))).color(
                match share {
                    s if s >= 0.95 => ui.visuals().error_fg_color,
                    s if s >= 0.85 => ui.visuals().warn_fg_color,
                    _ => ui.visuals().text_color(),
                },
            ));
        }
        if let Some(temp) = gpu.temp_c {
            let colour = match temp {
                t if t >= 90 => ui.visuals().error_fg_color,
                t if t >= 80 => ui.visuals().warn_fg_color,
                _ => ui.visuals().weak_text_color(),
            };
            ui.label(RichText::new(format!("{temp}°C")).color(colour));
        }
        if let Some(power) = gpu.power_mw {
            ui.weak(format!("{} W", power / 1000));
        }
        if let Some(clock) = gpu.clock_mhz {
            ui.weak(format!("{clock} MHz"));
        }
        for reason in gpu.throttle.iter().flatten() {
            if let Some((words, alarm)) = held_by(reason) {
                let colour = match alarm {
                    true => ui.visuals().error_fg_color,
                    false => ui.visuals().warn_fg_color,
                };
                ui.label(RichText::new(words).color(colour));
            }
        }
        ui.weak(short(&name));
    });
    match trace_of {
        Some(points) => trace(ui, points, accent(ui)),
        None => {
            ui.label("");
        }
    }
    ui.end_row();
}

/// What a throttle reason means to someone measuring, and whether it spoils the numbers.
/// An idle card, application clocks and the rest are how a card normally runs.
fn held_by(reason: &str) -> Option<(&'static str, bool)> {
    match reason {
        "hw_thermal" => Some(("too hot (hw)", true)),
        "sw_thermal" => Some(("too hot", true)),
        "hw_slowdown" => Some(("hw slowdown", true)),
        "hw_power_brake" => Some(("power brake", true)),
        "sw_power_cap" => Some(("at power cap", false)),
        _ => None,
    }
}

/// The card's model without its maker's prefix, which the tile has no room for.
fn short(name: &str) -> String {
    name.trim_start_matches("NVIDIA ")
        .trim_start_matches("GeForce ")
        .to_string()
}

fn bar(ui: &mut Ui, share: Option<f32>, fill: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(BAR, ROW), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 3.0, ui.visuals().faint_bg_color);
    if let Some(share) = share {
        let width = rect.width() * share.clamp(0.0, 1.0);
        painter.rect_filled(Rect::from_min_size(rect.min, vec2(width, ROW)), 3.0, fill);
    }
}

fn trace(ui: &mut Ui, points: &VecDeque<f32>, colour: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(TRACE, ROW), Sense::hover());
    if points.len() < 2 {
        return;
    }
    let step = rect.width() / (KEPT - 1) as f32;
    let start = rect.right() - step * (points.len() - 1) as f32;
    let line: Vec<egui::Pos2> = points
        .iter()
        .enumerate()
        .map(|(i, value)| egui::pos2(start + step * i as f32, rect.bottom() - value * rect.height()))
        .collect();
    ui.painter().line(line, Stroke::new(1.5, colour));
}

fn accent(ui: &Ui) -> Color32 {
    ui.visuals().selection.bg_fill
}

fn filling(ui: &Ui, share: f32) -> Color32 {
    match share {
        s if s >= 0.95 => ui.visuals().error_fg_color,
        s if s >= 0.85 => ui.visuals().warn_fg_color,
        _ => accent(ui),
    }
}

fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / (1u64 << 30) as f64)
}
