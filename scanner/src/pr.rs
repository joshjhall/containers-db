//! Open a PR with new version files via `git` + `gh`. The shell-out is
//! intentionally thin; the interesting logic (branch name, commit
//! message, PR title/body shapes) is in pure functions for easy testing.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

use crate::activity::{ActivitySignals, Score};
use crate::dedup::{self, Decision, OpenPr};

/// Branch name for a new scan PR: `scan/<tool>/<YYYY-MM-DD>`. An open PR
/// on any earlier date is reused by [`deliver`] rather than duplicated.
pub fn branch_name(tool_id: &str, now: DateTime<Utc>) -> String {
    format!("scan/{tool_id}/{}", now.format("%Y-%m-%d"))
}

/// Commit message body matches the repo's conventional-commit style
/// (`feat(<scope>): ...`). Uses the tool id as the scope.
pub fn commit_message(tool_id: &str, new_versions: &[String]) -> String {
    let listed = new_versions.join(", ");
    format!("feat({tool_id}): add versions {listed} from upstream scan")
}

/// Title is the commit message minus the suffix — fits in GitHub's
/// 72-character soft limit even with multi-version scans.
pub fn pr_title(tool_id: &str, new_versions: &[String]) -> String {
    let listed = new_versions.join(", ");
    format!("feat({tool_id}): add upstream versions {listed}")
}

/// Markdown PR body with the new versions, recomputed activity block,
/// and a link to the upstream releases page when known.
pub fn pr_body(
    tool_id: &str,
    new_versions: &[String],
    source_repo: Option<&str>,
    score: Score,
    signals: &ActivitySignals,
    cadence_days: u32,
) -> String {
    let mut body = String::new();
    body.push_str("## Summary\n\n");
    body.push_str(&format!(
        "Automated scan added {} new version(s) for `{tool_id}` and refreshed the activity block.\n\n",
        new_versions.len()
    ));

    body.push_str("## New versions\n\n");
    for v in new_versions {
        body.push_str(&format!("- `{v}`\n"));
    }
    body.push('\n');

    body.push_str("## Activity (recomputed)\n\n");
    body.push_str(&format!(
        "- `score`: `{}`\n- `scan_cadence_days`: `{cadence_days}`\n",
        score_str(score),
    ));
    if let Some(n) = signals.releases_last_90d {
        body.push_str(&format!("- `releases_last_90d`: `{n}`\n"));
    }
    if let Some(t) = signals.last_release_at {
        body.push_str(&format!("- `last_release_at`: `{}`\n", t.to_rfc3339()));
    }
    if let Some(t) = signals.last_commit_at {
        body.push_str(&format!("- `last_commit_at`: `{}`\n", t.to_rfc3339()));
    }
    if let Some(n) = signals.open_advisories {
        body.push_str(&format!("- `open_advisories`: `{n}`\n"));
    }
    body.push('\n');

    if let Some(repo) = source_repo {
        body.push_str("## Upstream\n\n");
        body.push_str(&format!("- {repo}\n"));
        if let Some((owner, repo_name)) = crate::adapters::rust::parse_github_url(repo) {
            body.push_str(&format!(
                "- Releases: https://github.com/{owner}/{repo_name}/releases\n"
            ));
        }
        body.push('\n');
    }

    body.push_str("---\n_Opened by the containers-db catalog scanner._\n");
    body
}

fn score_str(score: Score) -> &'static str {
    match score {
        Score::VeryActive => "very-active",
        Score::Active => "active",
        Score::Maintained => "maintained",
        Score::Slow => "slow",
        Score::Stale => "stale",
        Score::Dormant => "dormant",
        Score::Abandoned => "abandoned",
    }
}

/// Create the branch, stage `tools/<tool_id>/`, commit, push, and open
/// the PR via `gh`. Returns the PR URL.
pub fn open_pr(
    repo_root: &Path,
    tool_id: &str,
    branch: &str,
    title: &str,
    body: &str,
    base: &str,
) -> Result<String> {
    git(repo_root, &["checkout", "-b", branch])?;
    git(repo_root, &["add", &format!("tools/{tool_id}")])?;
    git(
        repo_root,
        &["commit", "-m", &commit_message_from_title(title)],
    )?;
    git(repo_root, &["push", "-u", "origin", branch])?;
    let url = gh(
        repo_root,
        &[
            "pr", "create", "--base", base, "--head", branch, "--title", title, "--body", body,
        ],
    )?;
    Ok(url.trim().to_string())
}

/// How [`deliver`] handled a set of new versions.
#[derive(Debug)]
pub enum Delivery {
    Opened(String),
    Updated(String),
    /// An open PR (by number) already covered every version.
    Skipped(u64),
}

