//! The team's benchmarks as tables: those that measure the same quantities share one, a row
//! each, so a cell reads against its column.

use std::cmp::Ordering;

use claudo_protocol::{Benchmark, Measurement, Quantity};

pub struct Group<'a> {
    /// What its benchmarks measure, which their titles share: "step times".
    pub name: String,
    pub axis: Option<&'a str>,
    pub quantities: &'a [Quantity],
    /// Every point along the axis any of them was measured at.
    pub points: Vec<f64>,
    pub rows: Vec<Row<'a>>,
}

pub struct Row<'a> {
    pub label: String,
    pub benchmark: &'a Benchmark,
}

/// A quantity's latest value in a benchmark.
pub struct Cell<'a> {
    pub value: f64,
    /// The same quantity in the measurement before.
    pub before: Option<f64>,
    pub measurement: &'a Measurement,
}

/// The benchmarks that measure the same quantities along the same axis, in one table each,
/// their rows in the order of their slugs with numbers read as numbers.
pub fn groups(benchmarks: &[Benchmark]) -> Vec<Group<'_>> {
    let mut sorted: Vec<&Benchmark> = benchmarks.iter().collect();
    sorted.sort_by(|a, b| natural(&a.slug, &b.slug));
    let mut groups: Vec<(Vec<&Benchmark>, &Benchmark)> = Vec::new();
    for benchmark in sorted {
        let same = |other: &Benchmark| {
            other.axis == benchmark.axis
                && other.quantities.len() == benchmark.quantities.len()
                && other
                    .quantities
                    .iter()
                    .zip(&benchmark.quantities)
                    .all(|(a, b)| a.name == b.name)
        };
        match groups.iter_mut().find(|(_, first)| same(first)) {
            Some((members, _)) => members.push(benchmark),
            None => groups.push((vec![benchmark], benchmark)),
        }
    }
    groups
        .into_iter()
        .map(|(members, first)| group(members, first))
        .collect()
}

fn group<'a>(members: Vec<&'a Benchmark>, first: &'a Benchmark) -> Group<'a> {
    let split: Vec<Option<(&str, &str)>> = members
        .iter()
        .map(|benchmark| benchmark.title.rsplit_once(": "))
        .collect();
    let shared = split
        .first()
        .copied()
        .flatten()
        .map(|(_, name)| name)
        .filter(|name| {
            split
                .iter()
                .all(|parts| parts.is_some_and(|(_, other)| other == *name))
        });
    let rows = members
        .iter()
        .zip(&split)
        .map(|(benchmark, parts)| Row {
            label: match (shared, parts) {
                (Some(_), Some((label, _))) => label.to_string(),
                _ => benchmark.title.clone(),
            },
            benchmark,
        })
        .collect();
    let mut points: Vec<f64> = members
        .iter()
        .flat_map(|benchmark| &benchmark.measurements)
        .flat_map(|measurement| &measurement.arms)
        .flat_map(|arm| &arm.values)
        .filter_map(|value| value.point)
        .collect();
    points.sort_by(f64::total_cmp);
    points.dedup();
    Group {
        name: shared.unwrap_or(&first.title).to_string(),
        axis: first.axis.as_deref(),
        quantities: &first.quantities,
        points,
        rows,
    }
}

/// `quantity` at `point` in the newest measurement that has it, from its first arm, the
/// median of its repeats. A value measured at no point holds at every one.
pub fn cell<'a>(benchmark: &'a Benchmark, quantity: &str, point: Option<f64>) -> Option<Cell<'a>> {
    let mut found = benchmark
        .measurements
        .iter()
        .filter_map(|measurement| Some((value(measurement, quantity, point)?, measurement)));
    let (value, measurement) = found.next()?;
    Some(Cell {
        value,
        before: found.next().map(|(value, _)| value),
        measurement,
    })
}

fn value(measurement: &Measurement, quantity: &str, point: Option<f64>) -> Option<f64> {
    let mut values: Vec<f64> = measurement
        .arms
        .first()?
        .values
        .iter()
        .filter(|value| {
            value.quantity == quantity && (value.point.is_none() || value.point == point)
        })
        .map(|value| value.value)
        .collect();
    values.sort_by(f64::total_cmp);
    values.get(values.len() / 2).copied()
}

