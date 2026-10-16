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
            discard_generated(repo_root, tool_id)?;
            Ok(Delivery::Skipped(pr))
        }
        Decision::UpdateExisting(pr) => on_branch_then_return(repo_root, || {
            update_pr(repo_root, tool_id, &pr, title, body).map(Delivery::Updated)
        }),
        Decision::OpenNew => on_branch_then_return(repo_root, || {
            open_pr(repo_root, tool_id, branch, title, body, "main").map(Delivery::Opened)
        }),
    }
}

/// Where a scan run started: the checked-out branch (or `None` when
/// detached) and its commit. Every tool must start from here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Base {
    branch: Option<String>,
    sha: String,
}

impl Base {
    /// Record the current checkout as the run's base.
    pub fn capture(repo_root: &Path) -> Result<Self> {
        let name = git(repo_root, &["rev-parse", "--abbrev-ref", "HEAD"])?;
        let sha = git(repo_root, &["rev-parse", "HEAD"])?;
        let branch = match name.trim() {
            "HEAD" => None,
            b => Some(b.to_string()),
        };
        Ok(Base {
            branch,
            sha: sha.trim().to_string(),
        })
    }

    /// Fail unless the checkout is still exactly at the base. A tool that
    /// started anywhere else would cut its PR branch from a previous tool's
    /// commit, so a `due` run stops here rather than continue (#403).
    pub fn verify(&self, repo_root: &Path) -> Result<()> {
        let now = Base::capture(repo_root)?;
        if now != *self {
            anyhow::bail!(
                "checkout left at {} ({}), not the run's base {} ({}); \
                 refusing to scan further tools from the wrong commit",
                now.branch.as_deref().unwrap_or("detached HEAD"),
                now.sha,
                self.branch.as_deref().unwrap_or("detached HEAD"),
                self.sha,
            );
        }
        Ok(())
    }
}

/// The base a `due` run checks before every tool. `None` for a dry run:
/// it never commits, so there is no base to drift from and no git
/// checkout is required.
#[derive(Debug)]
pub struct RunBase(Option<Base>);

impl RunBase {
    pub fn capture(repo_root: &Path, dry_run: bool) -> Result<Self> {
        Ok(RunBase(if dry_run {
            None
        } else {
            Some(Base::capture(repo_root)?)
        }))
    }

    /// Abort the run before `tool_id` if a previous tool left the checkout
    /// off the base (e.g. its return checkout failed), naming the failures
    /// collected so far.
    pub fn guard(&self, repo_root: &Path, tool_id: &str, failed: &[String]) -> Result<()> {
        let Some(base) = &self.0 else {
            return Ok(());
        };
        base.verify(repo_root).with_context(|| {
            format!(
                "aborted before `{tool_id}`; earlier failures: [{}]",
                failed.join(", ")
            )
        })
    }
}

