//! Open a PR with new version files via `git` + `gh`. The shell-out is
//! intentionally thin; the interesting logic (branch name, commit
//! message, PR title/body shapes) is in pure functions for easy testing.
//!
//! The scan commit is built locally but never pushed: [`crate::publish`]
//! replays it as a GitHub-signed commit, since `main` requires signatures
//! and the scanner holds no key (#526).

use std::io::Write;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

use crate::activity::{ActivitySignals, Score};
use crate::dedup::{self, Decision, OpenPr, RegenReason};
use crate::publish::{self, GhRemote, Remote, Target};

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
/// signed commit, and open the PR via `gh`. Returns the PR URL.
pub fn open_pr(
    repo_root: &Path,
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
/// Errors on an empty `new_versions` before touching `gh` or git: callers
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
    let mut open = list_open_prs(repo_root)?;
    mark_candidate_verified(tool_id, prefix, &mut open, |pr| {
        commits_verified(repo_root, pr)
    });
    deliver_with(
        repo_root,
        tool_id,
        &dedup::decide(prefix, &open, new_versions),
        |line| println!("{line}"),
        |pr| {
            update_pr(
                repo_root,
                &GhRemote::discover(repo_root)?,
                tool_id,
                pr,
                title,
                body,
            )
        },
        || {
            let remote = GhRemote::discover(repo_root)?;
            open_pr(repo_root, &remote, tool_id, branch, title, body, "main")
        },
    )
}

/// Act on a dedup [`Decision`]: the git-state half of [`deliver`], with
/// the PR-side actions injected so every arm is testable without `gh`.
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

/// Look up whether every commit on the PR [`dedup::decide`] will act on
/// is GitHub-verified, and record it on that row. Only the candidate needs
/// it, so this is one API call per tool rather than one per open PR.
///
/// A failed lookup leaves the row unverified: a redundant regen costs one
/// signed commit, a wrong skip strands the release (#529). `lookup` is
/// [`commits_verified`] in production, injected so the fail-closed path is
/// testable without `gh`.
fn mark_candidate_verified(
    tool_id: &str,
    prefix: &str,
    open: &mut [OpenPr],
    lookup: impl FnOnce(&OpenPr) -> Result<bool>,
) {
    let Some(number) = dedup::candidate(prefix, open).map(|pr| pr.number) else {
        return;
    };
    let Some(pr) = open.iter_mut().find(|pr| pr.number == number) else {
        return;
    };
    pr.commits_verified = match lookup(pr) {
        Ok(v) => v,
        Err(e) => {
            eprintln!(
                "scanner: {tool_id}: could not check the commit signatures of PR #{number}; treating it as unsigned: {e:#}"
            );
            false
        }
    };
}

/// The most commits `pulls/{n}/commits` returns, however it is paginated.
/// A listing that reaches it may be truncated, so it is refused.
const PR_COMMITS_LIMIT: usize = 250;

/// Whether every commit on `pr` carries a GitHub-verified signature.
fn commits_verified(repo_root: &Path, pr: &OpenPr) -> Result<bool> {
    let out = gh(
        repo_root,
        &[
            "api",
            "--paginate",
            &format!("repos/{{owner}}/{{repo}}/pulls/{}/commits", pr.number),
            "--jq",
            r#".[] | "\(.sha) \(.commit.verification.verified)""#,
        ],
    )?;
    parse_verified(&out, &pr.head_oid)
}

