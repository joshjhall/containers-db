//! Open a PR with new version files via `git` and the GitHub API
//! ([`crate::github`]). The I/O is intentionally thin; the interesting logic (branch name, commit
//! message, PR title/body shapes) is in pure functions for easy testing.
//!
//! The scan commit is built locally but never pushed: [`crate::publish`]
//! replays it as a GitHub-signed commit, since `main` requires signatures
//! and the scanner holds no key (#526).

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

use crate::activity::{ActivitySignals, Score};
use crate::dedup::{self, Decision, OpenPr, RegenReason};
use crate::github::GitHub;
use crate::publish::{self, ApiRemote, Remote, Target};

mod github;

use github::{commits_verified, list_open_prs, mark_candidate_verified};

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

/// Create the branch, stage `tools/<tool_id>/`, commit, publish it as a
/// signed commit, and open the PR. Returns the PR URL.
pub fn open_pr(
    repo_root: &Path,
    gh: &GitHub,
    tool_id: &str,
    branch: &str,
    title: &str,
    body: &str,
    base: &str,
) -> Result<String> {
    let remote = ApiRemote::discover(gh)?;
    open_pr_with(repo_root, gh, &remote, tool_id, branch, title, body, base)
}

/// [`open_pr`] publishing through a given `remote`, so a test can drive
/// the PR write against a bare repo and a mock API.
#[allow(clippy::too_many_arguments)]
fn open_pr_with(
    repo_root: &Path,
    gh: &GitHub,
    remote: &dyn Remote,
    tool_id: &str,
    branch: &str,
    title: &str,
    body: &str,
    base: &str,
) -> Result<String> {
    git(repo_root, &["checkout", "-b", branch])?;
    git(repo_root, &["add", &format!("tools/{tool_id}")])?;
    let message = commit_message_from_title(title);
    git(repo_root, &["commit", "-m", &message])?;
    publish::publish_head(repo_root, remote, Target::New(branch), &message)?;
    let pr = serde_json::json!({"base": base, "head": branch, "title": title, "body": body});
    html_url(&gh.rest(reqwest::Method::POST, "pulls", Some(&pr))?)
}

fn html_url(pr: &serde_json::Value) -> Result<String> {
    pr["html_url"]
        .as_str()
        .map(str::to_string)
        .context("PR response has no html_url")
}

/// How [`deliver`] handled a set of new versions.
#[derive(Debug)]
pub enum Delivery {
    Opened(String),
    Updated(String),
    /// An open PR (by number) already covered every version.
    Skipped(u64),
}

/// What a plan hands [`deliver`]: its arguments besides the checkout.
#[derive(Debug, Clone, PartialEq)]
pub struct PrRequest {
    pub tool_id: String,
    pub prefix: String,
    pub branch: String,
    pub new_versions: Vec<String>,
    pub title: String,
    pub body: String,
}

/// Open, update, or skip the PR for freshly generated `tools/<tool_id>/`
/// files, so a tool never has more than one open PR per `prefix` (see
/// [`crate::dedup`]). `branch` is used only when a new PR is opened.
///
/// Errors on an empty `new_versions` before touching GitHub or git: callers
/// return `NoChange` first, and [`dedup::decide`] would otherwise read an
/// empty list as vacuously covered by any open PR (#407).
pub fn deliver(repo_root: &Path, req: &PrRequest) -> Result<Delivery> {
    // Exhaustive: a new or renamed field fails to compile here (#433).
    let PrRequest {
        tool_id,
        prefix,
        branch,
        new_versions,
        title,
        body,
    } = req;
    anyhow::ensure!(
        !new_versions.is_empty(),
        "scanner: {tool_id}: deliver called with no new versions; the caller must return NoChange first"
    );
    let gh = GitHub::discover(repo_root)?;
    deliver_via(
        repo_root,
        &gh,
        tool_id,
        prefix,
        new_versions,
        |pr| update_pr(repo_root, &gh, tool_id, pr, title, body),
        || open_pr(repo_root, &gh, tool_id, branch, title, body, "main"),
    )
}

/// The decision half of [`deliver`] against a given client: list the open
/// PRs, look up the candidate's commit signatures, then [`dedup::decide`].
/// The PR writes are injected so a test can drive the lookup-before-decide
/// wiring against a mock API (#533); without the lookup every covering PR
/// would read unsigned and be regenerated every run.
fn deliver_via(
    repo_root: &Path,
    gh: &GitHub,
    tool_id: &str,
    prefix: &str,
    new_versions: &[String],
    update: impl FnOnce(&OpenPr) -> Result<String>,
    open: impl FnOnce() -> Result<String>,
) -> Result<Delivery> {
    let mut open_prs = list_open_prs(gh)?;
    mark_candidate_verified(
        tool_id,
        prefix,
        &mut open_prs,
        new_versions,
        |pr| commits_verified(gh, pr),
        std::thread::sleep,
    );
    deliver_with(
        repo_root,
        tool_id,
        &dedup::decide(prefix, &open_prs, new_versions),
        |line| println!("{line}"),
        update,
        open,
    )
}

