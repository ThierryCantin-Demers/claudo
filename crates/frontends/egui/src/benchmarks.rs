//! The team's benchmarks: an overview of how fast each is and how it moved since its night
//! before, and for one quantity at a time, every benchmark side by side and over the nights.

use std::collections::BTreeMap;

use claudo_protocol::Quantity;
use claudo_room::benchmarks::{
    Group, Row, Trend, cell, commits, date, families_and_variants, groups, history, moved, nights,
    number, on_night, series_colour, split, trend, when,
};
use egui::{Align2, Color32, FontId, RichText, Sense, Ui, vec2};
use egui_plot::{
    Corner, HoverPosition, Legend, Line, LineStyle, MarkerShape, Plot, PlotPoints, Points,
    uniform_grid_spacer,
};

/// A change smaller than this, in percent, is noise between two nights.
const STEADY: f64 = 2.0;
/// A change this large, in percent, is tinted as strongly as a tint goes.
const LOUD: f64 = 20.0;
const CELL: egui::Vec2 = vec2(144.0, 38.0);
const BAR_ROW: f32 = 22.0;
const BARS: f32 = 640.0;
const NAME: f32 = 150.0;
/// Wider than this, the bars and the chart sit side by side.
const SIDE_BY_SIDE: f32 = 1000.0;
const MOVERS: usize = 6;

#[derive(Default)]
pub struct Benchmarks {
    list: Vec<claudo_protocol::Benchmark>,
    /// The table compared, by name, or the overview.
    shown: Option<String>,
    /// What the person picked in each table, by its name.
    picked: BTreeMap<String, Picked>,
}

#[derive(Default, Clone)]
struct Picked {
    quantity: Option<String>,
    point: Option<f64>,
    /// The night the latest values are compared with.
    against: Option<i64>,
}

impl Picked {
    /// Along the axis, the point picked, or the last, which loads the most.
    fn point(&self, group: &Group) -> Option<f64> {
        self.point
            .filter(|point| group.points.contains(point))
            .or_else(|| group.points.last().copied())
    }
}

/// One quantity of one benchmark, as the overview counts it.
struct Seen<'a> {
    group: &'a str,
    row: &'a Row<'a>,
    quantity: &'a Quantity,
    point: Option<f64>,
    axis: Option<&'a str>,
    trend: Trend<'a>,
}

impl Seen<'_> {
    fn change(&self) -> Option<(f64, bool)> {
        let (before, _) = self.trend.before?;
        moved(self.trend.now, before, self.quantity)
    }
}

impl Benchmarks {
    pub fn heard(&mut self, list: Vec<claudo_protocol::Benchmark>) {
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
        let list = std::mem::take(&mut self.list);
        let groups = groups(&list);
        let shown = self
            .shown
            .as_ref()
            .and_then(|name| groups.iter().find(|group| group.name == *name));
        ui.horizontal_wrapped(|ui| {
            let tab = |text: &str| RichText::new(text).size(15.0);
            if ui
                .selectable_label(shown.is_none(), tab("Overview"))
                .clicked()
            {
                self.shown = None;
            }
            ui.separator();
            for group in &groups {
                let on = shown.is_some_and(|shown| shown.name == group.name);
                if ui.selectable_label(on, tab(&group.name)).clicked() {
                    self.shown = Some(group.name.clone());
                }
            }
        });
        ui.separator();
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show(ui, |ui| match shown {
                None => self.overview(ui, &groups),
                Some(group) => {
                    let picked = self.picked.entry(group.name.clone()).or_default();
                    compare(ui, group, picked);
                }
            });
        self.list = list;
    }

