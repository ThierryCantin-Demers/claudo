//! What a machine's CPU, memory and cards are doing, with a trace of the last looks as btop
//! draws them, and which job each card runs.

use std::collections::{HashMap, VecDeque};

use dibstop_view::dibs_format::status::{GpuMetrics, Metrics, Status};
use dibstop_view::item::Item;
use egui::{Color32, Rect, RichText, Sense, Stroke, Ui, vec2};

/// Looks kept for each trace: ten minutes at the default poll.
const KEPT: usize = 60;

const METER: f32 = 96.0;
const ROW: f32 = 14.0;

/// One machine's last looks, for the traces.
#[derive(Default)]
pub struct History {
    cpu: VecDeque<f32>,
    /// By the card's slot.
    gpus: HashMap<String, VecDeque<f32>>,
    /// When the last look was read, so the same look heard again adds nothing.
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

/// The CPU, memory and each card awake, one row each in aligned columns, then the sleeping
/// cards together. A card says which running jobs are on it.
pub fn ui(ui: &mut Ui, status: &Status, history: &History, jobs: &[Item]) {
    let metrics = status.metrics.as_ref();
    let gpus = metrics.map_or(&[][..], |metrics| &metrics.gpus[..]);
    let (asleep, awake): (Vec<&GpuMetrics>, Vec<&GpuMetrics>) =
        gpus.iter().partition(|gpu| gpu.suspended);
    egui::Grid::new("meters")
        .num_columns(8)
        .spacing(vec2(8.0, 3.0))
        .show(ui, |ui| {
            ui.label(RichText::new("CPU").strong());
            let busy = metrics
                .and_then(|metrics| metrics.cpu_busy_permille)
                .map(|busy| busy as f32 / 1000.0);
            meter(ui, busy, Some(&history.cpu), accent(ui));
            ui.label(busy.map_or(String::new(), percent));
            ui.weak(format!(
                "load {:.1}/{}",
                status.load as f32 / 100.0,
                status.cores
            ));
            ui.end_row();
            if let Some((used, total)) = metrics.and_then(memory) {
                let share = used as f32 / total.max(1) as f32;
                ui.label(RichText::new("Mem").strong());
                meter(ui, Some(share), None, filling(ui, share));
                ui.label(percent(share));
                ui.label(gib(used, total));
                ui.end_row();
            }
            for gpu in awake {
                card(ui, gpu, history.gpus.get(&gpu.pci), jobs);
            }
        });
    if !asleep.is_empty() {
        let names: Vec<String> = asleep.iter().map(|gpu| name(gpu)).collect();
        ui.weak(format!("asleep: {}", names.join(", ")))
            .on_hover_text("A card the driver has put to sleep, which reading would wake.");
    }
}

fn memory(metrics: &Metrics) -> Option<(u64, u64)> {
    Some((metrics.mem_used?, metrics.mem_total?))
}

fn card(ui: &mut Ui, gpu: &GpuMetrics, points: Option<&VecDeque<f32>>, jobs: &[Item]) {
    let on: Vec<&Item> = jobs
        .iter()
        .filter(|job| job.holding)
        .filter(|job| {
            let device = job.device.as_ref().map(|device| device.as_str());
            alias(gpu).is_some_and(|alias| device == Some(alias))
        })
        .collect();
    let named = RichText::new(name(gpu)).strong();
    ui.label(match on.is_empty() {
        true => named,
        false => named.color(ui.visuals().warn_fg_color),
    })
    .on_hover_text(format!(
        "{}\n{}",
        gpu.name.as_deref().unwrap_or("no name from its driver"),
        gpu.pci
    ));
    let util = gpu.util_pct.map(|util| util as f32 / 100.0);
    meter(ui, util, points, accent(ui));
    ui.label(util.map_or(String::new(), percent));
    match (gpu.mem_used, gpu.mem_total) {
        (Some(used), Some(total)) => {
            let share = used as f32 / total.max(1) as f32;
            let colour = match share {
                s if s >= 0.85 => filling(ui, s),
                _ => ui.visuals().text_color(),
            };
            ui.label(RichText::new(gib(used, total)).color(colour));
        }
        _ => {
            ui.label("");
        }
    }
    match gpu.temp_c {
        Some(temp) => {
            let colour = match temp {
                t if t >= 90 => ui.visuals().error_fg_color,
                t if t >= 80 => ui.visuals().warn_fg_color,
                _ => ui.visuals().weak_text_color(),
            };
            ui.label(RichText::new(format!("{temp}°C")).color(colour));
        }
        None => {
            ui.label("");
        }
    }
    ui.weak(gpu.power_mw.map_or(String::new(), |power| format!("{} W", power / 1000)));
    ui.weak(gpu.clock_mhz.map_or(String::new(), |clock| format!("{clock} MHz")));
    ui.horizontal(|ui| {
        for reason in gpu.throttle.iter().flatten() {
            if let Some((words, alarm)) = held_by(reason) {
                let colour = match alarm {
                    true => ui.visuals().error_fg_color,
                    false => ui.visuals().warn_fg_color,
                };
                ui.label(RichText::new(words).color(colour));
            }
        }
        for job in on {
            let label = format!("← {}", job.label.as_str());
            ui.label(RichText::new(label).color(ui.visuals().warn_fg_color));
        }
    });
    ui.end_row();
}

/// What `--device` names it by, which a job's `device` is.
fn alias(gpu: &GpuMetrics) -> Option<&str> {
    gpu.alias.as_deref()
}

/// The card's alias, else its model without its maker's prefix, else its slot.
fn name(gpu: &GpuMetrics) -> String {
    if let Some(alias) = alias(gpu) {
        return alias.trim_start_matches("gpu:").to_string();
    }
    match &gpu.name {
        Some(name) => name
            .trim_start_matches("NVIDIA ")
            .trim_start_matches("GeForce ")
            .to_string(),
        None => {
            let slot = gpu.pci.split_once(':').map_or(gpu.pci.as_str(), |(_, slot)| slot);
            format!("{:?} {slot}", gpu.vendor).to_lowercase()
        }
    }
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

/// How full it is now, and the trace of the last looks drawn over it.
fn meter(ui: &mut Ui, share: Option<f32>, points: Option<&VecDeque<f32>>, fill: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(METER, ROW), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 2.0, ui.visuals().widgets.inactive.bg_fill);
    if let Some(share) = share {
        let width = rect.width() * share.clamp(0.0, 1.0);
        let now = Rect::from_min_size(rect.min, vec2(width, ROW));
        painter.rect_filled(now, 2.0, fill.gamma_multiply(0.55));
    }
    let Some(points) = points.filter(|points| points.len() >= 2) else {
        return;
    };
    let step = rect.width() / (KEPT - 1) as f32;
    let start = rect.right() - step * (points.len() - 1) as f32;
    let line: Vec<egui::Pos2> = points
        .iter()
        .enumerate()
        .map(|(i, value)| {
            egui::pos2(start + step * i as f32, rect.bottom() - 1.0 - value * (rect.height() - 2.0))
        })
        .collect();
    painter.line(line, Stroke::new(1.5, ui.visuals().strong_text_color()));
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

fn percent(share: f32) -> String {
    format!("{:.0}%", share * 100.0)
}

fn gib(used: u64, total: u64) -> String {
    let gib = |bytes: u64| bytes as f64 / (1u64 << 30) as f64;
    format!("{:.1}/{:.1} GiB", gib(used), gib(total))
}