/// How far a cell moved since the measurement before, in percent, and whether for the better.
pub fn change(cell: &Cell, quantity: &Quantity) -> Option<(f64, bool)> {
    let before = cell.before.filter(|before| *before != 0.0)?;
    let percent = (cell.value - before) / before.abs() * 100.0;
    Some((percent, (percent > 0.0) == quantity.higher_is_better))
}

/// Each value against the best of its column, 1 for the best: a bar's length.
pub fn shares(values: &[Option<f64>], quantity: &Quantity) -> Vec<Option<f32>> {
    let present = values
        .iter()
        .flatten()
        .copied()
        .filter(|value| *value > 0.0);
    let best = match quantity.higher_is_better {
        true => present.max_by(f64::total_cmp),
        false => present.min_by(f64::total_cmp),
    };
    values
        .iter()
        .map(|value| {
            let (value, best) = (value.filter(|value| *value > 0.0)?, best?);
            Some(match quantity.higher_is_better {
                true => value / best,
                false => best / value,
            } as f32)
        })
        .collect()
}

/// A value with as many digits as a person reads: "2,664", "50.0", "0.26".
pub fn number(value: f64) -> String {
    let magnitude = value.abs();
    if magnitude >= 1000.0 {
        let whole = format!("{:.0}", magnitude);
        let mut grouped = String::new();
        for (i, digit) in whole.chars().enumerate() {
            if i > 0 && (whole.len() - i) % 3 == 0 {
                grouped.push(',');
            }
            grouped.push(digit);
        }
        format!("{}{grouped}", if value < 0.0 { "-" } else { "" })
    } else if magnitude >= 100.0 {
        format!("{value:.0}")
    } else if magnitude >= 10.0 {
        format!("{value:.1}")
    } else {
        format!("{value:.2}")
    }
}

/// When it was measured, to the minute: "2026-01-02 18:28 UTC".
pub fn when(at: &str) -> String {
    match (at.get(..10), at.get(11..16)) {
        (Some(day), Some(time)) => format!("{day} {time} UTC"),
        _ => at.to_string(),
    }
}

/// What it measured: each repository's commit, short.
pub fn commits(measurement: &Measurement) -> String {
    let Some(arm) = measurement.arms.first() else {
        return String::new();
    };
    arm.revisions
        .iter()
        .map(|(repo, commit)| {
            let repo = repo.rsplit('/').next().unwrap_or(repo);
            format!("{repo} {}", commit.get(..7).unwrap_or(commit))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The colours of a table's variants, told apart in light and dark themes.
pub const SERIES: [[u8; 3]; 8] = [
    [66, 133, 244],
    [245, 140, 50],
    [60, 175, 110],
    [225, 80, 140],
    [150, 110, 230],
    [40, 175, 190],
    [215, 175, 40],
    [205, 75, 65],
];

/// A row's label as what it runs and how: "model · 4 cards, TP" is the variant "4 cards, TP"
/// of the family "model". A label without the dot is a variant alone.
pub fn split(label: &str) -> (Option<&str>, &str) {
    match label.split_once(" · ") {
        Some((family, variant)) => (Some(family), variant),
        None => (None, label),
    }
}

/// The families and variants of a table's rows, each once, in the rows' order.
pub fn families_and_variants<'a>(group: &'a Group) -> (Vec<Option<&'a str>>, Vec<&'a str>) {
    let mut families = Vec::new();
    let mut variants = Vec::new();
    for row in &group.rows {
        let (family, variant) = split(&row.label);
        if !families.contains(&family) {
            families.push(family);
        }
        if !variants.contains(&variant) {
            variants.push(variant);
        }
    }
    (families, variants)
}

/// `quantity` at `point` in each measurement that has it, oldest first, at its time in days
/// since 1970.
pub fn history<'a>(
    benchmark: &'a Benchmark,
    quantity: &str,
    point: Option<f64>,
) -> Vec<(f64, f64, &'a Measurement)> {
    let mut found: Vec<(f64, f64, &Measurement)> = benchmark
        .measurements
        .iter()
        .filter_map(|measurement| {
            Some((
                days(&measurement.at)?,
                value(measurement, quantity, point)?,
                measurement,
            ))
        })
        .collect();
    found.sort_by(|a, b| a.0.total_cmp(&b.0));
    found
}

/// The days a table's benchmarks were measured on, newest first: "2026-01-02".
pub fn measured_days<'a>(group: &'a Group) -> Vec<&'a str> {
    let mut found: Vec<&str> = group
        .rows
        .iter()
        .flat_map(|row| &row.benchmark.measurements)
        .filter_map(|measurement| measurement.at.get(..10))
        .collect();
    found.sort_unstable_by(|a, b| b.cmp(a));
    found.dedup();
    found
}

