//! The person's open pull requests, from `gh`. All of its shapes stay in this file.

use std::collections::HashMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use claudo_protocol::{Checks, PullRequest};
use serde_json::Value;

/// The open pull requests the person authored, with each one's CI on its last commit, a
/// page at a time: `gh search prs` cannot give the checks.
const OPEN: &str = "query($endCursor: String) { search(query: \
    \"is:pr is:open author:@me archived:false\", type: ISSUE, first: 100, \
    after: $endCursor) { pageInfo { hasNextPage endCursor } \
    nodes { ... on PullRequest { number title url isDraft repository { nameWithOwner } \
    commits(last: 1) { nodes { commit { statusCheckRollup { state } } } } } } } }";

/// How long GitHub is given to answer.
const LIMIT: Duration = Duration::from_secs(60);

/// Every open pull request the person authored, by its link. None when `gh` cannot say.
pub fn open_pull_requests() -> Option<HashMap<String, PullRequest>> {
    let mut child = Command::new("gh")
        .args(["api", "graphql", "--paginate", "--slurp", "-f"])
        .arg(format!("query={OPEN}"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let (done, read) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        let _ = done.send(bytes);
    });
    // A call that never returns would leave every pull request as it was for good.
    let Ok(bytes) = read.recv_timeout(LIMIT) else {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    };
    if !child.wait().ok()?.success() {
        return None;
    }
    let pages: Vec<Value> = serde_json::from_slice(&bytes).ok()?;
    Some(
        pages
            .iter()
            .filter_map(|page| page.pointer("/data/search/nodes")?.as_array())
            .flatten()
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
        checks: pr
            .pointer("/commits/nodes/0/commit/statusCheckRollup/state")
            .and_then(Value::as_str)
            .and_then(|state| match state {
                "SUCCESS" => Some(Checks::Passing),
                "FAILURE" | "ERROR" => Some(Checks::Failing),
                "PENDING" | "EXPECTED" => Some(Checks::Running),
                _ => None,
            }),
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
    fn a_pull_request_says_how_its_ci_stands() {
        let answer = serde_json::json!({"data": {"search": {"nodes": [
            {"number": 42, "title": "Fix", "url": "https://github.com/acme/widgets/pull/42",
             "isDraft": false, "repository": {"nameWithOwner": "acme/widgets"},
             "commits": {"nodes": [{"commit": {"statusCheckRollup": {"state": "FAILURE"}}}]}},
            {"number": 43, "title": "Docs", "url": "https://github.com/acme/widgets/pull/43",
             "isDraft": true, "repository": {"nameWithOwner": "acme/widgets"},
             "commits": {"nodes": [{"commit": {"statusCheckRollup": null}}]}},
        ]}}});
        let nodes = answer
            .pointer("/data/search/nodes")
            .unwrap()
            .as_array()
            .unwrap();
        let prs: Vec<_> = nodes.iter().filter_map(pull_request).collect();
        assert_eq!(prs[0].checks, Some(Checks::Failing));
        assert_eq!(prs[1].checks, None, "no checks");
        assert!(prs[1].draft);
    }

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