    fn overview(&mut self, ui: &mut Ui, groups: &[Group]) {
        let points: Vec<Option<f64>> = groups
            .iter()
            .map(|group| {
                self.picked
                    .get(&group.name)
                    .cloned()
                    .unwrap_or_default()
                    .point(group)
            })
            .collect();
        let seen: Vec<Seen> = groups
            .iter()
            .zip(&points)
            .flat_map(|(group, point)| {
                group.rows.iter().flat_map(move |row| {
                    group.quantities.iter().filter_map(move |quantity| {
                        Some(Seen {
                            group: &group.name,
                            row,
                            quantity,
                            point: *point,
                            axis: group.axis,
                            trend: trend(row.benchmark, &quantity.name, *point)?,
                        })
                    })
                })
            })
            .collect();
        summary(ui, &seen);
        ui.add_space(12.0);
        for (group, point) in groups.iter().zip(&points) {
            if let Some(quantity) = matrix(ui, group, *point) {
                let picked = self.picked.entry(group.name.clone()).or_default();
                picked.quantity = Some(quantity);
                self.shown = Some(group.name.clone());
            }
            ui.add_space(14.0);
        }
    }
}

/// The newest night, how many values moved since their night before, and the most moved.
fn summary(ui: &mut Ui, seen: &[Seen]) {
    let Some(newest) = seen.iter().map(|seen| seen.trend.night).max() else {
        return;
    };
    let mut better: Vec<(&Seen, f64)> = Vec::new();
    let mut worse: Vec<(&Seen, f64)> = Vec::new();
    let (mut steady, mut new) = (0, 0);
    for each in seen {
        match each.change() {
            None => new += 1,
            Some((percent, _)) if percent.abs() < STEADY => steady += 1,
            Some((percent, true)) => better.push((each, percent)),
            Some((percent, false)) => worse.push((each, percent)),
        }
    }
    for list in [&mut better, &mut worse] {
        list.sort_by(|a, b| b.1.abs().total_cmp(&a.1.abs()));
    }
    ui.horizontal_wrapped(|ui| {
        ui.label(
            RichText::new(format!("Night of {}", date(newest as f64)))
                .size(18.0)
                .strong(),
        );
        ui.add_space(12.0);
        if new == seen.len() {
            ui.weak("The first night measured: nothing to compare with yet.");
            return;
        }
        let weak = ui.visuals().weak_text_color();
        let count = |ui: &mut Ui, n: usize, words: &str, colour: Color32| {
            ui.label(
                RichText::new(format!("{n} {words}"))
                    .size(16.0)
                    .color(colour),
            );
            ui.add_space(10.0);
        };
        count(ui, worse.len(), "regressed", RED);
        count(ui, better.len(), "improved", GREEN);
        count(ui, steady, "steady", weak);
        if new > 0 {
            count(ui, new, "without a night before", weak);
        }
    });
    if new == seen.len() {
        return;
    }
    ui.weak(format!(
        "Each value against the same benchmark's night before; under {STEADY:.0}% is steady."
    ));
    if better.is_empty() && worse.is_empty() {
        return;
    }
    ui.add_space(6.0);
    ui.horizontal_top(|ui| {
        for (title, list, colour) in [
            ("Regressed most", &worse, RED),
            ("Improved most", &better, GREEN),
        ] {
            if list.is_empty() {
                continue;
            }
            ui.vertical(|ui| {
                ui.label(RichText::new(title).strong().color(colour));
                egui::Grid::new(("movers", title))
                    .spacing(vec2(12.0, 2.0))
                    .show(ui, |ui| {
                        for (seen, percent) in list.iter().take(MOVERS) {
                            ui.label(&seen.row.label);
                            let mut what = seen.quantity.name.clone();
                            if let (Some(axis), Some(point)) = (seen.axis, seen.point) {
                                what.push_str(&format!(" at {} {axis}", plain(point)));
                            }
                            ui.weak(what).on_hover_text(seen.group);
                            ui.label(number(seen.trend.now));
                            ui.label(RichText::new(signed(*percent)).color(colour));
                            ui.end_row();
                        }
                    });
            });
            ui.add_space(32.0);
        }
    });
}