/// Parse `<sha> <verified>` lines, one per commit in PR order, across
/// every page. Verified only when every line says `true`. An error, so the
/// caller fails toward a regen, when:
/// - the listing is empty, or a line is not `<sha> true|false`;
/// - it reaches [`PR_COMMITS_LIMIT`] — unsigned commits past the cap
///   would go unseen and read as a wrong skip;
/// - its last commit is not `head_oid` — the branch moved since the PRs
///   were listed, so this answer describes a head `decide` never saw.
fn parse_verified(out: &str, head_oid: &str) -> Result<bool> {
    let mut all = true;
    let mut last = None;
    let mut count = 0;
    for line in out.lines() {
        let (sha, verified) = line
            .trim()
            .split_once(' ')
            .with_context(|| format!("unexpected commit verification line {line:?}"))?;
        match verified {
            "true" => {}
            "false" => all = false,
            other => anyhow::bail!("unexpected commit verification value {other:?}"),
        }
        last = Some(sha);
        count += 1;
    }
    let last = last.context("no commits listed for the PR")?;
    anyhow::ensure!(
        count < PR_COMMITS_LIMIT,
        "PR lists {count} commits, at the {PR_COMMITS_LIMIT}-commit API cap; refusing to trust a possibly truncated listing"
    );
    anyhow::ensure!(
        last == head_oid,
        "PR head moved since listing ({head_oid} listed, {last} now)"
    );
    Ok(all)
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
    remote: &dyn Remote,
    tool_id: &str,
    pr: &OpenPr,
    title: &str,
    body: &str,
) -> Result<String> {
    let number = pr.number.to_string();
    push_regenerated(repo_root, remote, tool_id, pr, title)?;
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
    use crate::publish::testing::BareRemote;
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
    fn head_sha_is_the_checked_out_commit() {
        let dir = temp_repo();
        let sha = head_sha(dir.path()).unwrap();
        assert_eq!(sha.len(), 40);
        assert!(
            sha.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')),
            "got {sha:?}"
        );
        assert_eq!(sha, git(dir.path(), &["rev-parse", "HEAD"]).unwrap().trim());
    }

    #[test]
    fn head_sha_errors_outside_a_repo() {
        let dir = tempfile::tempdir().unwrap();
        let err = head_sha(dir.path()).unwrap_err();
        assert!(
            format!("{err:#}").starts_with("git rev-parse HEAD failed:"),
            "got {err:#}"
        );
    }

    /// `<sha> <verified>` lines for commits `c0..`, the last being `head`.
    fn commits(verified: &[bool]) -> String {
        verified
            .iter()
            .enumerate()
            .map(|(i, v)| format!("c{i} {v}\n"))
            .collect()
    }

    #[test]
    fn parse_verified_requires_every_commit_verified() {
        assert!(parse_verified(&commits(&[true]), "c0").unwrap());
        assert!(parse_verified(&commits(&[true, true, true]), "c2").unwrap());
        assert!(!parse_verified(&commits(&[false]), "c0").unwrap());
        // A signed fixup on top of an unsigned commit is still unmergeable.
        assert!(!parse_verified(&commits(&[false, true]), "c1").unwrap());
        assert!(!parse_verified(&commits(&[true, false, true]), "c2").unwrap());
        for bad in [
            "",
            "\n",
            "c0 null\n",
            "c0 True\n",
            "true\n",
            "c0 true\nc1 yes\n",
        ] {
            assert!(
                parse_verified(bad, "c0").is_err(),
                "{bad:?} must be an error"
            );
        }
    }

    /// The commits endpoint stops at 250 however it is paginated: a full
    /// listing may hide unsigned commits past the cap, so it is refused.
    #[test]
    fn parse_verified_refuses_a_listing_at_the_api_cap() {
        let under = commits(&vec![true; PR_COMMITS_LIMIT - 1]);
        let head = format!("c{}", PR_COMMITS_LIMIT - 2);
        assert!(parse_verified(&under, &head).unwrap());
        let at = commits(&vec![true; PR_COMMITS_LIMIT]);
        let head = format!("c{}", PR_COMMITS_LIMIT - 1);
        let err = parse_verified(&at, &head).unwrap_err();
        assert!(format!("{err:#}").contains("commit API cap"), "got {err:#}");
    }

    /// A push between listing and lookup: the answer describes a head
    /// `decide` never saw, so it is refused rather than trusted.
    #[test]
    fn parse_verified_refuses_a_moved_head() {
        let err = parse_verified(&commits(&[true, true]), "c0").unwrap_err();
        assert!(format!("{err:#}").contains("head moved"), "got {err:#}");
    }

    #[test]
    fn regen_message_names_the_reason() {
        let missing = regen_message("node", 519, RegenReason::MissingVersions);
        let unsigned = regen_message("node", 519, RegenReason::UnsignedCommits);
        assert_eq!(
            missing,
            "scanner: node: open scan PR #519 is missing versions; regenerating"
        );
        assert_eq!(
            unsigned,
            "scanner: node: open scan PR #519 covers the versions but has unsigned commits; regenerating (its branch is replaced)"
        );
    }

    /// Two matching scan PRs plus a fork; only the oldest non-fork row is
    /// the candidate, so only it is looked up and only it is marked.
    fn listing() -> Vec<OpenPr> {
        let mut fork = open_pr_fixture(5, "scan/rust/evil");
        fork.cross_repo = true;
        vec![
            fork,
            open_pr_fixture(402, "scan/rust/2026-10-17"),
            open_pr_fixture(398, "scan/rust/2026-10-16"),
        ]
    }

    #[test]
    fn mark_candidate_verified_marks_only_the_candidate() {
        let mut open = listing();
        let mut asked = None;
        mark_candidate_verified("rust", "scan/rust/", &mut open, |pr| {
            asked = Some(pr.number);
            Ok(true)
        });
        assert_eq!(asked, Some(398));
        let marked: Vec<u64> = open
            .iter()
            .filter(|p| p.commits_verified)
            .map(|p| p.number)
            .collect();
        assert_eq!(marked, vec![398]);
    }

    /// #529: a lookup that errors must leave the candidate unverified, so
    /// `decide` regenerates it rather than skipping it.
    #[test]
    fn mark_candidate_verified_fails_closed_on_lookup_error() {
        let mut open = listing();
        for pr in &mut open {
            pr.commits_verified = true;
        }
        mark_candidate_verified("rust", "scan/rust/", &mut open, |_| {
            anyhow::bail!("gh api: 502")
        });
        let candidate = open.iter().find(|p| p.number == 398).unwrap();
        assert!(!candidate.commits_verified);
        assert!(
            open.iter()
                .filter(|p| p.number != 398)
                .all(|p| p.commits_verified),
            "non-candidate rows must be left untouched"
        );
    }

    #[test]
    fn mark_candidate_verified_skips_lookup_without_a_candidate() {
        let mut open = vec![open_pr_fixture(10, "scan/node/2026-10-16")];
        mark_candidate_verified("rust", "scan/rust/", &mut open, |_| {
            panic!("no candidate: lookup must not run")
        });
        assert!(!open[0].commits_verified);
    }

    #[test]
    fn deliver_rejects_empty_new_versions() {
        // Not a repo and no `gh` stub: reaching `list_open_prs` would fail
        // with a different message, so the text pins the guard itself.
        let dir = tempfile::tempdir().unwrap();
        let req = PrRequest {
            tool_id: "rust".into(),
            prefix: "scan/rust/".into(),
            branch: "b".into(),
            new_versions: vec![],
            title: "t".into(),
            body: "b".into(),
        };
        let err = deliver(dir.path(), &req).unwrap_err();
        assert!(
            format!("{err:#}").contains("deliver called with no new versions"),
            "got: {err:#}"
        );
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
        let mut report = Vec::new();
        let err = on_branch_then_return_to(p, &mut report, || -> Result<()> {
            git(p, &["checkout", "-q", "-b", "scan/a/x"])?;
            anyhow::bail!("push rejected")
        })
        .unwrap_err();
        assert!(err.to_string().contains("push rejected"));
        assert_eq!(current_branch(p), "main");
        assert!(report.is_empty(), "a successful return reports nothing");
    }

    #[test]
    fn delivery_error_wins_over_failed_return() {
        // Both fail: the delivery error is returned, and the failed return
        // is still reported, since the checkout is left on the PR branch.
        let dir = temp_repo();
        let p = dir.path();
        let mut report = Vec::new();
        let err = on_branch_then_return_to(p, &mut report, || -> Result<()> {
            git(p, &["checkout", "-q", "-b", "scan/a/x"])?;
            git(p, &["branch", "-q", "-D", "main"])?;
            anyhow::bail!("push rejected")
        })
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("push rejected"), "{msg}");
        assert!(!msg.contains("returning to the start"), "{msg}");
        assert_eq!(
            current_branch(p),
            "scan/a/x",
            "the return must have failed, or this covers the (Err, Ok) arm"
        );
        let report = String::from_utf8(report).unwrap();
        assert!(
            report.starts_with("scanner: returning to the start (`git checkout main`)"),
            "{report}"
        );
    }

    /// A clone of a bare `origin` whose open scan PR branch `scan/rust/old`
    /// holds an earlier scan commit, checked out on `main` with freshly
    /// regenerated `tools/rust/` files. Returns (clone, origin, the PR as
    /// listed — `head_oid` is the branch's real head).
    fn clone_with_open_scan_pr() -> (tempfile::TempDir, tempfile::TempDir, OpenPr) {
        // temp_repo() first: its GIT_DIR guard must run before any git call,
        // or a leaked GIT_DIR would `init --bare` the real repo.
        let dir = temp_repo();
        let p = dir.path();
        let origin = tempfile::tempdir().unwrap();
        git(origin.path(), &["init", "-q", "--bare", "-b", "main"]).unwrap();
        std::fs::create_dir_all(p.join("tools/rust")).unwrap();
        std::fs::write(p.join("tools/rust/index.json"), "old").unwrap();
        git(p, &["add", "tools"]).unwrap();
        git(p, &["commit", "-q", "-m", "rust"]).unwrap();
        let url = origin.path().to_str().unwrap();
        git(p, &["remote", "add", "origin", url]).unwrap();
        git(p, &["push", "-q", "origin", "main"]).unwrap();
        git(p, &["checkout", "-q", "-b", "scan/rust/old"]).unwrap();
        std::fs::write(p.join("tools/rust/index.json"), "earlier scan").unwrap();
        git(p, &["commit", "-q", "-am", "earlier scan"]).unwrap();
        git(p, &["push", "-q", "origin", "scan/rust/old"]).unwrap();
        let head_oid = git(p, &["rev-parse", "HEAD"]).unwrap().trim().to_string();
        git(p, &["checkout", "-q", "main"]).unwrap();
        std::fs::create_dir_all(p.join("tools/rust/versions")).unwrap();
        std::fs::write(p.join("tools/rust/index.json"), "new").unwrap();
        std::fs::write(p.join("tools/rust/versions/2.0.0.json"), "{}").unwrap();
        let pr = OpenPr {
            head_oid,
            ..open_pr_fixture(398, "scan/rust/old")
        };
        (dir, origin, pr)
    }

    fn rev(p: &Path, rev: &str) -> String {
        git(p, &["rev-parse", rev]).unwrap().trim().to_string()
    }

    #[test]
    fn push_regenerated_replaces_the_branch_when_the_lease_holds() {
        let (dir, origin, pr) = clone_with_open_scan_pr();
        let p = dir.path();
        let base = rev(p, "main");
        // Outside the tool's directory: must not ride along in its scan PR.
        std::fs::write(p.join("stray"), "stray").unwrap();
        let remote = BareRemote::new(origin.path());
        push_regenerated(
            p,
            &remote,
            "rust",
            &pr,
            "feat(rust): add upstream versions 2.0.0",
        )
        .unwrap();
        let o = origin.path();
        assert_eq!(
            rev(o, "scan/rust/old^"),
            base,
            "regenerated from the base: the earlier scan commit is replaced, not built on"
        );
        assert_eq!(
            git(o, &["show", "scan/rust/old:tools/rust/index.json"]).unwrap(),
            "new"
        );
        assert_eq!(
            git(o, &["show", "scan/rust/old:tools/rust/versions/2.0.0.json"]).unwrap(),
            "{}"
        );
        assert_eq!(
            git(o, &["log", "-1", "--format=%s", "scan/rust/old"])
                .unwrap()
                .trim(),
            "feat(rust): add upstream versions 2.0.0",
            "the commit subject is the PR title"
        );
        assert!(
            git(o, &["cat-file", "-e", "scan/rust/old:stray"]).is_err(),
            "only tools/rust/ is staged"
        );
    }

    #[test]
    fn push_regenerated_is_rejected_when_the_remote_moved() {
        let (dir, origin, pr) = clone_with_open_scan_pr();
        // Someone pushes to the PR branch after it was listed.
        let racer = tempfile::tempdir().unwrap();
        let r = racer.path();
        git(r, &["clone", "-q", origin.path().to_str().unwrap(), "."]).unwrap();
        git(r, &["config", "user.name", "t"]).unwrap();
        git(r, &["config", "user.email", "t@example.com"]).unwrap();
        git(r, &["checkout", "-q", "scan/rust/old"]).unwrap();
        std::fs::write(r.join("tools/rust/index.json"), "racing edit").unwrap();
        git(r, &["commit", "-q", "-am", "racing edit"]).unwrap();
        git(r, &["push", "-q", "origin", "scan/rust/old"]).unwrap();
        let raced = rev(r, "HEAD");
        // The clone's tracking ref now sees the race, so a name-only
        // lease would pass: only the oid pinned at listing time rejects.
        git(dir.path(), &["fetch", "-q", "origin"]).unwrap();

        let remote = BareRemote::new(origin.path());
        let err = push_regenerated(dir.path(), &remote, "rust", &pr, "feat(rust): x").unwrap_err();
        // The lease rejection names the race, not any publish failure.
        assert!(
            format!("{err:#}").contains("lease rejected: scan/rust/old moved"),
            "{err:#}"
        );
        assert_eq!(
            rev(origin.path(), "scan/rust/old"),
            raced,
            "the racing push must survive"
        );
    }

    /// Temp repo where `tools/rust` is tracked on `main` and a scan has just
    /// regenerated it: one modified tracked file, one new untracked file.
    fn repo_with_generated_rust() -> tempfile::TempDir {
        let dir = temp_repo();
        let p = dir.path();
        std::fs::create_dir_all(p.join("tools/rust/versions")).unwrap();
        std::fs::write(p.join("tools/rust/index.json"), "old").unwrap();
        git(p, &["add", "tools"]).unwrap();
        git(p, &["commit", "-q", "-m", "rust"]).unwrap();
        std::fs::write(p.join("tools/rust/index.json"), "new").unwrap();
        std::fs::write(p.join("tools/rust/versions/2.0.0.json"), "{}").unwrap();
        dir
    }

    /// Stand-in for `open_pr`/`update_pr`'s git half: branch, stage the
    /// tool's directory, commit.
    fn commit_on(p: &Path, branch: &str) -> Result<String> {
        git(p, &["checkout", "-q", "-B", branch])?;
        git(p, &["add", "tools/rust"])?;
        git(p, &["commit", "-q", "-m", "scan"])?;
        Ok(format!("https://example/{branch}"))
    }

    /// Main is untouched and clean: regenerated files neither leaked into a
    /// commit on `main` nor left in the tree for the next tool.
    fn assert_clean_at_main(p: &Path) {
        assert_eq!(current_branch(p), "main");
        assert_eq!(
            std::fs::read_to_string(p.join("tools/rust/index.json")).unwrap(),
            "old"
        );
        assert!(!p.join("tools/rust/versions/2.0.0.json").exists());
        assert_eq!(git(p, &["status", "--porcelain"]).unwrap(), "");
    }

    fn never_update(_: &OpenPr) -> Result<String> {
        panic!("update must not run for this decision")
    }

    fn no_log(_: &str) {}

    fn never_open() -> Result<String> {
        panic!("open must not run for this decision")
    }

    fn open_pr_fixture(number: u64, head: &str) -> OpenPr {
        OpenPr {
            number,
            head: head.to_string(),
            head_oid: "abc".to_string(),
            body: String::new(),
            cross_repo: false,
            commits_verified: false,
        }
    }

    #[test]
    fn deliver_skip_discards_and_stays_on_base() {
        let dir = repo_with_generated_rust();
        let p = dir.path();
        let decision = Decision::Skip {
            pr: 398,
            covered: vec!["2.0.0".to_string()],
        };
        let d = deliver_with(p, "rust", &decision, no_log, never_update, never_open).unwrap();
        assert!(matches!(d, Delivery::Skipped(398)));
        assert_clean_at_main(p);
    }

    #[test]
    fn deliver_open_commits_on_its_branch_and_returns() {
        let dir = repo_with_generated_rust();
        let p = dir.path();
        let d = deliver_with(p, "rust", &Decision::OpenNew, no_log, never_update, || {
            commit_on(p, "scan/rust/2026-10-16")
        })
        .unwrap();
        assert!(matches!(d, Delivery::Opened(ref u) if u.ends_with("scan/rust/2026-10-16")));
        assert_clean_at_main(p);
        let on_branch = git(p, &["show", "scan/rust/2026-10-16:tools/rust/index.json"]).unwrap();
        assert_eq!(
            on_branch, "new",
            "the PR branch carries the regenerated files"
        );
    }

    #[test]
    fn deliver_update_passes_the_pr_and_returns() {
        // Both regen reasons take the same update path; the logged line is
        // what tells them apart (#529), so it is asserted per reason.
        for reason in [RegenReason::MissingVersions, RegenReason::UnsignedCommits] {
            let mut logged = Vec::new();
            let dir = repo_with_generated_rust();
            let p = dir.path();
            let pr = open_pr_fixture(398, "scan/rust/2026-10-15");
            let decision = Decision::UpdateExisting {
                pr: pr.clone(),
                reason,
            };
            let d = deliver_with(
                p,
                "rust",
                &decision,
                |line| logged.push(line.to_string()),
                |got| {
                    assert_eq!(got, &pr);
                    commit_on(p, &got.head)
                },
                never_open,
            )
            .unwrap();
            assert!(matches!(d, Delivery::Updated(_)), "{reason:?}");
            assert_eq!(logged, vec![regen_message("rust", 398, reason)]);
            assert_clean_at_main(p);
        }
    }

    #[test]
    fn deliver_failure_discards_generated_and_returns() {
        // A push rejected before commit: the action fails on its branch
        // with the regenerated files still uncommitted.
        let dir = repo_with_generated_rust();
        let p = dir.path();
        let err = deliver_with(p, "rust", &Decision::OpenNew, no_log, never_update, || {
            git(p, &["checkout", "-q", "-b", "scan/rust/x"])?;
            anyhow::bail!("push rejected")
        })
        .unwrap_err();
        assert!(err.to_string().contains("push rejected"));
        assert_clean_at_main(p);
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