/// Run `f` (which commits onto a PR branch), then switch back to the
/// branch the run started on. Without this, the next tool in a `due` run
/// would cut its branch from this tool's commit and carry it into its PR.
///
/// A detached start is returned to by commit (`checkout --detach <sha>`):
/// `--abbrev-ref` alone would yield the literal `HEAD`, a no-op checkout.
fn on_branch_then_return<T>(repo_root: &Path, f: impl FnOnce() -> Result<T>) -> Result<T> {
    let name = git(repo_root, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    let start: Vec<String> = match name.trim() {
        "HEAD" => {
            let sha = git(repo_root, &["rev-parse", "HEAD"])?;
            vec!["checkout".into(), "--detach".into(), sha.trim().into()]
        }
        branch => vec!["checkout".into(), branch.into()],
    };
    let result = f();
    let args: Vec<&str> = start.iter().map(String::as_str).collect();
    let back = git(repo_root, &args)
        .with_context(|| format!("returning to the start (`git {}`)", args.join(" ")));
    match (result, back) {
        (Ok(value), Ok(_)) => Ok(value),
        (Ok(_), Err(back_err)) => Err(back_err),
        (Err(e), back) => {
            // A delivery failure is the more useful error; still surface a
            // failed return, since the next tool would branch from here.
            if let Err(back_err) = back {
                eprintln!("scanner: {back_err:#}");
            }
            Err(e)
        }
    }
}

/// Drop regenerated `tools/<tool_id>/` changes after a skip, so a later
/// tool in the same run doesn't commit them. Untracked files go first; the
/// restore runs only when the directory has tracked files — for a tool new
/// on the base, `git checkout -- <dir>` would fail on an unknown pathspec.
fn discard_generated(repo_root: &Path, tool_id: &str) -> Result<()> {
    let dir = format!("tools/{tool_id}");
    git(repo_root, &["clean", "-fd", "--", &dir])?;
    if !git(repo_root, &["ls-files", "--", &dir])?.trim().is_empty() {
        git(repo_root, &["checkout", "--", &dir])?;
    }
    Ok(())
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
    dedup::check_not_truncated(prs.len(), PR_LIST_LIMIT).map_err(anyhow::Error::msg)?;
    Ok(prs)
}

/// Regenerate onto an existing PR's branch: reset it to the current HEAD
/// (the run's base + regenerated files — [`deliver`] returns to the base
/// after each tool), commit, force-push, and refresh the title
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
    let lease = dedup::lease_arg(pr);
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

    /// Temp repo on `main` with one commit. Hermetic: explicit identity,
    /// no global config needed. Panics if an inherited `GIT_DIR` (e.g. from
    /// a pre-push hook) would point `git` at a different repo.
    fn temp_repo() -> tempfile::TempDir {
        assert!(
            std::env::var_os("GIT_DIR").is_none() && std::env::var_os("GIT_WORK_TREE").is_none(),
            "unset GIT_DIR/GIT_WORK_TREE: the temp-repo tests would act on another repo"
        );
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "-q", "-b", "main"]).unwrap();
        git(p, &["config", "user.name", "t"]).unwrap();
        git(p, &["config", "user.email", "t@example.com"]).unwrap();
        std::fs::write(p.join("a"), "a").unwrap();
        git(p, &["add", "a"]).unwrap();
        git(p, &["commit", "-q", "-m", "init"]).unwrap();
        dir
    }

    fn current_branch(p: &Path) -> String {
        git(p, &["rev-parse", "--abbrev-ref", "HEAD"])
            .unwrap()
            .trim()
            .to_string()
    }

    #[test]
    fn delivery_returns_to_the_starting_branch() {
        let dir = temp_repo();
        let p = dir.path();
        on_branch_then_return(p, || {
            git(p, &["checkout", "-q", "-b", "scan/a/2026-10-16"])?;
            std::fs::write(p.join("b"), "b").unwrap();
            git(p, &["add", "b"])?;
            git(p, &["commit", "-q", "-m", "tool a"])
        })
        .unwrap();
        assert_eq!(current_branch(p), "main");
        assert!(
            !p.join("b").exists(),
            "tool a's commit must not leak onto main"
        );
    }

    #[test]
    fn delivery_error_wins_and_branch_is_restored() {
        let dir = temp_repo();
        let p = dir.path();
        let err = on_branch_then_return(p, || -> Result<()> {
            git(p, &["checkout", "-q", "-b", "scan/a/x"])?;
            anyhow::bail!("push rejected")
        })
        .unwrap_err();
        assert!(err.to_string().contains("push rejected"));
        assert_eq!(current_branch(p), "main");
    }

    #[test]
    fn base_verify_passes_at_the_base() {
        let dir = temp_repo();
        let p = dir.path();
        let base = Base::capture(p).unwrap();
        base.verify(p).unwrap();
    }

    #[test]
    fn base_verify_stops_a_run_left_on_a_pr_branch() {
        // #403: tool A's return to base failed, leaving HEAD on its PR
        // branch; the run must stop before tool B branches from there.
        let dir = temp_repo();
        let p = dir.path();
        let base = Base::capture(p).unwrap();
        git(p, &["checkout", "-q", "-b", "scan/a/x"]).unwrap();
        std::fs::write(p.join("b"), "b").unwrap();
        git(p, &["add", "b"]).unwrap();
        git(p, &["commit", "-q", "-m", "tool a"]).unwrap();
        let err = base.verify(p).unwrap_err();
        assert!(err.to_string().contains("scan/a/x"), "{err:#}");
    }

    #[test]
    fn base_verify_catches_same_branch_new_commit() {
        let dir = temp_repo();
        let p = dir.path();
        let base = Base::capture(p).unwrap();
        std::fs::write(p.join("b"), "b").unwrap();
        git(p, &["add", "b"]).unwrap();
        git(p, &["commit", "-q", "-m", "stray"]).unwrap();
        assert!(base.verify(p).is_err());
    }

    #[test]
    fn run_guard_aborts_with_tool_and_earlier_failures() {
        let dir = temp_repo();
        let p = dir.path();
        let base = RunBase::capture(p, false).unwrap();
        base.guard(p, "node", &[]).unwrap();
        git(p, &["checkout", "-q", "-b", "scan/rust/x"]).unwrap();
        let err = base.guard(p, "python", &["go".to_string()]).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("aborted before `python`"), "{msg}");
        assert!(msg.contains("earlier failures: [go]"), "{msg}");
        assert!(msg.contains("scan/rust/x"), "{msg}");
        assert!(msg.contains("not the run's base main"), "{msg}");
    }

    #[test]
    fn dry_run_needs_no_checkout_and_never_aborts() {
        // A dry run outside any git repo: capture must not touch git.
        let dir = tempfile::tempdir().unwrap();
        let base = RunBase::capture(dir.path(), true).unwrap();
        base.guard(dir.path(), "rust", &[]).unwrap();
    }

    #[test]
    fn real_run_outside_a_checkout_fails_to_capture() {
        let dir = tempfile::tempdir().unwrap();
        assert!(RunBase::capture(dir.path(), false).is_err());
    }

    #[test]
    fn base_verify_detached_start() {
        let dir = temp_repo();
        let p = dir.path();
        git(p, &["checkout", "-q", "--detach"]).unwrap();
        let base = Base::capture(p).unwrap();
        base.verify(p).unwrap();
        git(p, &["checkout", "-q", "main"]).unwrap();
        assert!(base.verify(p).is_err(), "on a branch, not detached");
    }

    #[test]
    fn detached_start_is_returned_to_by_commit() {
        let dir = temp_repo();
        let p = dir.path();
        let base = git(p, &["rev-parse", "HEAD"]).unwrap();
        git(p, &["checkout", "-q", "--detach"]).unwrap();
        on_branch_then_return(p, || {
            git(p, &["checkout", "-q", "-b", "scan/a/x"])?;
            std::fs::write(p.join("b"), "b").unwrap();
            git(p, &["add", "b"])?;
            git(p, &["commit", "-q", "-m", "tool a"])
        })
        .unwrap();
        assert_eq!(current_branch(p), "HEAD", "still detached");
        assert_eq!(git(p, &["rev-parse", "HEAD"]).unwrap(), base);
    }

    #[test]
    fn failed_return_is_an_error() {
        let dir = temp_repo();
        let p = dir.path();
        let err = on_branch_then_return(p, || {
            git(p, &["checkout", "-q", "-b", "scan/a/x"])?;
            git(p, &["branch", "-q", "-D", "main"])
        })
        .unwrap_err();
        assert!(format!("{err:#}").contains("returning to the start"));
    }

    #[test]
    fn skip_cleanup_handles_a_tool_new_on_the_base() {
        let dir = temp_repo();
        let p = dir.path();
        std::fs::create_dir_all(p.join("tools/newtool/versions")).unwrap();
        std::fs::write(p.join("tools/newtool/versions/1.0.0.json"), "{}").unwrap();
        discard_generated(p, "newtool").unwrap();
        assert!(!p.join("tools/newtool").exists());
    }

    #[test]
    fn skip_cleanup_restores_tracked_and_drops_untracked() {
        let dir = temp_repo();
        let p = dir.path();
        std::fs::create_dir_all(p.join("tools/rust/versions")).unwrap();
        std::fs::write(p.join("tools/rust/index.json"), "old").unwrap();
        git(p, &["add", "tools"]).unwrap();
        git(p, &["commit", "-q", "-m", "rust"]).unwrap();
        std::fs::write(p.join("tools/rust/index.json"), "new").unwrap();
        std::fs::write(p.join("tools/rust/versions/2.0.0.json"), "{}").unwrap();
        discard_generated(p, "rust").unwrap();
        assert_eq!(
            std::fs::read_to_string(p.join("tools/rust/index.json")).unwrap(),
            "old"
        );
        assert!(!p.join("tools/rust/versions/2.0.0.json").exists());
    }

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
