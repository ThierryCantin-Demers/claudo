//! What a machine's CPU, memory and cards are doing, with a trace of the last looks as btop
//! draws them, and which job each card runs.

use std::collections::{HashMap, VecDeque};

use dibstop_view::dibs_format::status::{GpuMetrics, Metrics, Status};
use dibstop_view::item::Item;
use serde::{Deserialize, Serialize};

use egui::{Align2, Color32, FontId, Rect, RichText, Sense, Ui, Vec2, vec2};

/// Looks kept for each trace: ten minutes at the default poll.
const KEPT: usize = 60;

const CPU_GRAPH: f32 = 40.0;
const BAR: f32 = 16.0;
/// A card's panel, which several share a row in.
const CARD: f32 = 210.0;
const CARD_GRAPH: f32 = 30.0;
const ROW_BAR: f32 = 90.0;

/// What a machine's cell shows of its metrics, the same for every machine.
#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Show {
    pub cpu: bool,
    pub memory: bool,
    pub cards: Cards,
    pub asleep: bool,
}

impl Default for Show {
    fn default() -> Self {
        Self {
            cpu: true,
            memory: true,
            cards: Cards::Graphs,
            asleep: true,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Cards {
    /// A panel each, with its use as a graph.
    Graphs,
    /// A line each, with its use as a bar.
    Rows,
    Hidden,
}

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

/// The CPU as a graph of its last looks and the memory as a bar across the cell, then a panel
/// per card awake, each with its use as a graph, its memory as a bar and the job on it, as
/// btop draws them; the sleeping cards together after.
pub fn ui(ui: &mut Ui, show: Show, status: &Status, history: &History, jobs: &[Item]) {
    let metrics = status.metrics.as_ref();
    let width = ui.available_width();
    let busy = metrics
        .and_then(|metrics| metrics.cpu_busy_permille)
        .map(|busy| busy as f32 / 1000.0);
    let load = format!("load {:.1}/{}", status.load as f32 / 100.0, status.cores);
    let over = match busy {
        Some(busy) => format!("CPU {}   {load}", percent(busy)),
        None => format!("CPU   {load}"),
    };
    if show.cpu {
        graph(ui, vec2(width, CPU_GRAPH), &history.cpu, busy, &over);
    }
    if let Some((used, total)) = metrics.and_then(memory).filter(|_| show.memory) {
        let share = used as f32 / total.max(1) as f32;
        bar(
            ui,
            vec2(width, BAR),
            share,
            &format!("Mem {}   {}", percent(share), gib(used, total)),
        );
    }
    let gpus = metrics.map_or(&[][..], |metrics| &metrics.gpus[..]);
    let (asleep, awake): (Vec<&GpuMetrics>, Vec<&GpuMetrics>) =
        gpus.iter().partition(|gpu| gpu.suspended);
    if !awake.is_empty() && show.cards == Cards::Rows {
        egui::Grid::new("cards")
            .num_columns(5)
            .spacing(vec2(8.0, 2.0))
            .show(ui, |ui| {
                for gpu in &awake {
                    row(ui, gpu, jobs);
                }
            });
    }
    if !awake.is_empty() && show.cards == Cards::Graphs {
        ui.add_space(4.0);
        // As many panels a row as fit, sharing its width evenly.
        let gap = ui.spacing().item_spacing.x;
        let per_row = (((width + gap) / (CARD + gap)).floor() as usize).clamp(1, awake.len());
        let panel = (width - gap * (per_row - 1) as f32) / per_row as f32;
        for row in awake.chunks(per_row) {
            ui.horizontal_top(|ui| {
                for gpu in row {
                    card(ui, panel, gpu, history.gpus.get(&gpu.pci), jobs);
                }
            });
        }
    }
    if !asleep.is_empty() && show.asleep && show.cards != Cards::Hidden {
        let names: Vec<String> = asleep.iter().map(|gpu| name(gpu)).collect();
        ui.weak(format!("asleep: {}", names.join(", ")))
            .on_hover_text("A card the driver has put to sleep, which reading would wake.");
    }
}

fn memory(metrics: &Metrics) -> Option<(u64, u64)> {
    Some((metrics.mem_used?, metrics.mem_total?))
}

/// The running jobs on `gpu`.
fn running_on<'a>(gpu: &GpuMetrics, jobs: &'a [Item]) -> Vec<&'a Item> {
    jobs.iter()
        .filter(|job| job.holding)
        .filter(|job| {
            let device = job.device.as_ref().map(|device| device.as_str());
            alias(gpu).is_some_and(|alias| device == Some(alias))
        })
        .collect()
}

/// One card as a line: its name, its use and memory as bars, its temperature and its job.
fn row(ui: &mut Ui, gpu: &GpuMetrics, jobs: &[Item]) {
    let on = running_on(gpu, jobs);
    let busy = ui.visuals().warn_fg_color;
    let named = RichText::new(name(gpu)).strong();
    ui.label(match on.is_empty() {
        true => named,
        false => named.color(busy),
    });
    let util = gpu.util_pct.unwrap_or(0) as f32 / 100.0;
    bar(ui, vec2(ROW_BAR, BAR), util, &percent(util));
    match (gpu.mem_used, gpu.mem_total) {
        (Some(used), Some(total)) => {
            let share = used as f32 / total.max(1) as f32;
            bar(ui, vec2(ROW_BAR * 1.5, BAR), share, &gib(used, total));
        }
        _ => {
            ui.label("");
        }
    }
    match gpu.temp_c {
        Some(temp) => {
            let share = (temp as f32 - 30.0) / 60.0;
            ui.label(RichText::new(format!("{temp}°C")).color(level(share)));
        }
        None => {
            ui.label("");
        }
    }
    let names: Vec<&str> = on.iter().map(|job| job.label.as_str()).collect();
    match names.is_empty() {
        true => ui.label(""),
        false => ui.label(RichText::new(format!("← {}", names.join(", "))).color(busy)),
    };
    ui.end_row();
}

fn card(ui: &mut Ui, width: f32, gpu: &GpuMetrics, points: Option<&VecDeque<f32>>, jobs: &[Item]) {
    let on = running_on(gpu, jobs);
    let busy = ui.visuals().warn_fg_color;
    let stroke = match on.is_empty() {
        true => ui.visuals().widgets.noninteractive.bg_stroke,
        false => egui::Stroke::new(1.5, busy),
    };
    egui::Frame::new()
        .stroke(stroke)
        .corner_radius(4.0)
        .inner_margin(4)
        .show(ui, |ui| {
            let inner = width - 8.0 - 2.0 * stroke.width;
            // The row the panel sits in lays out left to right, and a frame keeps its layout.
            ui.vertical(|ui| panel(ui, inner, gpu, points, &on, busy));
        });
}

fn panel(
    ui: &mut Ui,
    inner: f32,
    gpu: &GpuMetrics,
    points: Option<&VecDeque<f32>>,
    on: &[&Item],
    busy: Color32,
) {
    ui.set_width(inner);
    ui.spacing_mut().item_spacing.y = 2.0;
    ui.horizontal(|ui| {
        let named = RichText::new(name(gpu)).strong();
        ui.add(
            egui::Label::new(match on.is_empty() {
                true => named,
                false => named.color(busy),
            })
            .truncate(),
        )
        .on_hover_text(format!(
            "{}\n{}",
            gpu.name.as_deref().unwrap_or("no name from its driver"),
            gpu.pci
        ));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if let Some(temp) = gpu.temp_c {
                let share = (temp as f32 - 30.0) / 60.0;
                ui.label(RichText::new(format!("{temp}°C")).color(level(share)));
            }
        });
    });
    let util = gpu.util_pct.map(|util| util as f32 / 100.0);
    let empty = VecDeque::new();
    let over = util.map_or(String::new(), percent);
    graph(
        ui,
        vec2(inner, CARD_GRAPH),
        points.unwrap_or(&empty),
        util,
        &over,
    );
    if let (Some(used), Some(total)) = (gpu.mem_used, gpu.mem_total) {
        let share = used as f32 / total.max(1) as f32;
        bar(ui, vec2(inner, BAR - 2.0), share, &gib(used, total));
    }
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
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
                    false => busy,
                };
                ui.label(RichText::new(words).color(colour));
            }
        }
    });
    for job in on {
        let label = RichText::new(format!("← {}", job.label.as_str())).color(busy);
        ui.add(egui::Label::new(label).truncate());
    }
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
            let slot = gpu
                .pci
                .split_once(':')
                .map_or(gpu.pci.as_str(), |(_, slot)| slot);
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