/// Act on a dedup [`Decision`]: the git-state half of [`deliver`], with
/// the PR-side actions injected so every arm is testable without GitHub.
///
/// - Skip: discard the regenerated files; HEAD stays on the base.
/// - Update / OpenNew: run the action on its PR branch, then return to the
///   start. On failure the regenerated files are discarded too, so a later
///   tool in the same run never starts from a dirty tree.
///
/// `log` receives the regen line naming the [`RegenReason`]; it is stdout
/// in production, injected so a test can see which line was emitted.
fn deliver_with(
    repo_root: &Path,
    tool_id: &str,
    decision: &Decision,
    mut log: impl FnMut(&str),
    update: impl FnOnce(&OpenPr) -> Result<String>,
    open: impl FnOnce() -> Result<String>,
) -> Result<Delivery> {
    let delivered = match decision {
        Decision::Skip { pr, covered } => {
            println!(
                "scanner: {tool_id}: open scan PR #{pr} already covers {}, skipping",
                covered.join(", ")
            );
            discard_generated(repo_root, tool_id)?;
            return Ok(Delivery::Skipped(*pr));
        }
        Decision::UpdateExisting { pr, reason } => {
            log(&regen_message(tool_id, pr.number, *reason));
            on_branch_then_return(repo_root, || update(pr).map(Delivery::Updated))
        }
        Decision::OpenNew => on_branch_then_return(repo_root, || open().map(Delivery::Opened)),
    };
    if delivered.is_err()
        && let Err(e) = discard_generated(repo_root, tool_id)
    {
        eprintln!("scanner: {tool_id}: discarding generated files after a failed delivery: {e:#}");
    }
    delivered
}

/// The log line for regenerating PR `n`, naming why (#529).
fn regen_message(tool_id: &str, n: u64, reason: RegenReason) -> String {
    match reason {
        RegenReason::MissingVersions => {
            format!("scanner: {tool_id}: open scan PR #{n} is missing versions; regenerating")
        }
        RegenReason::UnsignedCommits => format!(
            "scanner: {tool_id}: open scan PR #{n} covers the versions but has unsigned commits; regenerating (its branch is replaced)"
        ),
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

/// The checked-out commit.
pub fn head_sha(repo_root: &Path) -> Result<String> {
    Ok(git(repo_root, &["rev-parse", "HEAD"])?.trim().to_string())
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
    on_branch_then_return_to(repo_root, &mut std::io::stderr(), f)
}

/// [`on_branch_then_return`] with the failed-return report (written when
/// the delivery error wins) sent to `report` instead of stderr.
fn on_branch_then_return_to<T>(
    repo_root: &Path,
    report: &mut dyn Write,
    f: impl FnOnce() -> Result<T>,
) -> Result<T> {
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
            // failed return, since the next tool would branch from here. A
            // failed write must not mask the delivery error, so it is dropped.
            if let Err(back_err) = back {
                let _ = writeln!(report, "scanner: {back_err:#}");
            }
            Err(e)
        }
    }
}

/// Drop regenerated `tools/<tool_id>/` changes after a skip, so a later
/// tool in the same run doesn't commit them. Untracked files go first; the
/// restore runs only when the directory has tracked files — for a tool new
/// on the base, `git checkout -- <dir>` would fail on an unknown pathspec.
pub(crate) fn discard_generated(repo_root: &Path, tool_id: &str) -> Result<()> {
    let dir = format!("tools/{tool_id}");
    git(repo_root, &["clean", "-fd", "--", &dir])?;
    if !git(repo_root, &["ls-files", "--", &dir])?.trim().is_empty() {
        git(repo_root, &["checkout", "--", &dir])?;
    }
    Ok(())
}

/// Regenerate onto an existing PR's branch: reset it to the current HEAD
/// (the run's base + regenerated files — [`deliver`] returns to the base
/// after each tool), commit, replace the branch with the signed commit,
/// and refresh the title and body. Returns the PR URL.
///
/// The lease is pinned to the head commit seen when the PRs were listed,
/// so a push that raced in since then rejects this one instead of being
/// overwritten. Commits already on the branch at listing time are
/// replaced: the branch is regenerated from `main`.
fn update_pr(
    repo_root: &Path,
    gh: &GitHub,
    tool_id: &str,
    pr: &OpenPr,
    title: &str,
    body: &str,
) -> Result<String> {
    let remote = ApiRemote::discover(gh)?;
    update_pr_with(repo_root, gh, &remote, tool_id, pr, title, body)
}

/// [`update_pr`] publishing through a given `remote`, so a test can drive
/// the PR edit against a bare repo and a mock API.
fn update_pr_with(
    repo_root: &Path,
    gh: &GitHub,
    remote: &dyn Remote,
    tool_id: &str,
    pr: &OpenPr,
    title: &str,
    body: &str,
) -> Result<String> {
    push_regenerated(repo_root, remote, tool_id, pr, title)?;
    let edit = serde_json::json!({"title": title, "body": body});
    let path = format!("pulls/{}", pr.number);
    html_url(&gh.rest(reqwest::Method::PATCH, &path, Some(&edit))?)
}

/// The git half of [`update_pr`]: reset `pr.head` to the current HEAD,
/// commit the regenerated `tools/<tool_id>/`, and replace the remote
/// branch with the signed commit, leased on `pr.head_oid`.
fn push_regenerated(
    repo_root: &Path,
    remote: &dyn Remote,
    tool_id: &str,
    pr: &OpenPr,
    title: &str,
) -> Result<()> {
    git(repo_root, &["checkout", "-B", &pr.head])?;
    git(repo_root, &["add", &format!("tools/{tool_id}")])?;
    let message = commit_message_from_title(title);
    git(repo_root, &["commit", "-m", &message])?;
    let target = Target::Existing {
        branch: &pr.head,
        lease: &pr.head_oid,
    };
    publish::publish_head(repo_root, remote, target, &message)?;
    Ok(())
}

fn commit_message_from_title(title: &str) -> String {
    // The PR title already follows the conventional-commit convention,
    // so reuse it as the commit subject.
    title.to_string()
}

fn git(repo_root: &Path, args: &[&str]) -> Result<String> {
    let output = crate::git_command()
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

#[cfg(test)]
pub(super) mod tests;
