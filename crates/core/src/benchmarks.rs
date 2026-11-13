//! The team's benchmarks, from a command the person gives, for the pages to show beside the
//! machines they ran on. All of its output's shapes stay in this file.

use std::process::{Command, Stdio};
use std::time::Duration;

use claudo_protocol::{Arm, Benchmark, Measurement, Quantity, Value};
use serde_json::Value as Json;

use crate::room::Feed;

/// Measurements land nightly or by hand, so a look every few minutes misses nothing.
const POLL: Duration = Duration::from_secs(300);

/// Reads `projects`, each `project` or `project/slug-prefix`, into `feed` for as long as the
/// process runs, from what `command`, a shell command with `{project}` in it, prints.
pub fn spawn(feed: Feed, command: String, projects: Vec<String>) {
    std::thread::spawn(move || {
        loop {
            let mut found = Vec::new();
            for wanted in &projects {
                let (project, prefix) = wanted.split_once('/').unwrap_or((wanted, ""));
                match look(&command, project) {
                    Ok(text) => found.extend(
                        benchmarks(&text)
                            .into_iter()
                            .filter(|benchmark| benchmark.slug.starts_with(prefix)),
                    ),
                    Err(error) => eprintln!("claudo: cannot read the benchmarks: {error}"),
                }
            }
            feed.benchmarks(found);
            std::thread::sleep(POLL);
        }
    });
}

fn look(command: &str, project: &str) -> Result<String, String> {
    let output = Command::new("sh")
        .args(["-c", &command.replace("{project}", project)])
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("cannot run the benchmarks command: {error}"))?;
    if !output.status.success() {
        let said = String::from_utf8_lossy(&output.stderr);
        return Err(said
            .lines()
            .next()
            .unwrap_or("the benchmarks command failed")
            .to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn benchmarks(text: &str) -> Vec<Benchmark> {
    let Ok(json) = serde_json::from_str::<Json>(text) else {
        return Vec::new();
    };
    json["data"]["benchmarks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(benchmark)
        .collect()
}

fn benchmark(entry: &Json) -> Option<Benchmark> {
    let declared = &entry["benchmark"];
    let mut measurements: Vec<Measurement> = entry["measurements"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(measurement)
        .collect();
    // RFC 3339 in UTC sorts as text.
    measurements.sort_by(|a, b| b.at.cmp(&a.at));
    Some(Benchmark {
        slug: declared["slug"].as_str()?.to_string(),
        title: text(&declared["title"]),
        description: text(&declared["description"]),
        axis: declared["axis"].as_str().map(str::to_string),
        quantities: declared["quantities"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|quantity| {
                Some(Quantity {
                    name: quantity["name"].as_str()?.to_string(),
                    unit: text(&quantity["unit"]),
                    higher_is_better: quantity["better_when"] == "higher",
                })
            })
            .collect(),
        measurements,
    })
}

fn measurement(entry: &Json) -> Option<Measurement> {
    Some(Measurement {
        at: entry["measured_at"].as_str()?.to_string(),
        machine: entry["conditions"]["machine"].as_str().map(str::to_string),
        arms: entry["arms"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|arm| Arm {
                name: text(&arm["name"]),
                revisions: arm["revisions"]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .filter_map(|(repo, commit)| Some((repo.clone(), commit.as_str()?.to_string())))
                    .collect(),
                values: arm["values"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|value| {
                        Some(Value {
                            quantity: value["quantity"].as_str()?.to_string(),
                            point: value["point"].as_f64(),
                            value: value["value"].as_f64()?,
                        })
                    })
                    .collect(),
            })
            .collect(),
    })
}

fn text(json: &Json) -> String {
    json.as_str().unwrap_or_default().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_listed_benchmark_keeps_its_quantities_and_newest_measurement_first() {
        let text = r#"{"ok":true,"data":{"project_slug":"orchard","benchmarks":[{
            "benchmark":{"slug":"pick-steps","title":"apples · 2 baskets: steps","description":"d",
              "axis":null,"quantities":[{"name":"pick ms","unit":"ms","better_when":"lower"},
              {"name":"rate","unit":"apples/s","better_when":"higher"}]},
            "measurements":[
              {"measured_at":"2026-01-01T00:00:00Z","conditions":{"machine":"barn"},
               "arms":[{"name":"main","revisions":{"acme/orchard":"aaa"},
                 "values":[{"quantity":"pick ms","point":null,"rep":1,"value":5.0}]}]},
              {"measured_at":"2026-01-02T00:00:00Z","conditions":{},
               "arms":[{"name":"main","revisions":{"acme/orchard":"bbb"},
                 "values":[{"quantity":"rate","point":4.0,"rep":1,"value":9.5}]}]}]}]}}"#;
        let [benchmark] = &benchmarks(text)[..] else {
            panic!("one benchmark");
        };
        assert_eq!(benchmark.quantities.len(), 2);
        assert!(!benchmark.quantities[0].higher_is_better);
        assert!(benchmark.quantities[1].higher_is_better);
        let [newest, oldest] = &benchmark.measurements[..] else {
            panic!("two measurements");
        };
        assert_eq!(newest.at, "2026-01-02T00:00:00Z");
        assert_eq!(newest.machine, None);
        assert_eq!(newest.arms[0].values[0].point, Some(4.0));
        assert_eq!(oldest.machine.as_deref(), Some("barn"));
        assert_eq!(
            oldest.arms[0].revisions,
            [("acme/orchard".into(), "aaa".into())]
        );
    }
}