/// A table's every benchmark measured and quantity, each cell tinted by how it moved since
/// its night before, with a bar against its column. Gives back the quantity clicked.
fn matrix(ui: &mut Ui, group: &Group, point: Option<f64>) -> Option<String> {
    let rows: Vec<(&Row, Vec<Option<Trend>>)> = group
        .rows
        .iter()
        .map(|row| {
            let trends = group
                .quantities
                .iter()
                .map(|quantity| trend(row.benchmark, &quantity.name, point))
                .collect();
            (row, trends)
        })
        .filter(|(_, trends): &(_, Vec<Option<Trend>>)| trends.iter().any(Option::is_some))
        .collect();
    if rows.is_empty() {
        return None;
    }
    let columns: Vec<(f64, Option<f64>)> = group
        .quantities
        .iter()
        .enumerate()
        .map(|(column, quantity)| {
            let values = rows
                .iter()
                .filter_map(|(_, trends)| trends[column].as_ref().map(|trend| trend.now));
            let longest = values.clone().fold(0.0, f64::max);
            let best = values.reduce(|a, b| match quantity.higher_is_better {
                true => a.max(b),
                false => a.min(b),
            });
            (longest, best)
        })
        .collect();
    let mut clicked = None;
    ui.horizontal(|ui| {
        ui.label(RichText::new(&group.name).size(15.0).strong());
        if let (Some(axis), Some(point)) = (group.axis, point) {
            ui.weak(format!("at {} {axis}", plain(point)));
        }
    });
    egui::Grid::new(("overview", &group.name))
        .spacing(vec2(6.0, 4.0))
        .show(ui, |ui| {
            ui.label("");
            for quantity in group.quantities {
                ui.add_sized(
                    vec2(CELL.x, 0.0),
                    egui::Label::new(RichText::new(bare(quantity)).size(14.0)).wrap(),
                )
                .on_hover_text(match quantity.higher_is_better {
                    true => format!("{}, higher is better", quantity.unit),
                    false => format!("{}, lower is better", quantity.unit),
                });
            }
            ui.end_row();
            let mut family = None;
            for (row, trends) in &rows {
                let (this, variant) = split(&row.label);
                if this != family {
                    family = this;
                    if let Some(name) = this {
                        ui.label(RichText::new(name).strong());
                        ui.end_row();
                    }
                }
                let colour = colour(group, &row.label);
                ui.horizontal(|ui| {
                    swatch(ui, colour);
                    ui.label(variant);
                });
                for ((quantity, trend), (longest, best)) in
                    group.quantities.iter().zip(trends).zip(&columns)
                {
                    let shown = Shown {
                        colour,
                        longest: *longest,
                        best: trend.as_ref().is_some_and(|trend| Some(trend.now) == *best),
                    };
                    if tile(ui, trend.as_ref(), quantity, shown).clicked() {
                        clicked = Some(quantity.name.clone());
                    }
                }
                ui.end_row();
            }
        });
    clicked
}

/// How a cell sits in its table: its row's colour, its column's largest value, and whether
/// it is the column's best.
struct Shown {
    colour: Color32,
    longest: f64,
    best: bool,
}