/// Open, update, or skip the PR for freshly generated `tools/<tool_id>/`
/// files, so a tool never has more than one open PR per `prefix` (see
/// [`crate::dedup`]). `branch` is used only when a new PR is opened.
pub fn deliver(
    repo_root: &Path,
    tool_id: &str,
    prefix: &str,
    branch: &str,
    new_versions: &[String],
    title: &str,
    body: &str,
) -> Result<Delivery> {
    let open = list_open_prs(repo_root)?;
    match dedup::decide(prefix, &open, new_versions) {
        Decision::Skip { pr, covered } => {
            println!(
                "scanner: {tool_id}: open scan PR #{pr} already covers {}, skipping",
                covered.join(", ")
            );
            // Drop the regenerated files so a later tool in the same run
            // doesn't commit them.
            let dir = format!("tools/{tool_id}");
            git(repo_root, &["checkout", "--", &dir])?;
            git(repo_root, &["clean", "-fd", "--", &dir])?;
            Ok(Delivery::Skipped(pr))
        }
        Decision::UpdateExisting(pr) => {
            update_pr(repo_root, tool_id, &pr, title, body).map(Delivery::Updated)
        }
        Decision::OpenNew => {
            open_pr(repo_root, tool_id, branch, title, body, "main").map(Delivery::Opened)
        }
    }
}

/// Upper bound on listed open PRs. Hitting it errors rather than risk
/// missing the tool's scan PR and opening a duplicate.
const PR_LIST_LIMIT: usize = 1000;

/// Open PRs in the repo, as [`OpenPr`]s.
fn list_open_prs(repo_root: &Path) -> Result<Vec<OpenPr>> {
    let limit = PR_LIST_LIMIT.to_string();
    let out = gh(
        repo_root,
        &[
            "pr",
            "list",
            "--state",
            "open",
            "--limit",
            &limit,
            "--json",
            dedup::GH_PR_FIELDS,
        ],
    )?;
    let prs = dedup::parse_open_prs(&out).map_err(anyhow::Error::msg)?;
    if prs.len() >= PR_LIST_LIMIT {
        anyhow::bail!(
            "`gh pr list` hit its {PR_LIST_LIMIT}-PR limit; refusing to dedup on a truncated list"
        );
    }
    Ok(prs)
}

/// Regenerate onto an existing PR's branch: reset it to the current HEAD
/// (main + regenerated files), commit, force-push, and refresh the title
/// and body. Returns the PR URL.
///
/// The lease is pinned to the head commit seen when the PRs were listed,
/// so a push that raced in since then rejects this one instead of being
/// overwritten. Commits already on the branch at listing time are
/// replaced: the branch is regenerated from `main`.
fn update_pr(
    repo_root: &Path,
    tool_id: &str,
    pr: &OpenPr,
    title: &str,
    body: &str,
) -> Result<String> {
    let number = pr.number.to_string();
    let lease = format!("--force-with-lease={}:{}", pr.head, pr.head_oid);
    git(repo_root, &["checkout", "-B", &pr.head])?;
    git(repo_root, &["add", &format!("tools/{tool_id}")])?;
    git(
        repo_root,
        &["commit", "-m", &commit_message_from_title(title)],
    )?;
    git(repo_root, &["push", &lease, "-u", "origin", &pr.head])?;
    gh(
        repo_root,
        &["pr", "edit", &number, "--title", title, "--body", body],
    )?;
    let url = gh(
        repo_root,
        &["pr", "view", &number, "--json", "url", "--jq", ".url"],
    )?;
    Ok(url.trim().to_string())
}

fn commit_message_from_title(title: &str) -> String {
    // The PR title already follows the conventional-commit convention,
    // so reuse it as the commit subject.
    title.to_string()
}

fn git(repo_root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .current_dir(repo_root)
        .args(args)
        .output()
        .with_context(|| format!("running `git {}`", args.join(" ")))?;
    if !output.status.success() {
        anyhow::bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn gh(repo_root: &Path, args: &[&str]) -> Result<String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(year: i32, month: u32, day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, 0, 0, 0).unwrap()
    }

    #[test]
    fn branch_name_includes_tool_and_date() {
        let b = branch_name("rust", at(2026, 5, 11));
        assert_eq!(b, "scan/rust/2026-05-11");
    }

    #[test]
    fn pr_title_and_commit_use_conventional_commit_shape() {
        let versions = vec!["1.95.1".to_string(), "1.96.0".to_string()];
        assert_eq!(
            pr_title("rust", &versions),
            "feat(rust): add upstream versions 1.95.1, 1.96.0"
        );
        assert_eq!(
            commit_message("rust", &versions),
            "feat(rust): add versions 1.95.1, 1.96.0 from upstream scan"
        );
    }

    #[test]
    fn pr_body_lists_new_versions_and_activity_block() {
        let signals = ActivitySignals {
            releases_last_90d: Some(3),
            last_release_at: Some(at(2026, 4, 25)),
            ..ActivitySignals::default()
        };
        let body = pr_body(
            "rust",
            &["1.96.0".to_string()],
            Some("https://github.com/rust-lang/rust"),
            Score::VeryActive,
            &signals,
            1,
        );
        assert!(body.contains("## Summary"));
        assert!(body.contains("- `1.96.0`"));
        assert!(body.contains("`score`: `very-active`"));
        assert!(body.contains("`scan_cadence_days`: `1`"));
        assert!(body.contains("`releases_last_90d`: `3`"));
        assert!(body.contains("https://github.com/rust-lang/rust/releases"));
    }

    #[test]
    fn pr_body_omits_upstream_section_when_source_unknown() {
        let body = pr_body(
            "foo",
            &["1.0.0".to_string()],
            None,
            Score::Maintained,
            &ActivitySignals::default(),
            30,
        );
        assert!(!body.contains("## Upstream"));
        assert!(body.contains("`score`: `maintained`"));
    }
}
