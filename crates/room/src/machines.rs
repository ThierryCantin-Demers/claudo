//! The dibs machines: how the person arranges them, the last looks at each for its traces,
//! and what their metrics say in words and colour.

use std::collections::{HashMap, VecDeque};

use dibstop_view::dibs_format::status::{Disk, DiskRole, GpuMetrics, Metrics, QuotaKind, Status};
use dibstop_view::item::Item;
use serde::{Deserialize, Serialize};

/// Looks kept for each trace: ten minutes at the default poll.
pub const KEPT: usize = 60;

/// How the person arranged the machines.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Arrangement {
    /// The machines in the order shown; one dibs adds later goes last.
    pub order: Vec<String>,
    pub hidden: Vec<String>,
    /// None fits as many as the width takes.
    pub columns: Option<usize>,
    #[serde(default)]
    pub show: Show,
}

impl Arrangement {
    /// `names` in the person's order.
    pub fn ordered(&self, mut names: Vec<String>) -> Vec<String> {
        let at = |name: &String| {
            let order = &self.order;
            order
                .iter()
                .position(|other| other == name)
                .unwrap_or(order.len())
        };
        names.sort_by_key(at);
        names
    }

    /// Those of `names` shown, in the person's order.
    pub fn shown(&self, names: Vec<String>) -> Vec<String> {
        self.ordered(names)
            .into_iter()
            .filter(|name| !self.hidden.contains(name))
            .collect()
    }

    /// Shows or hides `name`.
    pub fn set_shown(&mut self, name: &str, shown: bool) {
        self.hidden.retain(|other| other != name);
        if !shown {
            self.hidden.push(name.to_string());
        }
    }

    /// Swaps the machines at `from` and `to` among `names` in the person's order.
    pub fn swap(&mut self, names: Vec<String>, from: usize, to: usize) {
        let mut order = self.ordered(names);
        if from < order.len() && to < order.len() {
            order.swap(from, to);
            self.order = order;
        }
    }

    /// How many columns of cells, of at least `narrowest` each, across `width`.
    pub fn columns(&self, shown: usize, width: f32, narrowest: f32, gap: f32) -> usize {
        let fit = ((width + gap) / (narrowest + gap)).floor() as usize;
        self.columns.unwrap_or(fit).clamp(1, shown.max(1))
    }
}

/// What a machine's cell shows of its metrics, the same for every machine.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Show {
    pub cpu: bool,
    pub memory: bool,
    pub disks: bool,
    pub cards: Cards,
    pub asleep: bool,
}

impl Default for Show {
    fn default() -> Self {
        Self {
            cpu: true,
            memory: true,
            disks: true,
            cards: Cards::Graphs,
            asleep: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
    pub cpu: VecDeque<f32>,
    /// By the card's slot.
    pub gpus: HashMap<String, VecDeque<f32>>,
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

/// The memory used and in all.
pub fn memory(metrics: &Metrics) -> Option<(u64, u64)> {
    Some((metrics.mem_used?, metrics.mem_total?))
}

/// How full a filesystem is to this account, and what to write on its bar: its own free
/// space, or a quota's room when that is less, since either stops a job the same way.
pub fn filled(disk: &Disk) -> (f32, String) {
    let role = match disk.role {
        DiskRole::Scratch => "scratch",
        DiskRole::Tmp => "tmp",
        DiskRole::SystemTmp => "/tmp",
    };
    let disk_share = 1.0 - disk.free as f32 / disk.total.max(1) as f32;
    let tightest = disk
        .quotas
        .iter()
        .min_by_key(|quota| quota.limit.saturating_sub(quota.used));
    match tightest {
        Some(quota) if quota.limit.saturating_sub(quota.used) < disk.free => {
            let share = quota.used as f32 / quota.limit.max(1) as f32;
            let kind = match quota.kind {
                QuotaKind::User => "user",
                QuotaKind::Group => "group",
                QuotaKind::Project => "project",
            };
            let over = format!(
                "{role} {}   {} free of its {kind} quota of {}",
                percent(share),
                size(quota.limit.saturating_sub(quota.used)),
                size(quota.limit)
            );
            (share.max(disk_share), over)
        }
        _ => {
            let over = format!(
                "{role} {}   {} free of {}",
                percent(disk_share),
                size(disk.free),
                size(disk.total)
            );
            (disk_share, over)
        }
    }
}

pub fn size(bytes: u64) -> String {
    const GIB: f64 = (1u64 << 30) as f64;
    match bytes as f64 / GIB {
        gib if gib >= 1024.0 => format!("{:.1} TiB", gib / 1024.0),
        gib => format!("{gib:.0} GiB"),
    }
}

/// The running jobs on `gpu`.
pub fn running_on<'a>(gpu: &GpuMetrics, jobs: &'a [Item]) -> Vec<&'a Item> {
    jobs.iter()
        .filter(|job| job.holding)
        .filter(|job| {
            let device = job.device.as_ref().map(|device| device.as_str());
            alias(gpu).is_some_and(|alias| device == Some(alias))
        })
        .collect()
}

/// What `--device` names it by, which a job's `device` is.
pub fn alias(gpu: &GpuMetrics) -> Option<&str> {
    gpu.alias.as_deref()
}

/// The card's alias, else its model without its maker's prefix, else its slot.
pub fn card_name(gpu: &GpuMetrics) -> String {
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
pub fn held_by(reason: &str) -> Option<(&'static str, bool)> {
    match reason {
        "hw_thermal" => Some(("too hot (hw)", true)),
        "sw_thermal" => Some(("too hot", true)),
        "hw_slowdown" => Some(("hw slowdown", true)),
        "hw_power_brake" => Some(("power brake", true)),
        "sw_power_cap" => Some(("at power cap", false)),
        _ => None,
    }
}

/// btop's ramp, as RGB: green when low, yellow halfway, red when full.
pub fn level(share: f32) -> [u8; 3] {
    const GREEN: [f32; 3] = [70.0, 190.0, 100.0];
    const YELLOW: [f32; 3] = [225.0, 195.0, 60.0];
    const RED: [f32; 3] = [225.0, 75.0, 65.0];
    let share = share.clamp(0.0, 1.0);
    let (from, to, t) = match share < 0.5 {
        true => (GREEN, YELLOW, share * 2.0),
        false => (YELLOW, RED, (share - 0.5) * 2.0),
    };
    std::array::from_fn(|i| (from[i] + (to[i] - from[i]) * t).round() as u8)
}

/// A card's temperature as a share of the range it is coloured over.
pub fn heat(celsius: i32) -> f32 {
    (celsius as f32 - 30.0) / 60.0
}

pub fn percent(share: f32) -> String {
    format!("{:.0}%", share * 100.0)
}

pub fn gib(used: u64, total: u64) -> String {
    let gib = |bytes: u64| bytes as f64 / (1u64 << 30) as f64;
    format!("{:.1}/{:.1} GiB", gib(used), gib(total))
}