/// A value, how it moved since its night before on a tint of that move, and a bar against
/// its column.
fn tile(ui: &mut Ui, trend: Option<&Trend>, quantity: &Quantity, shown: Shown) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(CELL, Sense::click());
    let painter = ui.painter_at(rect);
    let visuals = ui.visuals();
    let Some(trend) = trend else {
        painter.rect_filled(rect, 4.0, visuals.faint_bg_color);
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            "·",
            FontId::proportional(14.0),
            visuals.weak_text_color(),
        );
        return response;
    };
    let change = trend
        .before
        .and_then(|(before, _)| moved(trend.now, before, quantity));
    let ground = match change {
        Some((percent, better)) if percent.abs() >= STEADY => {
            let strength = (percent.abs() / LOUD).min(1.0) as f32;
            let tint = if better { GREEN } else { RED };
            tint.gamma_multiply(0.15 + 0.4 * strength)
        }
        _ => visuals.faint_bg_color,
    };
    painter.rect_filled(rect, 4.0, ground);
    if response.hovered() {
        painter.rect_stroke(
            rect,
            4.0,
            visuals.widgets.hovered.fg_stroke,
            egui::StrokeKind::Inside,
        );
    }
    let value = painter.text(
        rect.left_top() + vec2(8.0, 4.0),
        Align2::LEFT_TOP,
        number(trend.now),
        FontId::proportional(if shown.best { 16.5 } else { 15.0 }),
        match shown.best {
            true => visuals.strong_text_color(),
            false => visuals.text_color(),
        },
    );
    painter.text(
        value.right_bottom() + vec2(3.0, -2.0),
        Align2::LEFT_BOTTOM,
        &quantity.unit,
        FontId::proportional(11.0),
        visuals.weak_text_color(),
    );
    if let Some((percent, better)) = change {
        painter.text(
            rect.right_top() + vec2(-8.0, 6.0),
            Align2::RIGHT_TOP,
            signed(percent),
            FontId::proportional(11.0),
            shade(ui, percent, better),
        );
    }
    let track = egui::Rect::from_min_max(
        rect.left_bottom() + vec2(8.0, -9.0),
        rect.right_bottom() + vec2(-8.0, -5.0),
    );
    if shown.longest > 0.0 {
        let mut filled = track;
        filled.set_width(track.width() * (trend.now / shown.longest).clamp(0.0, 1.0) as f32);
        painter.rect_filled(filled, 2.0, shown.colour);
    }
    response.on_hover_ui(|ui| {
        ui.strong(format!("{} {}", number(trend.now), quantity.unit));
        if shown.best {
            ui.label(match quantity.higher_is_better {
                true => "The highest of its column",
                false => "The lowest of its column",
            });
        }
        match trend.before {
            Some((before, night)) => {
                ui.label(format!(
                    "{} {} on the night of {}",
                    number(before),
                    quantity.unit,
                    date(night as f64)
                ));
            }
            None => {
                ui.weak("Its first night");
            }
        }
        ui.weak(when(&trend.measurement.at));
        if let Some(machine) = &trend.measurement.machine {
            ui.weak(machine);
        }
        ui.weak(commits(trend.measurement));
        ui.weak("Click to compare this quantity across benchmarks and nights");
    })
}

fn compare(ui: &mut Ui, group: &Group, picked: &mut Picked) {
    let quantity = picked
        .quantity
        .as_ref()
        .and_then(|name| group.quantities.iter().find(|q| q.name == *name))
        .unwrap_or(&group.quantities[0]);
    let point = picked.point(group);
    let nights = nights(group);
    let against = picked
        .against
        .filter(|night| nights.contains(night))
        .or_else(|| nights.get(1).copied());

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
        ui.weak("compared with the night of");
        let named =
            |night: Option<i64>| night.map_or("none".to_string(), |night| date(night as f64));
        egui::ComboBox::from_id_salt(("against", &group.name))
            .selected_text(named(against))
            .show_ui(ui, |ui| {
                for &night in nights.iter().skip(1) {
                    if ui
                        .selectable_label(against == Some(night), named(Some(night)))
                        .clicked()
                    {
                        picked.against = Some(night);
                    }
                }
                if nights.len() < 2 {
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

    if nights.len() < 2 {
        bars(ui, group, quantity, point, against);
        ui.add_space(8.0);
        ui.weak("The chart over the nights starts with the second night.");
        return;
    }
    let width = ui.available_width();
    if width >= SIDE_BY_SIDE {
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(BARS);
                bars(ui, group, quantity, point, against);
            });
            ui.vertical(|ui| chart(ui, group, quantity, point));
        });
    } else {
        bars(ui, group, quantity, point, against);
        ui.add_space(10.0);
        chart(ui, group, quantity, point);
    }
}

