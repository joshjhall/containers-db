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
pub(super) mod tests {
    use super::*;
    use crate::publish::testing::BareRemote;
    use chrono::TimeZone;

    /// Temp repo on `main` with one commit. Hermetic: explicit identity,
    /// no global config needed. Panics if an inherited git variable (e.g.
    /// `GIT_DIR` from a pre-push hook) would point `git` at a different repo
    /// or config.
    fn temp_repo() -> tempfile::TempDir {
        crate::github::testing::assert_no_inherited_git_env();
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

    /// A `HOME` whose global git config (both `~/.gitconfig` and
    /// `$XDG_CONFIG_HOME/git/config`) breaks the fixtures four ways:
    /// signing through a `gpg.program` that always exits 1; a `pre-commit`
    /// hook that does too; a `reference-transaction` hook that aborts every
    /// ref update (so `init` and `update-ref`, which commit-free fixtures
    /// like `BareRemote` rely on); and an `init.templateDir` whose `config`
    /// already defines `origin`, so `git remote add origin` fails.
    /// Returns `(home, xdg_config_home)`.
    fn hostile_home(root: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let home = root.join("home");
        let xdg = home.join("xdg");
        let hooks = root.join("hooks");
        let template = root.join("template");
        std::fs::create_dir_all(xdg.join("git")).unwrap();
        std::fs::create_dir_all(&hooks).unwrap();
        std::fs::create_dir_all(&template).unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        for name in ["pre-commit", "reference-transaction"] {
            let hook = hooks.join(name);
            std::fs::write(
                &hook,
                format!("#!/bin/sh\necho hostile-{name} >&2\nexit 1\n"),
            )
            .unwrap();
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(
            template.join("config"),
            "[remote \"origin\"]\n\turl = hostile\n",
        )
        .unwrap();
        let config = format!(
            "[commit]\n\tgpgsign = true\n[gpg]\n\tprogram = false\n[core]\n\thooksPath = {}\n\
             [init]\n\ttemplateDir = {}\n",
            hooks.display(),
            template.display()
        );
        std::fs::write(home.join(".gitconfig"), &config).unwrap();
        std::fs::write(xdg.join("git/config"), &config).unwrap();
        (home, xdg)
    }

    /// Run the `#[ignore]`d test `name` in a child whose `HOME` holds the
    /// [`hostile_home`] global git config. Shared with the `publish` and
    /// `github` fixture tests.
    pub(crate) fn run_under_hostile_home(name: &str) {
        let root = tempfile::tempdir().unwrap();
        let (home, xdg) = hostile_home(root.path());
        let path = std::path::PathBuf::from(std::env::var_os("PATH").unwrap());
        crate::tests::run_with_env(name, &path, &[("HOME", &home), ("XDG_CONFIG_HOME", &xdg)]);
    }

    /// The fixtures ignore the host's global and system git config (#588):
    /// under a `HOME` whose config fails every commit, `temp_repo` and
    /// `clone_with_open_scan_pr` still build their repos.
    #[test]
    fn fixtures_ignore_a_hostile_global_git_config() {
        run_under_hostile_home("pr::tests::hostile_home_fixtures_still_commit");
    }

    #[test]
    #[ignore = "run by fixtures_ignore_a_hostile_global_git_config"]
    fn hostile_home_fixtures_still_commit() {
        if !crate::tests::in_child() {
            return;
        }
        let dir = temp_repo();
        assert_eq!(
            git(dir.path(), &["rev-list", "--count", "HEAD"])
                .unwrap()
                .trim(),
            "1"
        );
        let (dir, _origin, pr) = clone_with_open_scan_pr();
        assert_eq!(rev(dir.path(), &pr.head), pr.head_oid);
    }

    /// The control for the fixture tests: the same hostile `HOME` breaks a
    /// plain `git` through EACH of its four mechanisms, so those tests
    /// cannot pass because the config, or any part of it, was never read.
    #[test]
    fn a_plain_git_reads_the_hostile_global_config() {
        run_under_hostile_home("pr::tests::hostile_home_fails_a_plain_commit");
    }

    #[test]
    #[ignore = "run by a_plain_git_reads_the_hostile_global_config"]
    fn hostile_home_fails_a_plain_commit() {
        if !crate::tests::in_child() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        // No system config, so a failure can only come from the global one
        // under test; `off` disables the other mechanisms, so each step
        // below can fail for exactly one reason.
        let plain = |off: &[&str], args: &[&str]| {
            std::process::Command::new("git")
                .current_dir(p)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
                .args(off)
                .args(args)
                .output()
                .unwrap()
        };
        let hooks_off = ["-c", "core.hooksPath=/dev/null"];
        // `init` writes HEAD through the reference-transaction hook.
        let out = plain(&[], &["init", "-q", "-b", "main"]);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !out.status.success() && stderr.contains("hostile-reference-transaction"),
            "the hostile global config's reference-transaction hook was not applied: {out:?}"
        );
        std::fs::remove_dir_all(p.join(".git")).unwrap();
        assert!(
            plain(&hooks_off, &["init", "-q", "-b", "main"])
                .status
                .success()
        );
        std::fs::write(p.join("a"), "a").unwrap();
        assert!(plain(&[], &["add", "a"]).status.success());

        // The template seeded `origin`; `remote add` writes config only, so
        // no hook or signing is involved.
        let out = plain(&[], &["config", "remote.origin.url"]);
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "hostile",
            "the hostile global config's init.templateDir was not applied: {out:?}"
        );
        let out = plain(&[], &["remote", "add", "origin", "x"]);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !out.status.success() && stderr.contains("remote origin already exists"),
            "the templated origin did not block `remote add`: {out:?}"
        );