/// The last looks as columns, each as tall and as hot as its value, with `over` written on
/// top; `now` stands in until there are looks to draw.
fn graph(ui: &mut Ui, size: Vec2, points: &VecDeque<f32>, now: Option<f32>, over: &str) {
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 3.0, ui.visuals().extreme_bg_color);
    let column = rect.width() / KEPT as f32;
    let single = VecDeque::from_iter(now);
    let points = match points.is_empty() {
        true => &single,
        false => points,
    };
    for (i, value) in points.iter().rev().enumerate() {
        let right = rect.right() - column * i as f32;
        let height = (value * rect.height()).max(if *value > 0.0 { 1.0 } else { 0.0 });
        let bar = Rect::from_min_max(
            egui::pos2(right - column, rect.bottom() - height),
            egui::pos2(right, rect.bottom()),
        );
        painter.rect_filled(bar, 0.0, level(*value));
    }
    caption(&painter, rect, over);
}

/// How full it is, the colour of how full, with `over` written on it.
fn bar(ui: &mut Ui, size: Vec2, share: f32, over: &str) {
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 3.0, ui.visuals().extreme_bg_color);
    let width = rect.width() * share.clamp(0.0, 1.0);
    let full = Rect::from_min_size(rect.min, vec2(width, rect.height()));
    painter.rect_filled(full, 3.0, level(share).gamma_multiply(0.8));
    caption(&painter, rect, over);
}

/// Text over a graph, with a dark edge so it reads over any colour.
fn caption(painter: &egui::Painter, rect: Rect, text: &str) {
    let font = FontId::proportional((rect.height() - 2.0).clamp(10.0, 13.0));
    let at = rect.left_top() + vec2(5.0, 1.0);
    let shadow = Color32::from_black_alpha(200);
    for offset in [vec2(1.0, 1.0), vec2(-1.0, 0.0), vec2(0.0, -1.0)] {
        painter.text(at + offset, Align2::LEFT_TOP, text, font.clone(), shadow);
    }
    painter.text(at, Align2::LEFT_TOP, text, font, Color32::WHITE);
}

/// btop's ramp: green when low, yellow halfway, red when full.
fn level(share: f32) -> Color32 {
    let green = Color32::from_rgb(70, 190, 100);
    let yellow = Color32::from_rgb(225, 195, 60);
    let red = Color32::from_rgb(225, 75, 65);
    let share = share.clamp(0.0, 1.0);
    match share < 0.5 {
        true => green.lerp_to_gamma(yellow, share * 2.0),
        false => yellow.lerp_to_gamma(red, (share - 0.5) * 2.0),
    }
}

fn percent(share: f32) -> String {
    format!("{:.0}%", share * 100.0)
}

fn gib(used: u64, total: u64) -> String {
    let gib = |bytes: u64| bytes as f64 / (1u64 << 30) as f64;
    format!("{:.1}/{:.1} GiB", gib(used), gib(total))
}
