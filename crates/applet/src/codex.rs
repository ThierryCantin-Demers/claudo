//! A Codex plan's usage, from `codex app-server`: JSON-RPC lines on its stdin and stdout,
//! answered in under a second. It exits once its stdin closes.

use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use claudo_protocol::QuotaWindow;
use serde_json::{Value, json};

use crate::asks::now;
use crate::usage::{Meter, Problem, Usage};

/// The ChatGPT app bundles Codex here without putting it on `PATH`.
const BUNDLED: &str = "/usr/lib/chatgpt/resources/codex";

pub fn usage() -> Result<Usage, Problem> {
    let mut child = Command::new(bin())
        .arg("app-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => Problem::NotInstalled,
            _ => Problem::Failed(error.to_string()),
        })?;
    let mut stdin = child.stdin.take().expect("stdin piped");
    let stdout = child.stdout.take().expect("stdout piped");
    let mut answers = BufReader::new(stdout)
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str::<Value>(&line).ok());
    let mut ask = |id: u64, method: &str, params: Value| -> Result<Value, Problem> {
        let failed = |error: io::Error| Problem::Failed(error.to_string());
        writeln!(
            stdin,
            "{}",
            json!({ "id": id, "method": method, "params": params })
        )
        .map_err(failed)?;
        if id == 1 {
            writeln!(stdin, "{}", json!({ "method": "initialized" })).map_err(failed)?;
        }
        answers
            .find(|answer| answer.get("id").and_then(Value::as_u64) == Some(id))
            .ok_or_else(|| Problem::Failed("Codex exited without answering".to_string()))
    };
    let client =
        json!({ "clientInfo": { "name": "claudo_applet", "version": env!("CARGO_PKG_VERSION") } });
    let answer = ask(1, "initialize", client).and_then(|_| {
        ask(
            2,
            "account/rateLimits/read",
            json!({ "excludeResetCreditDetails": true }),
        )
    });
    let usage = match answer {
        Ok(answer) => match answer.get("result").and_then(usage_answer) {
            Some(usage) => Ok(usage),
            None => {
                let account = ask(3, "account/read", json!({}));
                Err(match account {
                    Ok(account) if logged_out(&account) => Problem::LoggedOut,
                    _ => Problem::Failed(message(&answer)),
                })
            }
        },
        Err(problem) => Err(problem),
    };
    drop(stdin);
    let _ = child.wait();
    usage
}

/// `CLAUDO_CODEX_BIN`, then `codex` on `PATH`, then the ChatGPT app's.
fn bin() -> PathBuf {
    if let Some(bin) = std::env::var_os("CLAUDO_CODEX_BIN") {
        return bin.into();
    }
    let on_path = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(|dir| dir.join("codex"))
        .find(|bin| bin.is_file());
    on_path
        .or_else(|| Path::new(BUNDLED).is_file().then(|| BUNDLED.into()))
        .unwrap_or_else(|| "codex".into())
}

/// The plan's main bucket. Its windows say their own length, so a plan without a five
/// hour window just has fewer.
fn usage_answer(result: &Value) -> Option<Usage> {
    let limits = result.get("rateLimits")?;
    let meters = ["primary", "secondary"]
        .into_iter()
        .filter_map(|key| {
            let window = limits.get(key)?;
            Some(Meter {
                length: window.get("windowDurationMins")?.as_u64()? * 60,
                window: QuotaWindow {
                    used: window.get("usedPercent")?.as_f64()? as f32 / 100.0,
                    resets_at: window.get("resetsAt")?.as_u64()?,
                },
                model: None,
            })
        })
        .collect();
    Some(Usage {
        meters,
        measured_at: Some(now()),
    })
}

fn logged_out(account: &Value) -> bool {
    account
        .get("result")
        .is_some_and(|result| result.get("account").is_none_or(Value::is_null))
}

fn message(answer: &Value) -> String {
    answer
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or("Codex gave no usage")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plan_has_the_windows_its_server_reports() {
        let result = json!({ "rateLimits": {
            "primary": { "usedPercent": 11, "windowDurationMins": 10080, "resetsAt": 1796929772 },
            "secondary": null,
        }});
        let usage = usage_answer(&result).unwrap();
        assert_eq!(usage.meters.len(), 1);
        let week = &usage.meters[0];
        assert_eq!(
            (week.length, week.window.resets_at),
            (7 * 86_400, 1796929772)
        );
        assert!((week.window.used - 0.11).abs() < 1e-6);
    }

    #[test]
    fn an_answer_without_an_account_is_logged_out() {
        assert!(logged_out(
            &json!({ "result": { "account": null, "requiresOpenaiAuth": true } })
        ));
        assert!(!logged_out(
            &json!({ "result": { "account": { "type": "chatgpt" } } })
        ));
        assert!(!logged_out(&json!({ "error": { "message": "boom" } })));
    }
}
