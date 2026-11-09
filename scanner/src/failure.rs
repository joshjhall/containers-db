//! On adapter or validation error, surface the problem as a GitHub
//! issue instead of producing a broken PR. Idempotent: if an open
//! issue with the same title already exists, the failure path
//! comments on that issue instead of creating a duplicate.

use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use reqwest::Method;
use serde_json::json;

use crate::github::GitHub;

/// Title of the tracking issue for a failed scan, matched exactly by the
/// idempotency lookup.
pub fn issue_title(tool_id: &str, now: DateTime<Utc>) -> String {
    format!("scanner: {tool_id} scan failed {}", now.format("%Y-%m-%d"))
}

/// Markdown body explaining what went wrong, with the error trimmed to
/// keep the issue scannable.
pub fn issue_body(tool_id: &str, error: &str, now: DateTime<Utc>) -> String {
    let truncated = trim_error(error, 4_000);
    let fence = fence_for(&truncated);
    format!(
        "## Failure\n\n\
         Scanner run for `{tool_id}` failed at {} UTC.\n\n\
         ## Error\n\n{fence}\n{truncated}\n{fence}\n\n\
         ---\n_Opened by the containers-db catalog scanner. Triggered by the\
         scheduled scan workflow; re-running the workflow with this tool will\
         retry. If the error is upstream (rate limit, API outage), no action\
         is needed — the next scheduled scan will re-attempt._\n",
        now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    )
}

/// The label every tracking issue carries; the idempotency lookup lists
/// only issues with it.
const FAILURE_LABEL: &str = "scanner-failure";

/// Most open tracking issues the lookup reads before giving up on finding
/// a match and filing a new one.
const OPEN_ISSUE_SCAN_LIMIT: usize = 500;

/// Open or comment on the failure-tracking issue. Returns the issue URL.
pub fn report_failure(
    repo_root: &Path,
    tool_id: &str,
    error: &str,
    now: DateTime<Utc>,
) -> Result<String> {
    report_failure_with(&GitHub::discover(repo_root)?, tool_id, error, now)
}

/// [`report_failure`] against a given client.
fn report_failure_with(
    gh: &GitHub,
    tool_id: &str,
    error: &str,
    now: DateTime<Utc>,
) -> Result<String> {
    let title = issue_title(tool_id, now);
    let body = issue_body(tool_id, error, now);

    if let Some((number, url)) = find_open_issue(gh, &title)? {
        let comment = json!({"body": body});
        gh.rest(
            Method::POST,
            &format!("issues/{number}/comments"),
            Some(&comment),
        )?;
        return Ok(url);
    }

    let issue = json!({"title": title, "body": body, "labels": [FAILURE_LABEL]});
    let created = gh.rest(Method::POST, "issues", Some(&issue))?;
    created["html_url"]
        .as_str()
        .map(str::to_string)
        .context("issue response has no html_url")
}

/// The number and URL of an open tracking issue titled exactly `title`.
/// Lists the label rather than searching: the search index lags, so a
/// second failure minutes after the first could miss it and file a
/// duplicate. The issues endpoint also returns PRs, which are skipped.
fn find_open_issue(gh: &GitHub, title: &str) -> Result<Option<(u64, String)>> {
    let suffix = format!("issues?state=open&labels={FAILURE_LABEL}&per_page=100");
    let issues = gh.list(&suffix, OPEN_ISSUE_SCAN_LIMIT)?;
    Ok(issues.iter().find_map(|i| {
        let is_issue = i.get("pull_request").is_none();
        (is_issue && i["title"].as_str() == Some(title))
            .then(|| Some((i["number"].as_u64()?, i["html_url"].as_str()?.to_string())))?
    }))
}

/// A code fence longer than any backtick run in `text`, so the error
/// can't close the block and inject markdown into the issue.
fn fence_for(text: &str) -> String {
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    "`".repeat(longest.max(2) + 1)
}

/// Truncate `error` to at most `max_chars`, appending an ellipsis if
/// cut. Avoids posting megabyte-scale stack traces on GitHub.
fn trim_error(error: &str, max_chars: usize) -> String {
    if error.chars().count() <= max_chars {
        return error.to_string();
    }
    let mut out: String = error.chars().take(max_chars).collect();
    out.push_str("\n…(truncated)");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(year: i32, month: u32, day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, 0, 0, 0).unwrap()
    }

    #[test]
    fn issue_title_includes_tool_and_date() {
        let t = issue_title("rust", at(2026, 5, 11));
        assert_eq!(t, "scanner: rust scan failed 2026-05-11");
    }

    #[test]
    fn issue_body_includes_error_and_timestamp() {
        let body = issue_body("rust", "boom: upstream 503", at(2026, 5, 11));
        assert!(body.contains("rust"));
        assert!(body.contains("boom: upstream 503"));
        assert!(body.contains("2026-05-11T00:00:00Z"));
    }

    #[test]
    fn issue_body_fences_the_error_past_its_own_backticks() {
        let error = "x ```\n## Pwned\n[click](https://evil.test) ```` y";
        let body = issue_body("rust", error, at(2026, 5, 11));
        assert!(
            body.contains(&format!("## Error\n\n`````\n{error}\n`````\n")),
            "got {body}"
        );
        assert_eq!(fence_for("no ticks"), "```");
    }

    #[test]
    fn trim_error_truncates_long_input() {
        let long = "x".repeat(10_000);
        let trimmed = trim_error(&long, 100);
        assert!(trimmed.len() < long.len());
        assert!(trimmed.ends_with("…(truncated)"));
    }

    #[test]
    fn trim_error_passes_short_input_through() {
        let short = "boom";
        assert_eq!(trim_error(short, 100), "boom");
    }

    use crate::github::testing::client_for;
    use wiremock::matchers::{body_partial_json, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const TITLE: &str = "scanner: rust scan failed 2026-05-11";

    async fn listing(server: &MockServer, rows: serde_json::Value) {
        Mock::given(method("GET"))
            .and(path("/repos/o/r/issues"))
            .and(query_param("labels", FAILURE_LABEL))
            .and(query_param("state", "open"))
            .respond_with(ResponseTemplate::new(200).set_body_json(rows))
            .expect(1)
            .mount(server)
            .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_open_issue_with_the_title_gets_a_comment() {
        let server = MockServer::start().await;
        listing(
            &server,
            json!([
                {"number": 3, "title": "scanner: go scan failed 2026-05-11", "html_url": "u3"},
                {"number": 9, "title": TITLE, "html_url": "u9"},
            ]),
        )
        .await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/issues/9/comments"))
            .and(body_partial_json(json!({})))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/issues"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        let url = report_failure_with(&client_for(&server), "rust", "boom", at(2026, 5, 11));
        assert_eq!(url.unwrap(), "u9");
    }

    /// A same-titled PR is not the tracking issue: filing still happens.
    #[tokio::test(flavor = "multi_thread")]
    async fn no_matching_issue_files_a_labelled_one() {
        let server = MockServer::start().await;
        listing(
            &server,
            json!([{"number": 4, "title": TITLE, "html_url": "pr4", "pull_request": {}}]),
        )
        .await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/issues"))
            .and(body_partial_json(
                json!({"title": TITLE, "labels": [FAILURE_LABEL]}),
            ))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"html_url": "new"})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/issues/4/comments"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        let url = report_failure_with(&client_for(&server), "rust", "boom", at(2026, 5, 11));
        assert_eq!(url.unwrap(), "new");
    }
}