/// A row each, under its family: the variant's colour and name, its latest value as a bar
/// from zero, and how it moved since `against`.
fn bars(ui: &mut Ui, group: &Group, quantity: &Quantity, point: Option<f64>, against: Option<i64>) {
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
        let colour = colour(group, &row.label);
        let width = ui.available_width().min(BARS);
        let (rect, response) = ui.allocate_exact_size(vec2(width, BAR_ROW), Sense::hover());
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
        painter.rect_filled(filled, 3.0, colour);
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
        let then = against.and_then(|night| on_night(row.benchmark, &quantity.name, point, night));
        let change = then.and_then(|then| moved(latest.value, then, quantity));
        if let Some((percent, better)) = change {
            painter.text(
                value.right_center() + vec2(8.0, 0.0),
                Align2::LEFT_CENTER,
                signed(percent),
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
            if let (Some(then), Some(night)) = (then, against) {
                ui.label(format!(
                    "{} {} on the night of {}",
                    number(then),
                    quantity.unit,
                    date(night as f64)
                ));
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

/// Each benchmark over the nights, a line each in its row's colour, dashed for the second
/// family and dotted for the third.
fn chart(ui: &mut Ui, group: &Group, quantity: &Quantity, point: Option<f64>) {
    let (families, _) = families_and_variants(group);
    let unit = quantity.unit.clone();
    Plot::new(("over time", &group.name))
        .height(340.0)
        .legend(
            Legend::default()
                .position(Corner::LeftTop)
                .follow_insertion_order(true),
        )
        .x_grid_spacer(uniform_grid_spacer(|_| [1.0, 7.0, 28.0]))
        .x_axis_formatter(|mark, _| match mark.value.fract() == 0.0 {
            true => date(mark.value),
            false => String::new(),
        })
        .y_axis_formatter(|mark, _| number(mark.value))
        .label_formatter(move |hover| match hover {
            HoverPosition::NearDataPoint {
                plot_name,
                position,
                ..
            } => Some(format!(
                "{plot_name}\n{} {unit}\nnight of {}",
                number(position.y),
                date(position.x.round())
            )),
            HoverPosition::Elsewhere { .. } => None,
        })
        .allow_scroll(false)
        .show(ui, |plot| {
            for row in &group.rows {
                let found = history(row.benchmark, &quantity.name, point);
                if found.is_empty() {
                    continue;
                }
                let (family, _) = split(&row.label);
                let style = match families.iter().position(|f| *f == family) {
                    Some(0) | None => LineStyle::Solid,
                    Some(1) => LineStyle::dashed_loose(),
                    _ => LineStyle::dotted_dense(),
                };
                let colour = colour(group, &row.label);
                let at: Vec<[f64; 2]> = found
                    .iter()
                    .map(|(night, value, _)| [*night as f64, *value])
                    .collect();
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
}

const GREEN: Color32 = Color32::from_rgb(70, 180, 100);
const RED: Color32 = Color32::from_rgb(225, 90, 80);

/// A row's colour: its variant's, shaded by its family.
fn colour(group: &Group, label: &str) -> Color32 {
    let (families, variants) = families_and_variants(group);
    let (family, variant) = split(label);
    let [r, g, b] = series_colour(
        variants.iter().position(|v| *v == variant).unwrap_or(0),
        families.iter().position(|f| *f == family).unwrap_or(0),
        families.len(),
    );
    Color32::from_rgb(r, g, b)
}

fn swatch(ui: &mut Ui, colour: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
    ui.painter().rect_filled(rect, 2.0, colour);
}

fn shade(ui: &Ui, percent: f64, better: bool) -> Color32 {
    match (percent.abs() < STEADY, better) {
        (true, _) => ui.visuals().weak_text_color(),
        (false, true) => GREEN,
        (false, false) => RED,
    }
}

/// A quantity's name without the unit it ends with, which its cells carry.
fn bare(quantity: &Quantity) -> &str {
    quantity
        .name
        .strip_suffix(&quantity.unit)
        .and_then(|name| name.strip_suffix(' '))
        .filter(|name| !name.is_empty())
        .unwrap_or(&quantity.name)
}

fn signed(percent: f64) -> String {
    format!(
        "{}{:.1}%",
        if percent >= 0.0 { "+" } else { "−" },
        percent.abs()
    )
}

fn plain(at: f64) -> String {
    match at.fract() == 0.0 {
        true => format!("{at:.0}"),
        false => at.to_string(),
    }
}
