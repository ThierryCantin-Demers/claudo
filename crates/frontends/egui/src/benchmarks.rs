//! The team's benchmarks, a table for those that measure the same quantities: each value
//! with a bar against the best of its column and how it moved since the measurement before.

use std::collections::BTreeMap;

use claudo_protocol::{Benchmark, Quantity};
use claudo_room::benchmarks::{Cell, Group, cell, change, commits, groups, number, shares, when};
use egui::{Align2, Color32, FontId, RichText, Sense, Ui, vec2};

const CELL: egui::Vec2 = vec2(118.0, 32.0);
/// A change smaller than this, in percent, is noise between two nights.
const STEADY: f64 = 1.0;

#[derive(Default)]
pub struct Benchmarks {
    list: Vec<Benchmark>,
    /// The point along its axis each table shows, by the table's name.
    points: BTreeMap<String, f64>,
}

impl Benchmarks {
    pub fn heard(&mut self, list: Vec<Benchmark>) {
        self.list = list;
    }

    pub fn any(&self) -> bool {
        !self.list.is_empty()
    }

    pub fn ui(&mut self, ui: &mut Ui) {
        egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
            if self.list.is_empty() {
                ui.weak("No benchmarks recorded yet.");
                return;
            }
            for group in groups(&self.list) {
                let point = self.points.get(&group.name).copied();
                let point = point.or_else(|| group.points.first().copied());
                if let Some(chosen) = table(ui, &group, point) {
                    self.points.insert(group.name.clone(), chosen);
                }
                ui.add_space(14.0);
            }
        });
    }
}

/// One table, and the point along its axis the person picked, if they did.
fn table(ui: &mut Ui, group: &Group, point: Option<f64>) -> Option<f64> {
    let mut chosen = None;
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(&group.name).strong().size(15.0));
        if let Some(axis) = group.axis.filter(|_| group.points.len() > 1) {
            ui.add_space(8.0);
            ui.weak(axis);
            for &at in &group.points {
                if ui
                    .selectable_label(point == Some(at), number_plain(at))
                    .clicked()
                {
                    chosen = Some(at);
                }
            }
        }
        if let Some(newest) = group
            .rows
            .iter()
            .filter_map(|row| row.benchmark.measurements.first())
            .max_by(|a, b| a.at.cmp(&b.at))
        {
            ui.add_space(8.0);
            let mut said = format!("latest {}", when(&newest.at));
            let measured = commits(newest);
            if !measured.is_empty() {
                said.push_str(&format!(" · {measured}"));
            }
            ui.weak(said)
                .on_hover_text(newest.machine.as_deref().unwrap_or(""));
        }
    });
    let cells: Vec<Vec<Option<Cell>>> = group
        .rows
        .iter()
        .map(|row| {
            group
                .quantities
                .iter()
                .map(|quantity| cell(row.benchmark, &quantity.name, point))
                .collect()
        })
        .collect();
    let bars: Vec<Vec<Option<f32>>> = group
        .quantities
        .iter()
        .enumerate()
        .map(|(column, quantity)| {
            let values: Vec<Option<f64>> = cells
                .iter()
                .map(|row| row[column].as_ref().map(|cell| cell.value))
                .collect();
            shares(&values, quantity)
        })
        .collect();
    egui::Grid::new(("benchmarks", &group.name))
        .striped(true)
        .spacing(vec2(10.0, 4.0))
        .show(ui, |ui| {
            ui.label("");
            for quantity in group.quantities {
                ui.add_sized(
                    vec2(CELL.x, 0.0),
                    egui::Label::new(RichText::new(heading(quantity)).small().weak()).wrap(),
                );
            }
            ui.end_row();
            for (r, row) in group.rows.iter().enumerate() {
                ui.label(&row.label)
                    .on_hover_text(&row.benchmark.description);
                for (column, quantity) in group.quantities.iter().enumerate() {
                    match &cells[r][column] {
                        Some(cell) => value(ui, cell, quantity, bars[column][r]),
                        None => {
                            ui.add_sized(CELL, egui::Label::new(RichText::new("·").weak()));
                        }
                    }
                }
                ui.end_row();
            }
        });
    chosen
}

/// A quantity's name, with its unit when the name does not already end with it.
fn heading(quantity: &Quantity) -> String {
    match quantity.unit.is_empty() || quantity.name.ends_with(&quantity.unit) {
        true => quantity.name.clone(),
        false => format!("{} ({})", quantity.name, quantity.unit),
    }
}

fn number_plain(at: f64) -> String {
    match at.fract() == 0.0 {
        true => format!("{at:.0}"),
        false => at.to_string(),
    }
}

/// The value, how it moved since the measurement before, and a bar against the column's best.
fn value(ui: &mut Ui, cell: &Cell, quantity: &Quantity, bar: Option<f32>) {
    let (rect, response) = ui.allocate_exact_size(CELL, Sense::hover());
    let painter = ui.painter_at(rect);
    let visuals = ui.visuals();
    let text = painter.text(
        rect.left_top() + vec2(0.0, 2.0),
        Align2::LEFT_TOP,
        number(cell.value),
        FontId::proportional(15.0),
        visuals.strong_text_color(),
    );
    let moved = change(cell, quantity);
    if let Some((percent, better)) = moved {
        let colour = match (percent.abs() < STEADY, better) {
            (true, _) => visuals.weak_text_color(),
            (false, true) => Color32::from_rgb(90, 185, 110),
            (false, false) => Color32::from_rgb(225, 95, 85),
        };
        let arrow = if percent >= 0.0 { "▲" } else { "▼" };
        painter.text(
            text.right_bottom() + vec2(6.0, -2.0),
            Align2::LEFT_BOTTOM,
            format!("{arrow}{:.0}%", percent.abs()),
            FontId::proportional(11.0),
            colour,
        );
    }
    let track = egui::Rect::from_min_size(
        rect.left_bottom() - vec2(0.0, 6.0),
        vec2(rect.width() - 8.0, 4.0),
    );
    painter.rect_filled(track, 2.0, visuals.extreme_bg_color);
    if let Some(share) = bar {
        let mut filled = track;
        filled.set_width(track.width() * share.clamp(0.0, 1.0));
        let colour = match share >= 0.999 {
            true => visuals.selection.stroke.color,
            false => visuals.selection.bg_fill,
        };
        painter.rect_filled(filled, 2.0, colour);
    }
    response.on_hover_ui(|ui| {
        ui.strong(format!("{} {}", number(cell.value), quantity.unit));
        if let Some(before) = cell.before {
            ui.label(format!("{} the measurement before", number(before)));
        }
        ui.label(match quantity.higher_is_better {
            true => "Higher is better",
            false => "Lower is better",
        });
        match bar {
            Some(share) if share >= 0.999 => {
                ui.label("The best of its column");
            }
            Some(share) => {
                ui.label(format!("{:.0}% of the best of its column", share * 100.0));
            }
            None => {}
        }
        ui.weak(when(&cell.measurement.at));
        if let Some(machine) = &cell.measurement.machine {
            ui.weak(machine);
        }
        let measured = commits(cell.measurement);
        if !measured.is_empty() {
            ui.weak(measured);
        }
    });
}
