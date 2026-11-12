//! The team's benchmarks, compared one quantity at a time: the latest night of every
//! benchmark that measures it side by side, each against a night the person picks, and
//! each benchmark's nights on a chart.

use std::collections::BTreeMap;

use claudo_protocol::{Benchmark, Quantity};
use claudo_room::benchmarks::{
    Group, SERIES, cell, commits, date, families_and_variants, groups, history, measured_days,
    moved, number, on_day, split, when,
};
use egui::{Align2, Color32, FontId, RichText, Sense, Ui, vec2};
use egui_plot::{
    Corner, HoverPosition, Legend, Line, LineStyle, MarkerShape, Plot, PlotPoints, Points,
};

/// A change smaller than this, in percent, is noise between two nights.
const STEADY: f64 = 1.0;
const BAR_ROW: f32 = 22.0;
const NAME: f32 = 150.0;
/// Wider than this, the bars and the chart sit side by side.
const SIDE_BY_SIDE: f32 = 900.0;

#[derive(Default)]
pub struct Benchmarks {
    list: Vec<Benchmark>,
    /// The table shown, by name.
    shown: Option<String>,
    /// What the person picked in each table, by its name.
    picked: BTreeMap<String, Picked>,
}

#[derive(Default, Clone)]
struct Picked {
    quantity: Option<String>,
    point: Option<f64>,
    /// The day the latest values are compared with.
    against: Option<String>,
}

impl Benchmarks {
    pub fn heard(&mut self, list: Vec<Benchmark>) {
        self.list = list;
    }

    pub fn any(&self) -> bool {
        !self.list.is_empty()
    }

    pub fn ui(&mut self, ui: &mut Ui) {
        if self.list.is_empty() {
            ui.weak("No benchmarks recorded yet.");
            return;
        }
        let groups = groups(&self.list);
        let shown = self
            .shown
            .as_ref()
            .and_then(|name| groups.iter().find(|group| group.name == *name))
            .unwrap_or(&groups[0]);
        ui.horizontal_wrapped(|ui| {
            for group in &groups {
                let label = RichText::new(&group.name).size(15.0);
                if ui
                    .selectable_label(group.name == shown.name, label)
                    .clicked()
                {
                    self.shown = Some(group.name.clone());
                }
            }
        });
        ui.separator();
        let picked = self.picked.entry(shown.name.clone()).or_default();
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show(ui, |ui| compare(ui, shown, picked));
    }
}

fn compare(ui: &mut Ui, group: &Group, picked: &mut Picked) {
    let quantity = picked
        .quantity
        .as_ref()
        .and_then(|name| group.quantities.iter().find(|q| q.name == *name))
        .unwrap_or(&group.quantities[0]);
    let point = picked
        .point
        .filter(|point| group.points.contains(point))
        .or_else(|| group.points.first().copied());
    let days = measured_days(group);
    let against = picked
        .against
        .clone()
        .filter(|day| days.contains(&day.as_str()))
        .or_else(|| days.get(1).map(|day| day.to_string()));

    ui.horizontal_wrapped(|ui| {
        for each in group.quantities {
            if ui
                .selectable_label(each.name == quantity.name, &each.name)
                .clicked()
            {
                picked.quantity = Some(each.name.clone());
            }
        }
    });
    ui.horizontal_wrapped(|ui| {
        if let Some(axis) = group.axis.filter(|_| !group.points.is_empty()) {
            ui.weak(format!("{axis}:"));
            for &at in &group.points {
                if ui.selectable_label(point == Some(at), plain(at)).clicked() {
                    picked.point = Some(at);
                }
            }
            ui.add_space(12.0);
        }
        ui.weak("compared with");
        let named = |day: &Option<String>| match day {
            Some(day) => day.clone(),
            None => "nothing".to_string(),
        };
        egui::ComboBox::from_id_salt(("against", &group.name))
            .selected_text(named(&against))
            .show_ui(ui, |ui| {
                for day in days.iter().skip(1) {
                    if ui
                        .selectable_label(against.as_deref() == Some(day), *day)
                        .clicked()
                    {
                        picked.against = Some(day.to_string());
                    }
                }
                if days.len() < 2 {
                    ui.weak("One night so far");
                }
            });
        ui.add_space(12.0);
        ui.weak(match quantity.higher_is_better {
            true => format!("{}, higher is better", quantity.unit),
            false => format!("{}, lower is better", quantity.unit),
        });
    });
    ui.add_space(6.0);

    let width = ui.available_width();
    if width >= SIDE_BY_SIDE {
        ui.horizontal_top(|ui| {
            let half = (width - ui.spacing().item_spacing.x) / 2.0;
            ui.vertical(|ui| {
                ui.set_width(half);
                bars(ui, group, quantity, point, against.as_deref());
            });
            ui.vertical(|ui| {
                ui.set_width(half);
                chart(ui, group, quantity, point);
            });
        });
    } else {
        bars(ui, group, quantity, point, against.as_deref());
        ui.add_space(10.0);
        chart(ui, group, quantity, point);
    }
}