        let out = plain(&hooks_off, &["commit", "-q", "-m", "init"]);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !out.status.success() && stderr.contains("gpg failed to sign"),
            "the hostile global config's signing was not applied: {out:?}"
        );

        let signing_off = ["-c", "commit.gpgsign=false"];
        let out = plain(&signing_off, &["commit", "-q", "-m", "init"]);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !out.status.success() && stderr.contains("hostile-pre-commit"),
            "the hostile global config's hooksPath was not applied: {out:?}"
        );
    }

    /// `git_command` carries both halves of the isolation, so every
    /// caller routed through it ignores the global AND the system config
    /// (the system half cannot be planted without root, so it is pinned
    /// here rather than exercised).
    #[test]
    fn git_command_ignores_global_and_system_config() {
        let cmd = crate::git_command();
        let envs: Vec<_> = cmd
            .get_envs()
            .map(|(k, v)| (k.to_os_string(), v.map(|v| v.to_os_string())))
            .collect();
        for (key, want) in [
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_CONFIG_NOSYSTEM", "1"),
        ] {
            assert!(
                envs.contains(&(key.into(), Some(want.into()))),
                "{key}={want} missing: {envs:?}"
            );
        }
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

    #[test]
    fn deliver_rejects_empty_new_versions() {
        // Not a repo and no token: reaching `list_open_prs` would fail
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

    pub(super) fn open_pr_fixture(number: u64, head: &str) -> OpenPr {
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

    /// Drive [`deliver_via`] against a mock API: open PR #398 on
    /// `scan/rust/old` (head `h1`) already lists `2.0.0`, and its commit
    /// listing answers with `commits`. Pins that the signature lookup runs
    /// before `decide` (#533): without it the PR reads unsigned and is
    /// regenerated on every run.
    mod deliver_via_api {
        use super::*;
        use crate::github::testing::client_for;
        use serde_json::json;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        const VERSION: &str = "2.0.0";

        async fn server_with_covering_pr(commits: ResponseTemplate) -> MockServer {
            let server = MockServer::start().await;
            let body = pr_body(
                "rust",
                &[VERSION.to_string()],
                None,
                Score::Active,
                &ActivitySignals::default(),
                7,
            );
            let pr = json!({
                "number": 398,
                "body": body,
                "head": {"ref": "scan/rust/old", "sha": "h1", "repo": {"full_name": "o/r"}},
                "base": {"ref": "main", "repo": {"full_name": "o/r"}},
            });
            Mock::given(method("GET"))
                .and(path("/repos/o/r/pulls"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!([pr])))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/repos/o/r/pulls/398/commits"))
                .respond_with(commits)
                .expect(1)
                .mount(&server)
                .await;
            server
        }

        fn commits(verified: bool) -> ResponseTemplate {
            ResponseTemplate::new(200).set_body_json(
                json!([{"sha": "h1", "commit": {"verification": {"verified": verified}}}]),
            )
        }

        fn deliver_rust(
            p: &Path,
            server: &MockServer,
            update: impl FnOnce(&OpenPr) -> Result<String>,
        ) -> Result<Delivery> {
            deliver_via(
                p,
                &client_for(server),
                "rust",
                "scan/rust/",
                &[VERSION.to_string()],
                update,
                never_open,
            )
        }

        /// The update must target the listed PR at its listed head.
        fn update_398(pr: &OpenPr) -> Result<String> {
            assert_eq!((pr.number, pr.head_oid.as_str()), (398, "h1"));
            Ok("https://example/pull/398".to_string())
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_covering_pr_with_verified_commits_is_skipped() {
            let server = server_with_covering_pr(commits(true)).await;
            let dir = repo_with_generated_rust();
            let d = deliver_rust(dir.path(), &server, never_update).unwrap();
            assert!(matches!(d, Delivery::Skipped(398)), "{d:?}");
            assert_clean_at_main(dir.path());
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_covering_pr_with_an_unsigned_commit_is_updated() {
            let server = server_with_covering_pr(commits(false)).await;
            let dir = repo_with_generated_rust();
            let d = deliver_rust(dir.path(), &server, update_398).unwrap();
            assert!(
                matches!(&d, Delivery::Updated(url) if url == "https://example/pull/398"),
                "{d:?}"
            );
        }

        /// A failed lookup fails closed into a regen, never a skip. A 404 is
        /// definitive, so it is asked once and no retry delay is slept.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_covering_pr_whose_lookup_fails_is_updated() {
            let server = server_with_covering_pr(ResponseTemplate::new(404)).await;
            let dir = repo_with_generated_rust();
            let d = deliver_rust(dir.path(), &server, update_398).unwrap();
            assert!(
                matches!(&d, Delivery::Updated(url) if url == "https://example/pull/398"),
                "{d:?}"
            );
        }
    }

    /// The PR writes against a mock API, publishing to a bare `origin`:
    /// the request bodies must carry GitHub's field names exactly (a
    /// misnamed one would not match and the mock answers 404), and a
    /// response without `html_url` is an error, never an empty URL.
    mod pr_writes_api {
        use super::*;
        use crate::github::testing::client_for;
        use serde_json::json;
        use wiremock::matchers::{body_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        const TITLE: &str = "feat(rust): add upstream versions 2.0.0";
        const BODY: &str = "the body";
        const URL: &str = "https://github.example/o/r/pull/9";

        async fn serve(
            verb: &str,
            p: &str,
            want: serde_json::Value,
            answer: ResponseTemplate,
        ) -> MockServer {
            let server = MockServer::start().await;
            Mock::given(method(verb))
                .and(path(p))
                .and(body_json(want))
                .respond_with(answer)
                .expect(1)
                .mount(&server)
                .await;
            server
        }

        fn created(answer: serde_json::Value) -> ResponseTemplate {
            ResponseTemplate::new(201).set_body_json(answer)
        }

        fn open_want() -> serde_json::Value {
            json!({"base": "main", "head": "scan/rust/new", "title": TITLE, "body": BODY})
        }

        async fn open(answer: ResponseTemplate) -> (Result<String>, tempfile::TempDir) {
            let (dir, origin, _) = clone_with_open_scan_pr();
            let server = serve("POST", "/repos/o/r/pulls", open_want(), answer).await;
            let remote = BareRemote::new(origin.path());
            let got = open_pr_with(
                dir.path(),
                &client_for(&server),
                &remote,
                "rust",
                "scan/rust/new",
                TITLE,
                BODY,
                "main",
            );
            (got, origin)
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn open_pr_posts_the_pr_and_returns_its_html_url() {
            let (got, origin) = open(created(json!({"number": 9, "html_url": URL}))).await;
            assert_eq!(got.unwrap(), URL);
            let o = origin.path();
            assert_eq!(
                rev(o, "scan/rust/new^"),
                rev(o, "main"),
                "the published commit sits directly on main"
            );
            let tree = git(o, &["ls-tree", "-r", "--name-only", "scan/rust/new"]).unwrap();
            assert!(
                tree.lines().any(|l| l == "tools/rust/versions/2.0.0.json"),
                "the tip carries the regenerated files: {tree}"
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn open_pr_without_html_url_is_an_error() {
            let (got, _origin) = open(created(json!({"number": 9, "url": URL}))).await;
            let err = got.unwrap_err();
            assert!(format!("{err:#}").contains("no html_url"), "{err:#}");
        }

        /// The branch publishes, then GitHub refuses the PR (e.g. 422 for
        /// an existing PR on that head): an error naming the status, never
        /// a URL.
        #[tokio::test(flavor = "multi_thread")]
        async fn open_pr_with_a_rejected_pr_write_is_an_error() {
            let refused = ResponseTemplate::new(422).set_body_json(json!({"message": "nope"}));
            let (got, origin) = open(refused).await;
            let err = got.unwrap_err();
            let status = err
                .downcast_ref::<crate::github::HttpStatusError>()
                .map(|e| e.status);
            assert_eq!(
                status,
                Some(reqwest::StatusCode::UNPROCESSABLE_ENTITY),
                "{err:#}"
            );
            assert!(
                BareRemote::new(origin.path())
                    .head("scan/rust/new")
                    .is_some(),
                "the failure is the PR write, after the branch published"
            );
        }

        /// Returns the update and the bare `origin`, for asserting the push.
        async fn update(answer: ResponseTemplate) -> (Result<String>, tempfile::TempDir, OpenPr) {
            let (dir, origin, pr) = clone_with_open_scan_pr();
            let want = json!({"title": TITLE, "body": BODY});
            let server = serve("PATCH", "/repos/o/r/pulls/398", want, answer).await;
            let remote = BareRemote::new(origin.path());
            let got = update_pr_with(
                dir.path(),
                &client_for(&server),
                &remote,
                "rust",
                &pr,
                TITLE,
                BODY,
            );
            (got, origin, pr)
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn update_pr_patches_the_pr_and_returns_its_html_url() {
            let (got, origin, pr) = update(created(json!({"html_url": URL}))).await;
            assert_eq!(got.unwrap(), URL);
            let o = origin.path();
            assert_ne!(
                rev(o, &pr.head),
                pr.head_oid,
                "the regenerated branch is pushed"
            );
            assert_eq!(rev(o, &format!("{}^", pr.head)), rev(o, "main"));
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn update_pr_without_html_url_is_an_error() {
            let err = update(created(json!({"number": 398}))).await.0.unwrap_err();
            assert!(format!("{err:#}").contains("no html_url"), "{err:#}");
        }

        /// The regenerated branch is pushed, then GitHub refuses the PR
        /// edit: an error naming the status, never a URL.
        #[tokio::test(flavor = "multi_thread")]
        async fn update_pr_with_a_rejected_pr_write_is_an_error() {
            let refused = ResponseTemplate::new(422).set_body_json(json!({"message": "nope"}));
            let (got, origin, pr) = update(refused).await;
            let err = got.unwrap_err();
            let status = err
                .downcast_ref::<crate::github::HttpStatusError>()
                .map(|e| e.status);
            assert_eq!(
                status,
                Some(reqwest::StatusCode::UNPROCESSABLE_ENTITY),
                "{err:#}"
            );
            assert_ne!(
                rev(origin.path(), &pr.head),
                pr.head_oid,
                "the failure is the PR edit, after the branch was pushed"
            );
        }
    }

    /// The public [`open_pr`]/[`update_pr`] wrappers, publishing through
    /// [`ApiRemote::discover`] against a mock GraphQL API. These pin the
    /// discover → publish → PR-write wiring (repository id, staging branch,
    /// signed commit, lease); what lands in git is pinned by the `_with`
    /// tests above against a bare remote.
    mod pr_wrappers_api {
        use super::*;
        use crate::github::testing::client_for;
        use serde_json::json;
        use wiremock::matchers::{
            body_json, body_partial_json, body_string_contains, method, path,
        };
        use wiremock::{Mock, MockServer, ResponseTemplate};

        const TITLE: &str = "feat(rust): add upstream versions 2.0.0";
        const BODY: &str = "the body";
        const URL: &str = "https://github.example/o/r/pull/9";
        const SIGNED: &str = "5160ed0000000000000000000000000000000000";

        /// The tree the scan commit will have: `main` plus `tools/rust/`.
        /// Built on a throwaway index, so the clone's own stays unstaged —
        /// as `deliver` hands it over — and the wrapper's `git add` is what
        /// the tree check actually exercises.
        fn scan_tree(p: &Path) -> String {
            let index = tempfile::tempdir().unwrap();
            let run = |args: &[&str]| {
                let out = crate::git_command()
                    .current_dir(p)
                    .env("GIT_INDEX_FILE", index.path().join("index"))
                    .args(args)
                    .output()
                    .unwrap();
                assert!(out.status.success(), "git {args:?}");
                String::from_utf8(out.stdout).unwrap()
            };
            run(&["read-tree", "main"]);
            run(&["add", "tools/rust"]);
            run(&["write-tree"]).trim().to_string()
        }

        fn ok(data: serde_json::Value) -> ResponseTemplate {
            ResponseTemplate::new(200).set_body_json(json!({ "data": data }))
        }

        /// GraphQL `variables` the mock must see exactly once.
        async fn expect_graphql(
            server: &MockServer,
            vars: serde_json::Value,
            answer: ResponseTemplate,
        ) {
            Mock::given(method("POST"))
                .and(path("/graphql"))
                .and(body_partial_json(json!({ "variables": vars })))
                .respond_with(answer)
                .expect(1)
                .mount(server)
                .await;
        }

        /// A `createCommitOnBranch` answer: the signed commit with `tree`.
        fn signed_with(tree: &str) -> ResponseTemplate {
            ok(json!({"createCommitOnBranch": {"commit": {"oid": SIGNED, "tree": {"oid": tree}}}}))
        }

        /// True when `name` has the shape [`publish::publish_head`] gives a
        /// staging branch: `scanner-staging/<branch>/` plus 12 hex digits.
        /// The shape only gates the mocks; [`assert_staging_named_for`]
        /// pins which commit the digits come from.
        fn is_staging_for(name: &str, branch: &str) -> bool {
            name.strip_prefix(&format!("scanner-staging/{branch}/"))
                .is_some_and(|sha| {
                    sha.len() == 12 && sha.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
                })
        }

        /// The one staging branch created and the one deleted are both
        /// named for the local commit published from `branch`, which the
        /// wrapper leaves checked out whether or not the publish succeeded.
        async fn assert_staging_named_for(server: &MockServer, p: &Path, branch: &str) {
            let want = format!("scanner-staging/{branch}/{}", &rev(p, branch)[..12]);
            let sent = server.received_requests().await.unwrap();
            let created: Vec<String> = sent
                .iter()
                .filter_map(|r| {
                    let body: serde_json::Value = serde_json::from_slice(&r.body).ok()?;
                    let name = body["variables"]["i"]["name"].as_str()?;
                    name.strip_prefix("refs/heads/")
                        .filter(|n| n.starts_with("scanner-staging/"))
                        .map(str::to_string)
                })
                .collect();
            let deleted: Vec<&str> = sent
                .iter()
                .filter(|r| r.method == "DELETE")
                .filter_map(|r| r.url.path().strip_prefix("/repos/o/r/git/refs/heads/"))
                .collect();
            assert_eq!(deleted, [want.as_str()], "staging branch deleted");
            assert_eq!(created, [want], "staging branch created");
        }

        /// Repository-id discovery, the staging branch for `branch` at
        /// `main` (its name pinned at creation), the signed commit answering
        /// `commit`, and the staging cleanup.
        async fn publish_mocks(
            server: &MockServer,
            branch: &str,
            main: &str,
            commit: ResponseTemplate,
        ) {
            publish_mocks_deleting(server, branch, main, commit, ResponseTemplate::new(204)).await;
        }

        /// [`publish_mocks`] with the staging DELETE answering `delete`.
        async fn publish_mocks_deleting(
            server: &MockServer,
            branch: &str,
            main: &str,
            commit: ResponseTemplate,
            delete: ResponseTemplate,
        ) {
            expect_graphql(
                server,
                json!({"o": "o", "n": "r"}),
                ok(json!({"repository": {"id": "R_1"}})),
            )
            .await;
            let staging_branch = branch.to_string();
            Mock::given(method("POST"))
                .and(path("/graphql"))
                .and(body_partial_json(
                    json!({"variables": {"i": {"repositoryId": "R_1", "oid": main}}}),
                ))
                .and(move |req: &wiremock::Request| {
                    let body: serde_json::Value = match serde_json::from_slice(&req.body) {
                        Ok(v) => v,
                        Err(_) => return false,
                    };
                    body["variables"]["i"]["name"]
                        .as_str()
                        .and_then(|n| n.strip_prefix("refs/heads/"))
                        .is_some_and(|n| is_staging_for(n, &staging_branch))
                })
                .respond_with(ok(json!({})))
                .expect(1)
                .mount(server)
                .await;
            expect_graphql(
                server,
                json!({"i": {"expectedHeadOid": main, "message": {"headline": TITLE}}}),
                commit,
            )
            .await;
            let staging_branch = branch.to_string();
            Mock::given(method("DELETE"))
                .and(move |req: &wiremock::Request| {
                    req.url
                        .path()
                        .strip_prefix("/repos/o/r/git/refs/heads/")
                        .is_some_and(|n| is_staging_for(n, &staging_branch))
                })
                .respond_with(delete)
                .expect(1)
                .mount(server)
                .await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn open_pr_discovers_the_repo_and_publishes_a_signed_branch() {
            open_publishes(ResponseTemplate::new(204)).await;
        }

        /// The staging cleanup is best effort: a DELETE GitHub refuses
        /// leaves clutter, not a failed PR. The PR is still written once
        /// and its URL returned.
        #[tokio::test(flavor = "multi_thread")]
        async fn open_pr_with_a_failed_staging_delete_still_returns_the_pr() {
            open_publishes(ResponseTemplate::new(500)).await;
        }

        /// A full open: discovery, signed publish, the PR POSTed exactly
        /// once and its URL returned, with the staging DELETE answering
        /// `delete`.
        async fn open_publishes(delete: ResponseTemplate) {
            let (dir, _origin, _) = clone_with_open_scan_pr();
            let p = dir.path();
            let (main, tree) = (rev(p, "main"), scan_tree(p));
            let server = MockServer::start().await;
            publish_mocks_deleting(&server, "scan/rust/new", &main, signed_with(&tree), delete)
                .await;
            expect_graphql(
                &server,
                json!({"i": {"repositoryId": "R_1", "name": "refs/heads/scan/rust/new", "oid": SIGNED}}),
                ok(json!({})),
            )
            .await;
            Mock::given(method("POST"))
                .and(path("/repos/o/r/pulls"))
                .and(body_json(
                    json!({"base": "main", "head": "scan/rust/new", "title": TITLE, "body": BODY}),
                ))
                .respond_with(ResponseTemplate::new(201).set_body_json(json!({"html_url": URL})))
                .expect(1)
                .mount(&server)
                .await;
            let got = open_pr(
                p,
                &client_for(&server),
                "rust",
                "scan/rust/new",
                TITLE,
                BODY,
                "main",
            );
            assert_eq!(got.unwrap(), URL);
            assert_staging_named_for(&server, p, "scan/rust/new").await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn update_pr_discovers_the_repo_and_moves_the_branch_on_its_lease() {
            update_publishes(ResponseTemplate::new(204)).await;
        }

        /// The same best-effort cleanup on an update: a refused staging
        /// DELETE does not fail it, and the PR is still PATCHed once.
        #[tokio::test(flavor = "multi_thread")]
        async fn update_pr_with_a_failed_staging_delete_still_returns_the_pr() {
            update_publishes(ResponseTemplate::new(500)).await;
        }

        /// A full update: discovery, signed publish, the branch moved on
        /// its lease, the PR PATCHed exactly once and its URL returned,
        /// with the staging DELETE answering `delete`.
        async fn update_publishes(delete: ResponseTemplate) {
            let (dir, _origin, pr) = clone_with_open_scan_pr();
            let p = dir.path();
            let (main, tree) = (rev(p, "main"), scan_tree(p));
            let server = MockServer::start().await;
            publish_mocks_deleting(&server, "scan/rust/old", &main, signed_with(&tree), delete)
                .await;
            expect_graphql(
                &server,
                json!({"i": {"repositoryId": "R_1", "refUpdates": [{
                    "name": "refs/heads/scan/rust/old",
                    "beforeOid": pr.head_oid,
                    "afterOid": SIGNED,
                    "force": true,
                }]}}),
                ok(json!({})),
            )
            .await;
            Mock::given(method("PATCH"))
                .and(path("/repos/o/r/pulls/398"))
                .and(body_json(json!({"title": TITLE, "body": BODY})))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"html_url": URL})))
                .expect(1)
                .mount(&server)
                .await;
            let got = update_pr(p, &client_for(&server), "rust", &pr, TITLE, BODY);
            assert_eq!(got.unwrap(), URL);
            assert_staging_named_for(&server, p, &pr.head).await;
        }

        /// Discovery runs first: without a repository id nothing is
        /// committed or published, and no PR is written.
        #[tokio::test(flavor = "multi_thread")]
        async fn open_pr_without_a_repo_id_writes_nothing() {
            let (dir, _origin, _) = clone_with_open_scan_pr();
            let server = MockServer::start().await;
            expect_graphql(
                &server,
                json!({"o": "o", "n": "r"}),
                ok(json!({"repository": null})),
            )
            .await;
            Mock::given(method("POST"))
                .and(path("/repos/o/r/pulls"))
                .respond_with(ResponseTemplate::new(201))
                .expect(0)
                .mount(&server)
                .await;
            let got = open_pr(
                dir.path(),
                &client_for(&server),
                "rust",
                "scan/rust/new",
                TITLE,
                BODY,
                "main",
            );
            let err = got.unwrap_err();
            assert!(format!("{err:#}").contains("no repository id"), "{err:#}");
            assert_eq!(current_branch(dir.path()), "main", "no branch was cut");
        }

        /// Discovery runs first for an update too: the PR's branch is not
        /// reset locally, and no PR edit is sent.
        #[tokio::test(flavor = "multi_thread")]
        async fn update_pr_without_a_repo_id_writes_nothing() {
            let (dir, _origin, pr) = clone_with_open_scan_pr();
            let p = dir.path();
            let server = MockServer::start().await;
            expect_graphql(
                &server,
                json!({"o": "o", "n": "r"}),
                ok(json!({"repository": null})),
            )
            .await;
            Mock::given(method("PATCH"))
                .respond_with(ResponseTemplate::new(200))
                .expect(0)
                .mount(&server)
                .await;
            let got = update_pr(p, &client_for(&server), "rust", &pr, TITLE, BODY);
            let err = got.unwrap_err();
            assert!(format!("{err:#}").contains("no repository id"), "{err:#}");
            assert_eq!(
                current_branch(p),
                "main",
                "the PR branch was not checked out"
            );
            assert_eq!(rev(p, &pr.head), pr.head_oid, "the PR branch was not reset");
        }

        /// A server answering repository-id discovery with `answer` and
        /// nothing else: any request past discovery is unmatched.
        async fn discover_fails(answer: ResponseTemplate) -> MockServer {
            let server = MockServer::start().await;
            expect_graphql(&server, json!({"o": "o", "n": "r"}), answer).await;
            server
        }

        fn graphql_errors() -> ResponseTemplate {
            ResponseTemplate::new(200).set_body_json(json!({"errors": [{"message": "nope"}]}))
        }

        /// Whether an error is the failure a test provoked.
        type IsExpected = fn(&anyhow::Error) -> bool;

        /// The two ways a GraphQL call fails, each with the check that the
        /// error is that failure: a refused request surfaces as a typed
        /// [`crate::github::HttpStatusError`], a GraphQL `errors` payload
        /// as its own message.
        fn graphql_failures() -> [(ResponseTemplate, IsExpected); 2] {
            [
                (ResponseTemplate::new(500), |e| {
                    e.downcast_ref::<crate::github::HttpStatusError>()
                        .is_some_and(|h| h.status == 500)
                }),
                (graphql_errors(), |e| {
                    format!("{e:#}").contains("GraphQL errors")
                }),
            ]
        }

        /// A failed discovery — the API refusing outright, or answering with
        /// GraphQL `errors` — is an error before any git or API write: no
        /// branch is cut and discovery is the only request sent.
        #[tokio::test(flavor = "multi_thread")]
        async fn open_pr_with_a_failed_discovery_writes_nothing() {
            for (answer, is_expected) in graphql_failures() {
                let (dir, _origin, _) = clone_with_open_scan_pr();
                let server = discover_fails(answer).await;
                let got = open_pr(
                    dir.path(),
                    &client_for(&server),
                    "rust",
                    "scan/rust/new",
                    TITLE,
                    BODY,
                    "main",
                );
                let err = got.unwrap_err();
                assert!(is_expected(&err), "{err:#}");
                let sent = server.received_requests().await.unwrap();
                assert_eq!(sent.len(), 1, "only discovery was sent: {sent:?}");
                assert_eq!(current_branch(dir.path()), "main", "no branch was cut");
            }
        }

        /// The same for an update: the PR branch is neither checked out nor
        /// reset, and no request follows discovery.
        #[tokio::test(flavor = "multi_thread")]
        async fn update_pr_with_a_failed_discovery_writes_nothing() {
            for (answer, is_expected) in graphql_failures() {
                let (dir, _origin, pr) = clone_with_open_scan_pr();
                let p = dir.path();
                let server = discover_fails(answer).await;
                let got = update_pr(p, &client_for(&server), "rust", &pr, TITLE, BODY);
                let err = got.unwrap_err();
                assert!(is_expected(&err), "{err:#}");
                let sent = server.received_requests().await.unwrap();
                assert_eq!(sent.len(), 1, "only discovery was sent: {sent:?}");
                assert_eq!(
                    current_branch(p),
                    "main",
                    "the PR branch was not checked out"
                );
                assert_eq!(rev(p, &pr.head), pr.head_oid, "the PR branch was not reset");
            }
        }

        /// A server where discovery succeeds and the staging-branch
        /// `createRef` answers `answer`. Nothing may follow a failed
        /// create: no signed commit and no staging cleanup.
        async fn create_fails(answer: ResponseTemplate) -> MockServer {
            let server = MockServer::start().await;
            expect_graphql(
                &server,
                json!({"o": "o", "n": "r"}),
                ok(json!({"repository": {"id": "R_1"}})),
            )
            .await;
            Mock::given(method("POST"))
                .and(path("/graphql"))
                .and(body_string_contains("createRef"))
                .and(body_string_contains("refs/heads/scanner-staging/"))
                .respond_with(answer)
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/graphql"))
                .and(body_string_contains("createCommitOnBranch"))
                .respond_with(ok(json!({})))
                .expect(0)
                .mount(&server)
                .await;
            Mock::given(method("DELETE"))
                .respond_with(ResponseTemplate::new(204))
                .expect(0)
                .mount(&server)
                .await;
            server
        }

        /// The staging branch is created before anything is committed, so
        /// a failed create is an error that stops the publish outright: no
        /// commit, no cleanup (there is nothing to clean), and no branch
        /// or PR written. Only discovery and the create are sent.
        #[tokio::test(flavor = "multi_thread")]
        async fn open_pr_with_a_failed_staging_create_writes_nothing() {
            for (answer, is_expected) in graphql_failures() {
                let (dir, _origin, _) = clone_with_open_scan_pr();
                let server = create_fails(answer).await;
                refuse_open_writes(&server).await;
                let got = open_pr(
                    dir.path(),
                    &client_for(&server),
                    "rust",
                    "scan/rust/new",
                    TITLE,
                    BODY,
                    "main",
                );
                let err = got.unwrap_err();
                assert!(is_expected(&err), "{err:#}");
                assert!(
                    format!("{err:#}").contains("creating staging branch"),
                    "{err:#}"
                );
                let sent = server.received_requests().await.unwrap();
                assert_eq!(sent.len(), 2, "only discovery and the create: {sent:?}");
            }
        }

        /// The same for an update: the PR's branch is not moved, the PR is
        /// not edited, and nothing follows the failed create.
        #[tokio::test(flavor = "multi_thread")]
        async fn update_pr_with_a_failed_staging_create_writes_nothing() {
            for (answer, is_expected) in graphql_failures() {
                let (dir, _origin, pr) = clone_with_open_scan_pr();
                let server = create_fails(answer).await;
                refuse_update_writes(&server).await;
                let got = update_pr(dir.path(), &client_for(&server), "rust", &pr, TITLE, BODY);
                let err = got.unwrap_err();
                assert!(is_expected(&err), "{err:#}");
                assert!(
                    format!("{err:#}").contains("creating staging branch"),
                    "{err:#}"
                );
                let sent = server.received_requests().await.unwrap();
                assert_eq!(sent.len(), 2, "only discovery and the create: {sent:?}");
            }
        }

        /// A tree no local commit has: the remote "signed" something else.
        const OTHER_TREE: &str = "0000000000000000000000000000000000000000";

        /// No branch may point at a signed commit, and no PR may be opened.
        async fn refuse_open_writes(server: &MockServer) {
            expect_graphql_never(server, json!({"i": {"name": "refs/heads/scan/rust/new"}})).await;
            Mock::given(method("POST"))
                .and(path("/repos/o/r/pulls"))
                .respond_with(ResponseTemplate::new(201))
                .expect(0)
                .mount(server)
                .await;
        }

        /// The PR's branch may not be moved, and the PR may not be edited.
        async fn refuse_update_writes(server: &MockServer) {
            Mock::given(method("POST"))
                .and(path("/graphql"))
                .and(body_string_contains("updateRefs"))
                .respond_with(ok(json!({})))
                .expect(0)
                .mount(server)
                .await;
            Mock::given(method("PATCH"))
                .respond_with(ResponseTemplate::new(200))
                .expect(0)
                .mount(server)
                .await;
        }

        /// GraphQL `variables` the mock must never see.
        async fn expect_graphql_never(server: &MockServer, vars: serde_json::Value) {
            Mock::given(method("POST"))
                .and(path("/graphql"))
                .and(body_partial_json(json!({ "variables": vars })))
                .respond_with(ok(json!({})))
                .expect(0)
                .mount(server)
                .await;
        }

        /// A signed commit whose tree differs from the local one is never
        /// published: no branch, no PR, and the staging branch is still
        /// deleted (`publish_mocks` expects the DELETE once).
        #[tokio::test(flavor = "multi_thread")]
        async fn open_pr_with_a_mismatched_tree_writes_no_pr_and_cleans_up() {
            let (dir, _origin, _) = clone_with_open_scan_pr();
            let p = dir.path();
            let main = rev(p, "main");
            let server = MockServer::start().await;
            publish_mocks(&server, "scan/rust/new", &main, signed_with(OTHER_TREE)).await;
            refuse_open_writes(&server).await;
            let got = open_pr(
                p,
                &client_for(&server),
                "rust",
                "scan/rust/new",
                TITLE,
                BODY,
                "main",
            );
            let err = got.unwrap_err();
            assert!(
                format!("{err:#}").contains("refusing to publish"),
                "{err:#}"
            );
            assert_staging_named_for(&server, p, "scan/rust/new").await;
        }

        /// The same refusal on an update: the PR's branch is not moved and
        /// the PR is not edited, and the staging branch is still deleted.
        #[tokio::test(flavor = "multi_thread")]
        async fn update_pr_with_a_mismatched_tree_writes_no_pr_and_cleans_up() {
            let (dir, _origin, pr) = clone_with_open_scan_pr();
            let p = dir.path();
            let main = rev(p, "main");
            let server = MockServer::start().await;
            publish_mocks(&server, "scan/rust/old", &main, signed_with(OTHER_TREE)).await;
            refuse_update_writes(&server).await;
            let got = update_pr(p, &client_for(&server), "rust", &pr, TITLE, BODY);
            let err = got.unwrap_err();
            assert!(
                format!("{err:#}").contains("refusing to publish"),
                "{err:#}"
            );
            assert_staging_named_for(&server, p, &pr.head).await;
        }

        /// A GraphQL error from `createCommitOnBranch` is an error with no
        /// branch or PR written, and the staging branch is still deleted.
        #[tokio::test(flavor = "multi_thread")]
        async fn open_pr_with_a_failed_commit_writes_no_pr_and_cleans_up() {
            let (dir, _origin, _) = clone_with_open_scan_pr();
            let p = dir.path();
            let main = rev(p, "main");
            let server = MockServer::start().await;
            publish_mocks(&server, "scan/rust/new", &main, graphql_errors()).await;
            refuse_open_writes(&server).await;
            let got = open_pr(
                p,
                &client_for(&server),
                "rust",
                "scan/rust/new",
                TITLE,
                BODY,
                "main",
            );
            let err = got.unwrap_err();
            assert!(
                format!("{err:#}").contains("creating the signed commit"),
                "{err:#}"
            );
            assert_staging_named_for(&server, p, "scan/rust/new").await;
        }

        /// The same for an update: a failed commit moves no branch, edits
        /// no PR, and still deletes the staging branch.
        #[tokio::test(flavor = "multi_thread")]
        async fn update_pr_with_a_failed_commit_writes_no_pr_and_cleans_up() {
            let (dir, _origin, pr) = clone_with_open_scan_pr();
            let p = dir.path();
            let main = rev(p, "main");
            let server = MockServer::start().await;
            publish_mocks(&server, "scan/rust/old", &main, graphql_errors()).await;
            refuse_update_writes(&server).await;
            let got = update_pr(p, &client_for(&server), "rust", &pr, TITLE, BODY);
            let err = got.unwrap_err();
            assert!(
                format!("{err:#}").contains("creating the signed commit"),
                "{err:#}"
            );
            assert_staging_named_for(&server, p, &pr.head).await;
        }
    }
}
