//! On adapter or validation error, surface the problem as a GitHub
//! issue instead of producing a broken PR. Idempotent: if an open
//! issue with the same title already exists, the failure path
//! comments on that issue instead of creating a duplicate.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

/// Title of the tracking issue for a failed scan. Same shape used for
/// idempotency lookups via `gh issue list --search`.
pub fn issue_title(tool_id: &str, now: DateTime<Utc>) -> String {
    format!("scanner: {tool_id} scan failed {}", now.format("%Y-%m-%d"))
}

/// Markdown body explaining what went wrong, with the error trimmed to
/// keep the issue scannable.
pub fn issue_body(tool_id: &str, error: &str, now: DateTime<Utc>) -> String {
    let truncated = trim_error(error, 4_000);
    format!(
        "## Failure\n\n\
         Scanner run for `{tool_id}` failed at {} UTC.\n\n\
         ## Error\n\n```\n{truncated}\n```\n\n\
         ---\n_Opened by the containers-db catalog scanner. Triggered by the\
         scheduled scan workflow; re-running the workflow with this tool will\
         retry. If the error is upstream (rate limit, API outage), no action\
         is needed — the next scheduled scan will re-attempt._\n",
        now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    )
}

/// Open or comment on the failure-tracking issue. Returns the issue URL.
pub fn report_failure(
    repo_root: &Path,
    tool_id: &str,
    error: &str,
    now: DateTime<Utc>,
) -> Result<String> {
    let title = issue_title(tool_id, now);
    let body = issue_body(tool_id, error, now);

    if let Some(existing) = find_open_issue(repo_root, &title)? {
        comment_on_issue(repo_root, &existing, &body)?;
        return Ok(existing);
    }

    create_issue(repo_root, &title, &body)
}

fn find_open_issue(repo_root: &Path, title: &str) -> Result<Option<String>> {
    // `gh issue list --search "in:title \"<title>\"" --state open --json url,title`
    let output = gh_with_stdout(
        repo_root,
        &[
            "issue",
            "list",
            "--state",
            "open",
            "--search",
            &format!("in:title \"{title}\""),
            "--json",
            "url,title",
            "--limit",
            "5",
        ],
    )?;
    let list: Vec<serde_json::Value> =
        serde_json::from_str(&output).with_context(|| "parsing `gh issue list` JSON output")?;
    for item in list {
        if item.get("title").and_then(serde_json::Value::as_str) == Some(title)
            && let Some(url) = item.get("url").and_then(serde_json::Value::as_str)
        {
            return Ok(Some(url.to_string()));
        }
    }
    Ok(None)
}

fn create_issue(repo_root: &Path, title: &str, body: &str) -> Result<String> {
    let out = gh_with_stdout(
        repo_root,
        &[
            "issue",
            "create",
            "--title",
            title,
            "--body",
            body,
            "--label",
            "scanner-failure",
        ],
    )?;
    Ok(out.trim().to_string())
}

fn comment_on_issue(repo_root: &Path, issue_url: &str, body: &str) -> Result<()> {
    gh_with_stdout(repo_root, &["issue", "comment", issue_url, "--body", body])?;
    Ok(())
}

fn gh_with_stdout(repo_root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("gh")
        .current_dir(repo_root)
        .args(args)
        .output()
        .with_context(|| format!("running `gh {}`", args.join(" ")))?;
    if !output.status.success() {
        anyhow::bail!(
            "gh {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
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
}