/// `quantity` at `point` as the newest measurement on `day` had it.
pub fn on_day(benchmark: &Benchmark, quantity: &str, point: Option<f64>, day: &str) -> Option<f64> {
    benchmark
        .measurements
        .iter()
        .filter(|measurement| measurement.at.starts_with(day))
        .find_map(|measurement| value(measurement, quantity, point))
}

/// How far `now` moved from `then`, in percent, and whether for the better.
pub fn moved(now: f64, then: f64, quantity: &Quantity) -> Option<(f64, bool)> {
    (then != 0.0).then(|| {
        let percent = (now - then) / then.abs() * 100.0;
        (percent, (percent > 0.0) == quantity.higher_is_better)
    })
}

/// An RFC 3339 time in UTC as days since 1970.
pub fn days(at: &str) -> Option<f64> {
    let number = |range: std::ops::Range<usize>| at.get(range)?.parse::<i64>().ok();
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    // Howard Hinnant's days_from_civil.
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let of_era = year - era * 400;
    let of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let of_cycle = of_era * 365 + of_era / 4 - of_era / 100 + of_year;
    let whole = era * 146_097 + of_cycle - 719_468;
    Some(whole as f64 + (hour * 3600 + minute * 60 + second) as f64 / 86_400.0)
}

/// Days since 1970 as a date a person reads: "Jan 2".
pub fn date(days: f64) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    // Howard Hinnant's civil_from_days.
    let z = days.floor() as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let of_era = z - era * 146_097;
    let of_cycle = (of_era - of_era / 1460 + of_era / 36_524 - of_era / 146_096) / 365;
    let of_year = of_era - (365 * of_cycle + of_cycle / 4 - of_cycle / 100);
    let shifted = (5 * of_year + 2) / 153;
    let day = of_year - (153 * shifted + 2) / 5 + 1;
    let month = if shifted < 10 {
        shifted + 3
    } else {
        shifted - 9
    };
    format!("{} {day}", MONTHS[(month - 1) as usize])
}

