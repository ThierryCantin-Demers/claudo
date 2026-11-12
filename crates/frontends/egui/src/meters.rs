//! What a machine's CPU, memory and cards are doing, with a trace of the last looks as btop
//! draws them, and which job each card runs.

use std::collections::{HashMap, VecDeque};

use dibstop_view::dibs_format::status::{GpuMetrics, Metrics, Status};
use dibstop_view::item::Item;
use egui::{Color32, Rect, RichText, Sense, Stroke, Ui, vec2};

/// Looks kept for each trace: ten minutes at the default poll.
const KEPT: usize = 60;

const BAR: f32 = 64.0;
const TRACE: f32 = 56.0;
const ROW: f32 = 12.0;

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

/// The CPU and memory on one line, which wraps on a narrow tile.
pub fn host(ui: &mut Ui, status: &Status, history: &History) {
    let metrics = status.metrics.as_ref();
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("CPU").strong());
        let busy = metrics
            .and_then(|metrics| metrics.cpu_busy_permille)
            .map(|busy| busy as f32 / 1000.0);
        if let Some(busy) = busy {
            bar(ui, busy, accent(ui));
            trace(ui, &history.cpu, accent(ui));
            ui.label(format!("{:.0}%", busy * 100.0));
        }
        ui.weak(format!(
            "load {:.1} of {}",
            status.load as f32 / 100.0,
            status.cores
        ));
        if let Some((used, total)) = metrics.and_then(memory) {
            ui.add_space(8.0);
            ui.label(RichText::new("Mem").strong());
            let share = used as f32 / total.max(1) as f32;
            bar(ui, share, filling(ui, share));
            ui.label(format!("{} of {}", gib(used), gib(total)));
        }
    });
}

fn memory(metrics: &Metrics) -> Option<(u64, u64)> {
    Some((metrics.mem_used?, metrics.mem_total?))
}

/// A line per card awake, saying which jobs run on it, then the sleeping ones together.
pub fn cards(ui: &mut Ui, status: &Status, history: &History, jobs: &[Item]) {
    let Some(metrics) = &status.metrics else {
        return;
    };
    let (asleep, awake): (Vec<&GpuMetrics>, Vec<&GpuMetrics>) =
        metrics.gpus.iter().partition(|gpu| gpu.suspended);
    for gpu in awake {
        card(ui, gpu, history.gpus.get(&gpu.pci), jobs);
    }
    if !asleep.is_empty() {
        let names: Vec<String> = asleep.iter().map(|gpu| name(gpu)).collect();
        ui.weak(format!("asleep: {}", names.join(", ")))
            .on_hover_text("A card the driver has put to sleep, which reading would wake.");
    }
}

fn card(ui: &mut Ui, gpu: &GpuMetrics, points: Option<&VecDeque<f32>>, jobs: &[Item]) {
    ui.horizontal_wrapped(|ui| {
        let on: Vec<&Item> = jobs
            .iter()
            .filter(|job| job.holding)
            .filter(|job| alias(gpu).is_some_and(|alias| job.device.as_ref().is_some_and(|device| device.as_str() == alias)))
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
        if let Some(util) = gpu.util_pct {
            bar(ui, util as f32 / 100.0, accent(ui));
            if let Some(points) = points {
                trace(ui, points, accent(ui));
            }
            ui.label(format!("{util}%"));
        }
        if let (Some(used), Some(total)) = (gpu.mem_used, gpu.mem_total) {
            let share = used as f32 / total.max(1) as f32;
            let colour = match share {
                s if s >= 0.85 => filling(ui, s),
                _ => ui.visuals().text_color(),
            };
            ui.label(RichText::new(format!("{} of {}", gib(used), gib(total))).color(colour));
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
        for job in on {
            ui.label(RichText::new(format!("← {}", job.label.as_str())).color(ui.visuals().warn_fg_color));
        }
    });
}

/// The alias `--device` takes, once dibs gives it.
fn alias(_gpu: &GpuMetrics) -> Option<&str> {
    None
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

fn bar(ui: &mut Ui, share: f32, fill: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(BAR, ROW), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 2.0, ui.visuals().extreme_bg_color);
    let width = rect.width() * share.clamp(0.0, 1.0);
    painter.rect_filled(Rect::from_min_size(rect.min, vec2(width, ROW)), 2.0, fill);
}

fn trace(ui: &mut Ui, points: &VecDeque<f32>, colour: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(TRACE, ROW), Sense::hover());
    ui.painter()
        .rect_stroke(rect, 2.0, ui.visuals().widgets.noninteractive.bg_stroke, egui::StrokeKind::Inside);
    if points.len() < 2 {
        return;
    }
    let step = rect.width() / (KEPT - 1) as f32;
    let start = rect.right() - step * (points.len() - 1) as f32;
    let line: Vec<egui::Pos2> = points
        .iter()
        .enumerate()
        .map(|(i, value)| {
            egui::pos2(start + step * i as f32, rect.bottom() - value * rect.height())
        })
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
