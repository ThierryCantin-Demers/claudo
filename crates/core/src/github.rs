//! The person's open pull requests, from `gh`. All of its shapes stay in this file.

use std::collections::HashMap;
use std::process::{Command, Stdio};

use claudo_protocol::PullRequest;
use serde_json::Value;

/// Every open pull request the person authored, by its link. None when `gh` cannot say.
pub fn open_pull_requests() -> Option<HashMap<String, PullRequest>> {
    let output = Command::new("gh")
        .args(["search", "prs", "--author", "@me", "--state", "open"])
        .args([
            "--json",
            "number,title,url,repository,isDraft",
            "--limit",
            "200",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let listed: Vec<Value> = serde_json::from_slice(&output.stdout).ok()?;
    Some(
        listed
            .iter()
            .filter_map(pull_request)
            .map(|pr| (pr.url.clone(), pr))
            .collect(),
    )
}

fn pull_request(pr: &Value) -> Option<PullRequest> {
    Some(PullRequest {
        url: pr.get("url")?.as_str()?.to_string(),
        number: pr.get("number")?.as_u64()?,
        title: pr.get("title")?.as_str()?.to_string(),
        repo: pr
            .pointer("/repository/nameWithOwner")?
            .as_str()?
            .to_string(),
        draft: pr.get("isDraft").and_then(Value::as_bool).unwrap_or(false),
    })
}

/// Every pull request link in `text`.
pub fn pull_links(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '(' | ')' | '<' | '>'))
        .map(|word| word.trim_end_matches([',', '.', ';', ':']))
        .filter(|word| {
            word.strip_prefix("https://github.com/")
                .and_then(|rest| rest.split_once("/pull/"))
                .is_some_and(|(repo, number)| {
                    repo.contains('/')
                        && !number.is_empty()
                        && number.bytes().all(|b| b.is_ascii_digit())
                })
        })
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pull_links_are_found_in_any_text() {
        let text = "Created (https://github.com/acme/widgets/pull/42), see also \
                    https://github.com/acme/widgets/pull/43. Not https://github.com/acme/widgets/pull/x";
        let links: Vec<_> = pull_links(text).collect();
        assert_eq!(
            links,
            [
                "https://github.com/acme/widgets/pull/42",
                "https://github.com/acme/widgets/pull/43"
            ]
        );
    }

    #[test]
    fn a_pull_request_is_read_from_ghs_json() {
        let pr = serde_json::json!({
            "number": 42, "title": "Fix the widget", "url": "https://github.com/acme/widgets/pull/42",
            "repository": {"name": "widgets", "nameWithOwner": "acme/widgets"}, "isDraft": true,
        });
        let pr = pull_request(&pr).unwrap();
        assert_eq!(
            (pr.number, pr.repo.as_str(), pr.draft),
            (42, "acme/widgets", true)
        );
    }
}