/// A row each, under its family: the variant's colour and name, its latest value as a bar
/// from zero, and how it moved since `against`.
fn bars(
    ui: &mut Ui,
    group: &Group,
    quantity: &Quantity,
    point: Option<f64>,
    against: Option<&str>,
) {
    let (_, variants) = families_and_variants(group);
    let latest: Vec<_> = group
        .rows
        .iter()
        .map(|row| cell(row.benchmark, &quantity.name, point))
        .collect();
    let longest = latest
        .iter()
        .flatten()
        .map(|cell| cell.value)
        .fold(0.0, f64::max);
    let best = latest
        .iter()
        .flatten()
        .map(|cell| cell.value)
        .reduce(|a, b| match quantity.higher_is_better {
            true => a.max(b),
            false => a.min(b),
        });
    let mut family = None;
    for (row, latest) in group.rows.iter().zip(&latest) {
        let (this, variant) = split(&row.label);
        if this != family {
            family = this;
            if let Some(name) = this {
                ui.add_space(4.0);
                ui.label(RichText::new(name).strong());
            }
        }
        let colour = colour(&variants, variant);
        let (rect, response) =
            ui.allocate_exact_size(vec2(ui.available_width(), BAR_ROW), Sense::hover());
        let painter = ui.painter_at(rect);
        let visuals = ui.visuals();
        let swatch =
            egui::Rect::from_center_size(rect.left_center() + vec2(6.0, 0.0), vec2(10.0, 10.0));
        painter.rect_filled(swatch, 2.0, colour);
        painter.text(
            rect.left_center() + vec2(18.0, 0.0),
            Align2::LEFT_CENTER,
            variant,
            FontId::proportional(13.0),
            visuals.text_color(),
        );
        let track = egui::Rect::from_min_max(
            egui::pos2(rect.left() + NAME, rect.top() + 4.0),
            egui::pos2(rect.right() - 130.0, rect.bottom() - 4.0),
        );
        let Some(latest) = latest else {
            painter.text(
                track.left_center(),
                Align2::LEFT_CENTER,
                "not measured",
                FontId::proportional(12.0),
                visuals.weak_text_color(),
            );
            continue;
        };
        let share = match longest > 0.0 {
            true => (latest.value / longest) as f32,
            false => 0.0,
        };
        let mut filled = track;
        filled.set_width(track.width() * share.clamp(0.0, 1.0));
        painter.rect_filled(filled, 3.0, colour.gamma_multiply(0.85));
        let is_best = best == Some(latest.value);
        let value = painter.text(
            filled.right_center() + vec2(6.0, 0.0),
            Align2::LEFT_CENTER,
            number(latest.value),
            FontId::proportional(13.0),
            match is_best {
                true => visuals.strong_text_color(),
                false => visuals.text_color(),
            },
        );
        let then = against.and_then(|day| on_day(row.benchmark, &quantity.name, point, day));
        let change = then.and_then(|then| moved(latest.value, then, quantity));
        if let Some((percent, better)) = change {
            painter.text(
                value.right_center() + vec2(8.0, 0.0),
                Align2::LEFT_CENTER,
                format!(
                    "{}{:.1}%",
                    if percent >= 0.0 { "+" } else { "−" },
                    percent.abs()
                ),
                FontId::proportional(12.0),
                shade(ui, percent, better),
            );
        }
        response.on_hover_ui(|ui| {
            ui.strong(&row.label);
            ui.label(format!("{} {}", number(latest.value), quantity.unit));
            if is_best {
                ui.label("The best of these");
            }
            if let (Some(then), Some(day)) = (then, against) {
                ui.label(format!("{} {} on {day}", number(then), quantity.unit));
            }
            ui.weak(when(&latest.measurement.at));
            if let Some(machine) = &latest.measurement.machine {
                ui.weak(machine);
            }
            ui.weak(commits(latest.measurement));
            if !row.benchmark.description.is_empty() {
                ui.add_space(4.0);
                ui.weak(&row.benchmark.description);
            }
        });
    }
}