/// Text order, except that runs of digits compare as numbers: 2 before 10.
fn natural(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a, b);
    loop {
        let (Some(x), Some(y)) = (a.chars().next(), b.chars().next()) else {
            return a.len().cmp(&b.len());
        };
        if x.is_ascii_digit() && y.is_ascii_digit() {
            let digits = |text: &str| {
                text.find(|c: char| !c.is_ascii_digit())
                    .unwrap_or(text.len())
            };
            let (m, n) = (digits(a), digits(b));
            let ordered = a[..m]
                .trim_start_matches('0')
                .len()
                .cmp(&b[..n].trim_start_matches('0').len())
                .then_with(|| {
                    a[..m]
                        .trim_start_matches('0')
                        .cmp(b[..n].trim_start_matches('0'))
                });
            if ordered != Ordering::Equal {
                return ordered;
            }
            (a, b) = (&a[m..], &b[n..]);
        } else {
            if x != y {
                return x.cmp(&y);
            }
            (a, b) = (&a[x.len_utf8()..], &b[y.len_utf8()..]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claudo_protocol::{Arm, Value};

    fn quantity(name: &str, higher: bool) -> Quantity {
        Quantity {
            name: name.into(),
            unit: String::new(),
            higher_is_better: higher,
        }
    }

    fn measured(at: &str, values: &[(&str, Option<f64>, f64)]) -> Measurement {
        Measurement {
            at: at.into(),
            machine: None,
            arms: vec![Arm {
                name: "main".into(),
                revisions: vec![("acme/orchard".into(), "0123456789".into())],
                values: values
                    .iter()
                    .map(|(quantity, point, value)| Value {
                        quantity: quantity.to_string(),
                        point: *point,
                        value: *value,
                    })
                    .collect(),
            }],
        }
    }

    fn benchmark(
        slug: &str,
        title: &str,
        axis: Option<&str>,
        measurements: Vec<Measurement>,
    ) -> Benchmark {
        Benchmark {
            slug: slug.into(),
            title: title.into(),
            description: String::new(),
            axis: axis.map(str::to_string),
            quantities: vec![quantity("pick ms", false), quantity("rate", true)],
            measurements,
        }
    }

    #[test]
    fn benchmarks_of_the_same_quantities_share_a_table_named_by_their_titles() {
        let benchmarks = [
            benchmark(
                "pick-10-baskets",
                "apples · 10 baskets: picking",
                None,
                vec![],
            ),
            benchmark(
                "pick-2-baskets",
                "apples · 2 baskets: picking",
                None,
                vec![],
            ),
            benchmark(
                "sell-2-baskets",
                "apples · 2 baskets: selling",
                Some("buyers"),
                vec![],
            ),
        ];
        let groups = groups(&benchmarks);
        let names: Vec<&str> = groups.iter().map(|group| group.name.as_str()).collect();
        assert_eq!(names, ["picking", "selling"]);
        let rows: Vec<&str> = groups[0]
            .rows
            .iter()
            .map(|row| row.label.as_str())
            .collect();
        assert_eq!(rows, ["apples · 2 baskets", "apples · 10 baskets"]);
    }

    #[test]
    fn a_cell_is_the_newest_value_and_says_how_it_moved() {
        let picking = benchmark(
            "pick",
            "picking",
            Some("buyers"),
            vec![
                measured(
                    "2026-01-03T00:00:00Z",
                    &[("rate", Some(4.0), 12.0), ("pick ms", None, 3.0)],
                ),
                measured("2026-01-02T00:00:00Z", &[("rate", Some(1.0), 99.0)]),
                measured("2026-01-01T00:00:00Z", &[("rate", Some(4.0), 10.0)]),
            ],
        );
        let rate = cell(&picking, "rate", Some(4.0)).expect("measured at 4");
        assert_eq!((rate.value, rate.before), (12.0, Some(10.0)));
        let (percent, better) = change(&rate, &picking.quantities[1]).unwrap();
        assert!((percent - 20.0).abs() < 1e-9 && better);
        assert_eq!(
            cell(&picking, "pick ms", Some(16.0)).map(|cell| cell.value),
            Some(3.0)
        );
        assert!(cell(&picking, "rate", Some(16.0)).is_none());
    }

    #[test]
    fn the_best_of_a_column_has_the_longest_bar() {
        let shares = shares(&[Some(10.0), None, Some(20.0)], &quantity("pick ms", false));
        assert_eq!(shares, [Some(1.0), None, Some(0.5)]);
    }

    #[test]
    fn numbers_read_as_a_person_writes_them() {
        assert_eq!(number(2663.6), "2,664");
        assert_eq!(number(1_234_567.0), "1,234,567");
        assert_eq!(number(50.0), "50.0");
        assert_eq!(number(0.26), "0.26");
        assert_eq!(when("2026-01-02T18:28:28Z"), "2026-01-02 18:28 UTC");
        assert_eq!(natural("b-2", "b-10"), Ordering::Less);
    }

    #[test]
    fn a_time_is_counted_in_days_and_read_back_as_a_date() {
        assert_eq!(days("1970-01-02T12:00:00Z"), Some(1.5));
        let leap = days("2024-02-29T00:00:00Z").unwrap();
        assert_eq!(date(leap), "Feb 29");
        assert_eq!(date(leap + 1.0), "Mar 1");
        assert_eq!(split("apples · 2 baskets"), (Some("apples"), "2 baskets"));
        assert_eq!(split("apples"), (None, "apples"));
    }

    #[test]
    fn a_day_compares_with_another() {
        let picking = benchmark(
            "pick",
            "picking",
            None,
            vec![
                measured("2026-01-03T05:00:00Z", &[("rate", None, 12.0)]),
                measured("2026-01-01T05:00:00Z", &[("rate", None, 10.0)]),
            ],
        );
        let group = &groups(std::slice::from_ref(&picking))[0];
        assert_eq!(measured_days(group), ["2026-01-03", "2026-01-01"]);
        assert_eq!(on_day(&picking, "rate", None, "2026-01-01"), Some(10.0));
        let history = history(&picking, "rate", None);
        assert!(history[0].0 < history[1].0 && history[1].1 == 12.0);
    }
}