/// Each benchmark's nights, a line each: its variant's colour, and a dash for each family
/// after the first.
fn chart(ui: &mut Ui, group: &Group, quantity: &Quantity, point: Option<f64>) {
    let (families, variants) = families_and_variants(group);
    let unit = quantity.unit.clone();
    let mut nights = 0;
    Plot::new(("over time", &group.name))
        .height(320.0)
        .legend(
            Legend::default()
                .position(Corner::LeftTop)
                .follow_insertion_order(true),
        )
        .x_axis_formatter(|mark, _| date(mark.value))
        .y_axis_formatter(|mark, _| number(mark.value))
        .label_formatter(move |hover| match hover {
            HoverPosition::NearDataPoint {
                plot_name,
                position,
                ..
            } => Some(format!(
                "{plot_name}\n{} {unit}\n{}",
                number(position.y),
                date(position.x)
            )),
            HoverPosition::Elsewhere { .. } => None,
        })
        .allow_scroll(false)
        .show(ui, |plot| {
            for row in &group.rows {
                let found = history(row.benchmark, &quantity.name, point);
                nights = nights.max(found.len());
                if found.is_empty() {
                    continue;
                }
                let (family, variant) = split(&row.label);
                let colour = colour(&variants, variant);
                let style = match families.iter().position(|f| *f == family) {
                    Some(0) | None => LineStyle::Solid,
                    Some(1) => LineStyle::dashed_loose(),
                    _ => LineStyle::dotted_dense(),
                };
                let at: Vec<[f64; 2]> = found.iter().map(|(x, y, _)| [*x, *y]).collect();
                plot.line(
                    Line::new(row.label.clone(), PlotPoints::from(at.clone()))
                        .color(colour)
                        .style(style)
                        .width(2.0),
                );
                plot.points(
                    Points::new(row.label.clone(), PlotPoints::from(at))
                        .color(colour)
                        .shape(MarkerShape::Circle)
                        .filled(true)
                        .radius(3.5),
                );
            }
        });
    if nights < 2 {
        ui.weak("One night so far: each line starts with the next.");
    }
}

fn colour(variants: &[&str], variant: &str) -> Color32 {
    let index = variants.iter().position(|v| *v == variant).unwrap_or(0);
    let [r, g, b] = SERIES[index % SERIES.len()];
    Color32::from_rgb(r, g, b)
}

fn shade(ui: &Ui, percent: f64, better: bool) -> Color32 {
    match (percent.abs() < STEADY, better) {
        (true, _) => ui.visuals().weak_text_color(),
        (false, true) => Color32::from_rgb(70, 180, 100),
        (false, false) => Color32::from_rgb(225, 90, 80),
    }
}

fn plain(at: f64) -> String {
    match at.fract() == 0.0 {
        true => format!("{at:.0}"),
        false => at.to_string(),
    }
}
