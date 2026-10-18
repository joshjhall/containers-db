//! Daily catalog scanner for containers-db.
//!
//! Walks each tool's `index.json`, decides which are due based on
//! `activity.scan_cadence_days`, dispatches to a per-tool
//! [`adapter::Adapter`] for upstream signals and new versions,
//! recomputes activity, generates `versions/<v>.json` files, and opens
//! one PR per tool. Failures surface as issues so a broken adapter
//! never produces malformed catalog data.
//!
//! In CI the work is split across two jobs ([`stage`], #416): one with no
//! secret generates and validates, the other only delivers.

pub mod activity;
pub mod adapter;
pub mod adapters;
mod backfill;
pub mod dedup;
pub mod due;
pub mod failure;
pub mod generate;
pub(crate) mod outcome;
pub mod pr;
pub mod stage;
pub mod validate;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, NaiveDate, Utc};
use clap::Parser;
use serde_json::Value;

use crate::activity::compute_tier;
use crate::adapter::{Adapter, ToolIndex, UpstreamVersion};
use crate::stage::{Activity, Failure, Kind, Manifest, Plan, Schemas};

pub use outcome::Outcome;

#[derive(Debug, Parser)]
#[command(name = "scanner", about = "containers-db catalog scanner")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// How a scan applies what it generated. Shared by `due`, `tool`, and
/// `backfill`.
#[derive(Debug, clap::Args)]
pub struct RunArgs {
    /// Print the plan but don't write files or open PRs/issues.
    #[arg(long, conflicts_with = "stage")]
    pub dry_run: bool,
    /// Generate and validate (ajv required), then write the PRs to open
    /// to this path instead of opening them. Pushes nothing, opens no
    /// PRs or issues, and needs no token; `scanner deliver --plan` does
    /// the rest (#416).
    #[arg(long, value_name = "PATH")]
    pub stage: Option<PathBuf>,
}

#[derive(Debug, clap::Subcommand)]
pub enum Command {
    /// Scan every tool whose `activity.scanned_at + scan_cadence_days`
    /// is in the past.
    Due {
        #[command(flatten)]
        run: RunArgs,
    },
    /// Force-scan a single tool by id (skips the cadence check).
    Tool {
        /// Tool id, e.g. `rust`.
        id: String,
        #[command(flatten)]
        run: RunArgs,
    },
    /// Add every upstream stable release from `--from` up to (not
    /// including) the oldest cataloged version, templated from that oldest
    /// entry. A regular scan only looks forward from the newest release, so
    /// lowering a tool's support floor needs this.
    Backfill {
        /// Tool id, e.g. `rust`.
        id: String,
        /// Lowest version to include, e.g. `1.82.0`.
        #[arg(long)]
        from: String,
        #[command(flatten)]
        run: RunArgs,
    },
    /// Open, update, or skip the PRs a `--stage` run wrote to `--plan`, and
    /// file the tracking issues for its failed tools. Runs no adapter, npm,
    /// or ajv: this is the only step that needs the write token.
    Deliver {
        #[arg(long, value_name = "PATH")]
        plan: PathBuf,
    },
}

/// What a scan does with each tool's [`Plan`].
#[derive(Debug)]
pub(crate) enum Mode<'a> {
    DryRun,
    /// Write, validate, and deliver in one process (a local run).
    Deliver,
    /// Write and validate, record the plan here, then restore the tree.
    Stage(&'a mut Manifest),
}

/// A tool's scan result before it is applied: nothing to do, or a plan.
#[derive(Debug)]
pub(crate) enum Scanned {
    NoChange,
    Plan(Plan),
}

fn mode<'a>(args: &RunArgs, staging: &'a mut Option<Manifest>) -> Mode<'a> {
    match staging {
        Some(manifest) => Mode::Stage(manifest),
        None if args.dry_run => Mode::DryRun,
        None => Mode::Deliver,
    }
}

pub async fn run(cli: Cli) -> anyhow::Result<()> {
    let repo_root = std::env::current_dir()?;
    let registry = adapters::register_all();
    let now = Utc::now();

    match cli.command {
        Command::Due { run } => {
            let mut staging = stage::start(&repo_root, &run)?;
            let due_tools = due::find_due_tools(&repo_root, now)?;
            if due_tools.is_empty() {
                println!("scanner: no tools due for scanning");
            } else {
                // A dry run never commits. A staging run restores the tree
                // after each tool, so the base check still applies.
                let base = pr::RunBase::capture(&repo_root, run.dry_run)?;
                let mut failed = Vec::new();
                for tool in due_tools {
                    base.guard(&repo_root, &tool.id, &failed)?;
                    let Some(adapter) = registry.get(&tool.id).cloned() else {
                        println!("scanner: {}: no adapter registered, skipping", tool.id);
                        continue;
                    };
                    let scanned = scan_one(&repo_root, &tool.id, adapter, now).await;
                    let mode = mode(&run, &mut staging);
                    if !apply_with_failure_reporting(&repo_root, &tool.id, scanned, mode, now) {
                        failed.push(tool.id);
                    }
                }
                // A staged failure is filed and fails the run at delivery.
                if staging.is_none() {
                    fail_if_any(&failed)?;
                }
            }
            stage::finish(&run, staging)?;
        }
        Command::Tool { id, run } => {
            let mut staging = stage::start(&repo_root, &run)?;
            let adapter = registry
                .get(&id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no adapter registered for `{id}`"))?;
            let scanned = scan_one(&repo_root, &id, adapter, now).await;
            let ok = apply_with_failure_reporting(
                &repo_root,
                &id,
                scanned,
                mode(&run, &mut staging),
                now,
            );
            if staging.is_none() && !ok {
                fail_if_any(&[id])?;
            }
            stage::finish(&run, staging)?;
        }
        Command::Backfill { id, from, run } => {
            let mut staging = stage::start(&repo_root, &run)?;
            let adapter = registry
                .get(&id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no adapter registered for `{id}`"))?;
            // A backfill is run by hand: errors propagate, no tracking issue.
            let scanned = backfill::backfill_one(&repo_root, &id, &from, adapter, now).await?;
            let outcome = apply(&repo_root, &id, scanned, mode(&run, &mut staging))?;
            println!("scanner: {id}: {outcome:?}");
            outcome::report_step_summary(&id, &outcome);
            stage::finish(&run, staging)?;
        }
        Command::Deliver { plan } => {
            stage::deliver_manifest(&repo_root, &Manifest::read(&plan)?, now)?
        }
    }
    Ok(())
}

/// Turn the per-tool failures collected during a run into the run's
/// result, so a scan that failed for any tool exits non-zero.
///
/// The tracking issue alone is not enough: if filing it fails too (a
/// missing label, a permissions change), a run that reports success
/// leaves nothing anywhere to show that the catalog stopped updating.
pub(crate) fn fail_if_any(failed: &[String]) -> anyhow::Result<()> {
    if failed.is_empty() {
        return Ok(());
    }
    anyhow::bail!("scan failed for: {}", failed.join(", "))
}

pub(crate) fn report_failure(repo_root: &Path, tool_id: &str, error: &str, now: DateTime<Utc>) {
    if let Err(report_err) = failure::report_failure(repo_root, tool_id, error, now) {
        eprintln!("scanner: {tool_id}: failure-reporting itself failed: {report_err:#}");
    }
}

/// Apply a [`scan_one`] result, so any error surfaces as a tracking issue.
/// The orchestrator continues to the next tool — one broken adapter
/// shouldn't take down the whole scan — and the caller turns a `false`
/// return into a failed run via [`fail_if_any`].
///
/// A dry run files nothing. A staging run can't file issues (it has no
/// token), so it records the failure for `scanner deliver` to file.
///
/// Returns `true` when the tool scanned cleanly.
fn apply_with_failure_reporting(
    repo_root: &Path,
    tool_id: &str,
    scanned: anyhow::Result<Scanned>,
    mut mode: Mode<'_>,
    now: DateTime<Utc>,
) -> bool {
    let result = scanned.and_then(|s| apply(repo_root, tool_id, s, mode.reborrow()));
    match result {
        Ok(outcome) => {
            println!("scanner: {tool_id}: {outcome:?}");
            outcome::report_step_summary(tool_id, &outcome);
            true
        }
        Err(e) => {
            eprintln!("scanner: {tool_id} FAILED: {e:#}");
            let error = format!("{e:#}");
            match mode {
                Mode::DryRun => {}
                Mode::Stage(manifest) => manifest.failures.push(Failure {
                    tool_id: tool_id.to_string(),
                    error,
                }),
                Mode::Deliver => report_failure(repo_root, tool_id, &error, now),
            }
            false
        }
    }
}

impl Mode<'_> {
    fn reborrow(&mut self) -> Mode<'_> {
        match self {
            Mode::DryRun => Mode::DryRun,
            Mode::Deliver => Mode::Deliver,
            Mode::Stage(m) => Mode::Stage(m),
        }
    }
}

/// Act on one tool's [`Scanned`] result according to `mode`.
fn apply(
    repo_root: &Path,
    tool_id: &str,
    scanned: Scanned,
    mode: Mode<'_>,
) -> anyhow::Result<Outcome> {
    let Scanned::Plan(plan) = scanned else {
        return Ok(Outcome::NoChange);
    };
    match mode {
        Mode::DryRun => {
            println!(
                "scanner: {tool_id} (dry-run): {} new version(s)",
                plan.new_versions.len()
            );
            for v in &plan.new_versions {
                println!("  + {v}");
            }
            println!(
                "  score={}, cadence={}d",
                score_str(plan.activity.score),
                plan.activity.cadence_days
            );
            Ok(Outcome::DryRun {
                new_versions: plan.new_versions,
            })
        }
        Mode::Stage(manifest) => {
            // Gate in the secret-free job, then put the tree back so the
            // next tool (and the token job's checkout) starts clean.
            let gated = stage::write_and_gate(repo_root, &plan, Schemas::Require);
            let restored = pr::discard_generated(repo_root, tool_id);
            if let Err(e) = gated {
                // The gate failure is the error; a dirty tree left behind
                // would fail the next tool's base check, so say why.
                if let Err(r) = restored {
                    eprintln!("scanner: {tool_id}: restoring the tree after a failed gate: {r:#}");
                }
                return Err(e);
            }
            restored?;
            let new_versions = plan.new_versions.clone();
            manifest.plans.push(plan);
            Ok(Outcome::Staged { new_versions })
        }
        Mode::Deliver => {
            let source_repo = stage::write_and_gate(repo_root, &plan, Schemas::IfAvailable)?;
            stage::deliver_checked(repo_root, &plan, source_repo.as_deref())
        }
    }
}

/// Fetch upstream for `tool_id` and build the [`Plan`] that would add its
/// new versions. Writes nothing: the caller's [`Mode`] decides what
/// happens to the plan.
async fn scan_one(
    repo_root: &Path,
    tool_id: &str,
    adapter: Arc<dyn Adapter>,
    now: DateTime<Utc>,
) -> anyhow::Result<Scanned> {
    let index_path = repo_root.join("tools").join(tool_id).join("index.json");
    let index = ToolIndex::from_path(&index_path)?;

    let result = adapter.fetch(&index).await?;
    let (score, cadence_days) = compute_tier(&result.signals, result.source_archived, now);

    // Read the template first so we can use its `released` date as the
    // scanner's high-water mark — without this, every release the
    // upstream has ever published that predates the catalog's first
    // backfilled version would be flagged as "new".
    let existing = index.available_versions();
    let template_version = existing.last().cloned().ok_or_else(|| {
        anyhow::anyhow!("tool `{tool_id}` has no existing versions to template from")
    })?;
    let template_path = repo_root
        .join("tools")
        .join(tool_id)
        .join("versions")
        .join(format!("{template_version}.json"));
    let template_bytes = std::fs::read(&template_path)
        .map_err(|e| anyhow::anyhow!("reading template {}: {e}", template_path.display()))?;
    let template: Value = serde_json::from_slice(&template_bytes)
        .map_err(|e| anyhow::anyhow!("parsing template {}: {e}", template_path.display()))?;
    let cutoff = template
        .get("released")
        .and_then(Value::as_str)
        .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok());

    let new_versions = diff_new_versions(&index, &result.upstream_versions, cutoff);

    if new_versions.is_empty() {
        println!(
            "scanner: {tool_id}: no new versions (score={}, cadence={cadence_days}d)",
            score_str(score)
        );
        return Ok(Scanned::NoChange);
    }

    let kind = Kind::Scan {
        date: now.date_naive(),
    };
    let activity = Activity {
        score,
        cadence_days,
        signals: result.signals,
    };
    let files = stage::render_files(repo_root, tool_id, &kind, &new_versions, &activity, now)?;
    Ok(Scanned::Plan(Plan {
        tool_id: tool_id.to_string(),
        kind,
        new_versions: new_versions.iter().map(|v| v.version.clone()).collect(),
        releases: new_versions,
        now,
        activity,
        files,
    }))
}

fn diff_new_versions(
    index: &ToolIndex,
    upstream: &[UpstreamVersion],
    cutoff: Option<NaiveDate>,
) -> Vec<UpstreamVersion> {
    let existing: HashSet<String> = index.available_versions().into_iter().collect();
    let mut new_ones: Vec<UpstreamVersion> = upstream
        .iter()
        .filter(|v| !existing.contains(&v.version))
        .filter(|v| match cutoff {
            // Strictly newer than the latest cataloged release date.
            // Equal dates are excluded — same-day re-publishes are
            // almost always already-cataloged versions.
            Some(c) => v.released_at.date_naive() > c,
            None => true,
        })
        .cloned()
        .collect();
    new_ones.sort_by_key(|v| v.released_at);
    new_ones
}

pub(crate) fn which_ajv() -> bool {
    validate::npx()
        .args(["--no-install", "ajv", "--version"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn score_str(score: activity::Score) -> &'static str {
    match score {
        activity::Score::VeryActive => "very-active",
        activity::Score::Active => "active",
        activity::Score::Maintained => "maintained",
        activity::Score::Slow => "slow",
        activity::Score::Stale => "stale",
        activity::Score::Dormant => "dormant",
        activity::Score::Abandoned => "abandoned",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;

    fn git(p: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .current_dir(p)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    /// Temp repo on `main` with a one-tool catalog committed. Panics on an
    /// inherited `GIT_DIR`, which would point `git` at another repo.
    fn catalog_repo() -> tempfile::TempDir {
        assert!(
            std::env::var_os("GIT_DIR").is_none() && std::env::var_os("GIT_WORK_TREE").is_none(),
            "unset GIT_DIR/GIT_WORK_TREE: the temp-repo tests would act on another repo"
        );
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "-q", "-b", "main"]);
        git(p, &["config", "user.name", "t"]);
        git(p, &["config", "user.email", "t@example.com"]);
        write_tool(p, None, TEMPLATE);
        git(p, &["add", "."]);
        git(p, &["commit", "-q", "-m", "init"]);
        dir
    }

    /// A version file the rules accept.
    const TEMPLATE: &str = r#"{
        "schemaVersion": 1, "tool": "foo", "version": "0.9.0",
        "install_methods": [
            { "name": "x",
              "verification": { "tier": 4, "algorithm": "sha256", "tofu": true },
              "source_url_template": "https://x/0.9.0" }
        ]
    }"#;

    /// A version file the rules reject: its dependency names no tool.
    const RULE_BREAKING_TEMPLATE: &str = r#"{
        "schemaVersion": 1, "tool": "foo", "version": "0.9.0",
        "install_methods": [
            { "name": "x",
              "verification": { "tier": 4, "algorithm": "sha256", "tofu": true },
              "source_url_template": "https://x/0.9.0",
              "dependencies": [ { "tool": "ghost" } ] }
        ]
    }"#;

    /// `tools/foo` with one cataloged version, 0.9.0, whose file is
    /// `template`, and an optional `source_repo`.
    fn write_tool(p: &Path, source_repo: Option<&str>, template: &str) {
        write_tool_as(p, "foo", source_repo, template);
    }

    /// [`write_tool`] for tool `id`.
    fn write_tool_as(p: &Path, id: &str, source_repo: Option<&str>, template: &str) {
        let tool = p.join("tools").join(id);
        std::fs::create_dir_all(tool.join("versions")).unwrap();
        let template = template.replace("\"tool\": \"foo\"", &format!("\"tool\": \"{id}\""));
        let mut index = json!({
            "schemaVersion": 1, "id": id, "kind": "cli",
            "available": [ { "version": "0.9.0" } ],
        });
        if let Some(repo) = source_repo {
            index["source_repo"] = repo.into();
        }
        std::fs::write(tool.join("index.json"), index.to_string()).unwrap();
        std::fs::write(tool.join("versions").join("0.9.0.json"), template).unwrap();
    }

    /// The plan the staging job would produce for adding 1.0.0 to `p`'s
    /// catalog: files rendered from the checkout, so it passes re-render.
    fn rendered_plan(p: &Path) -> Plan {
        let kind = Kind::Scan {
            date: NaiveDate::from_ymd_opt(2026, 10, 17).unwrap(),
        };
        rendered_plan_for(p, "foo", kind, "1.0.0")
    }

    /// The plan the staging job would produce for adding `version` to tool
    /// `id` in `p`'s catalog.
    fn rendered_plan_for(p: &Path, id: &str, kind: Kind, version: &str) -> Plan {
        let releases = vec![UpstreamVersion {
            version: version.into(),
            released_at: at(2026, 10, 1),
            channel: "stable".into(),
        }];
        let activity = Activity {
            score: activity::Score::Active,
            cadence_days: 7,
            signals: activity::ActivitySignals::default(),
        };
        let now = at(2026, 10, 17);
        let files = stage::render_files(p, id, &kind, &releases, &activity, now).unwrap();
        Plan {
            tool_id: id.into(),
            kind,
            new_versions: vec![version.into()],
            releases,
            now,
            activity,
            files,
        }
    }

    #[test]
    fn a_staged_tool_that_fails_its_gate_is_recorded_and_the_tree_restored() {
        let repo = catalog_repo();
        let p = repo.path();
        write_tool(p, None, RULE_BREAKING_TEMPLATE);
        git(p, &["commit", "-qam", "break the template"]);
        let mut manifest = Manifest::new(pr::head_sha(p).unwrap());
        let ok = apply_with_failure_reporting(
            p,
            "foo",
            Ok(Scanned::Plan(rendered_plan(p))),
            Mode::Stage(&mut manifest),
            at(2026, 10, 17),
        );
        assert!(!ok);
        // Recorded for the token job to file, not filed here (no `gh` call).
        assert!(manifest.plans.is_empty());
        assert_eq!(manifest.failures.len(), 1);
        assert_eq!(manifest.failures[0].tool_id, "foo");
        assert!(
            manifest.failures[0].error.contains("ghost"),
            "got {:?}",
            manifest.failures[0].error
        );
        assert_eq!(git(p, &["status", "--porcelain"]), "");
    }

    #[test]
    fn a_failed_scan_is_staged_as_a_failure() {
        let repo = catalog_repo();
        let mut manifest = Manifest::new("abc".into());
        let ok = apply_with_failure_reporting(
            repo.path(),
            "foo",
            Err(anyhow::anyhow!("upstream 502")),
            Mode::Stage(&mut manifest),
            at(2026, 10, 17),
        );
        assert!(!ok);
        assert_eq!(
            manifest.failures,
            vec![Failure {
                tool_id: "foo".into(),
                error: "upstream 502".into(),
            }]
        );
    }

    #[test]
    fn deliver_refuses_a_plan_staged_at_another_commit() {
        let repo = catalog_repo();
        let p = repo.path();
        let head = pr::head_sha(p).unwrap();
        let mut manifest = Manifest::new("0".repeat(40));
        manifest.plans.push(rendered_plan(p));
        manifest.failures.push(Failure {
            tool_id: "foo".into(),
            error: "boom".into(),
        });
        let err = deliver_refused(p, &manifest);
        assert_eq!(
            err.to_string(),
            format!(
                "plan was staged at {} but the checkout is at {head}; refusing to deliver",
                "0".repeat(40)
            )
        );
        // Refused before writing a plan's files or filing its failures.
        assert_eq!(git(p, &["status", "--porcelain"]), "");
    }

    #[test]
    fn a_bad_staged_failure_files_nothing_but_still_fails_the_run() {
        // Refused before `gh` is called (no issue for a tool the checkout
        // doesn't have), yet the run still ends non-zero naming it.
        let repo = catalog_repo();
        let p = repo.path();
        let manifest = {
            let mut m = Manifest::new(pr::head_sha(p).unwrap());
            m.failures.push(Failure {
                tool_id: "ghost".into(),
                error: "@everyone".into(),
            });
            m
        };
        let err = deliver_refused(p, &manifest);
        assert_eq!(err.to_string(), "scan failed for: \"ghost\"");
        assert_eq!(git(p, &["status", "--porcelain"]), "");
    }

    /// A PR step for a test whose plan must be refused before `gh`.
    fn no_pr(req: &stage::PrRequest) -> anyhow::Result<pr::Delivery> {
        panic!("refused plan reached the PR step: {req:?}");
    }

    /// [`stage::deliver_manifest`] for a manifest whose every entry must be
    /// refused: filing an issue or reaching the PR step panics instead of
    /// calling `gh`.
    fn deliver_refused(p: &Path, manifest: &Manifest) -> anyhow::Error {
        stage::deliver_manifest_with(
            p,
            manifest,
            |tool, error| panic!("refused entry was reported: {tool}: {error}"),
            no_pr,
            None,
        )
        .unwrap_err()
    }

    #[test]
    fn delivery_files_failures_and_delivers_every_other_plan() {
        // A staged failure, a plan whose PR step fails, then a backfill
        // that delivers: each is handled and the loop keeps going.
        let repo = catalog_repo();
        let p = repo.path();
        write_tool_as(p, "bar", None, TEMPLATE);
        write_tool_as(p, "baz", None, TEMPLATE);
        git(p, &["add", "."]);
        git(p, &["commit", "-q", "-m", "bar, baz"]);
        let mut manifest = Manifest::new(pr::head_sha(p).unwrap());
        manifest.failures.push(Failure {
            tool_id: "baz".into(),
            error: "upstream 502".into(),
        });
        manifest.plans.push(rendered_plan(p));
        let backfill = Kind::Backfill {
            from: "0.5.0".into(),
        };
        manifest
            .plans
            .push(rendered_plan_for(p, "bar", backfill, "0.5.0"));

        let mut reports = Vec::new();
        let mut requests = Vec::new();
        // Outside the checkout, so the clean-tree check below still holds.
        let summary_dir = tempfile::tempdir().unwrap();
        let summary = summary_dir.path().join("summary.md");
        let err = stage::deliver_manifest_with(
            p,
            &manifest,
            |tool, error| reports.push((tool.to_string(), error.to_string())),
            |req| {
                requests.push(req.clone());
                // What pr::deliver leaves behind either way: a clean base.
                pr::discard_generated(p, &req.tool_id)?;
                match req.tool_id.as_str() {
                    "foo" => anyhow::bail!("gh pr create failed: 502"),
                    _ => Ok(pr::Delivery::Opened("https://pr/1".into())),
                }
            },
            Some(&summary),
        )
        .unwrap_err();

        assert_eq!(
            reports,
            [
                ("baz".to_string(), "upstream 502".to_string()),
                ("foo".to_string(), "gh pr create failed: 502".to_string()),
            ]
        );
        let tools: Vec<&str> = requests.iter().map(|r| r.tool_id.as_str()).collect();
        assert_eq!(tools, ["foo", "bar"]);
        let foo = &requests[0];
        assert_eq!(foo.prefix, "scan/foo/");
        assert_eq!(foo.branch, "scan/foo/2026-10-17");
        let bar = &requests[1];
        assert_eq!(bar.prefix, "backfill/bar/");
        assert_eq!(bar.branch, "backfill/bar/0.5.0");
        assert_eq!(bar.title, "feat(bar): backfill versions 0.5.0 to 0.5.0");
        assert_eq!(bar.new_versions, ["0.5.0"]);
        // Staged failures are named debug-escaped, delivery failures raw.
        assert_eq!(err.to_string(), "scan failed for: \"baz\", foo");
        // Only the delivered plan reaches the job summary.
        assert_eq!(
            std::fs::read_to_string(&summary).unwrap(),
            "- `bar`: opened https://pr/1\n"
        );
        assert_eq!(git(p, &["status", "--porcelain"]), "");
    }

    #[test]
    fn delivery_stops_before_the_next_plan_once_off_base() {
        // A PR step that fails and leaves the checkout on another branch
        // must stop the loop: the next plan's branch would be cut from the
        // wrong commit.
        let repo = catalog_repo();
        let p = repo.path();
        write_tool_as(p, "bar", None, TEMPLATE);
        write_tool_as(p, "baz", None, TEMPLATE);
        git(p, &["add", "."]);
        git(p, &["commit", "-q", "-m", "bar, baz"]);
        let mut manifest = Manifest::new(pr::head_sha(p).unwrap());
        manifest.failures.push(Failure {
            tool_id: "baz".into(),
            error: "upstream 502".into(),
        });
        manifest.plans.push(rendered_plan(p));
        let backfill = Kind::Backfill {
            from: "0.5.0".into(),
        };
        manifest
            .plans
            .push(rendered_plan_for(p, "bar", backfill, "0.5.0"));

        let mut reports = Vec::new();
        let mut requests = Vec::new();
        let summary_dir = tempfile::tempdir().unwrap();
        let summary = summary_dir.path().join("summary.md");
        let err = stage::deliver_manifest_with(
            p,
            &manifest,
            |tool, error| reports.push((tool.to_string(), error.to_string())),
            |req| {
                requests.push(req.tool_id.clone());
                pr::discard_generated(p, &req.tool_id)?;
                git(p, &["checkout", "-q", "-b", "stranded"]);
                anyhow::bail!("return checkout failed")
            },
            Some(&summary),
        )
        .unwrap_err();

        assert_eq!(requests, ["foo"]);
        assert_eq!(
            reports,
            [
                ("baz".to_string(), "upstream 502".to_string()),
                ("foo".to_string(), "return checkout failed".to_string()),
            ]
        );
        assert_eq!(
            err.to_string(),
            "aborted before `bar`; earlier failures: [\"baz\", foo]"
        );
        assert!(!summary.exists());
    }

    #[test]
    fn start_is_a_no_op_without_stage() {
        // Not a repo: reaching head_sha (or ajv) would fail.
        let dir = tempfile::tempdir().unwrap();
        let args = RunArgs {
            dry_run: false,
            stage: None,
        };
        assert_eq!(stage::start(dir.path(), &args).unwrap(), None);
    }

    /// Run the `#[ignore]`d test `name` in a child of this test binary with
    /// an empty environment and `PATH` set to an empty directory, so no
    /// `npx`/`gh` and no token. `PATH` is set, not just cleared: with it
    /// unset, libc falls back to a default search path (`/bin:/usr/bin`)
    /// that may well hold them. Asserts that exactly that one test ran and
    /// passed.
    fn run_without_path(name: &str) {
        let empty = tempfile::tempdir().unwrap();
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--ignored", "--test-threads=1"])
            .env_clear()
            .env("PATH", empty.path())
            .env(CHILD_MARKER, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && stdout.contains("test result: ok. 1 passed"),
            "child {name}: {out:?}"
        );
    }

    /// Set only in a [`run_without_path`] child.
    const CHILD_MARKER: &str = "SCANNER_TEST_NO_PATH_CHILD";

    fn in_child() -> bool {
        std::env::var_os(CHILD_MARKER).is_some()
    }

    #[test]
    fn stage_refuses_to_start_without_ajv() {
        run_without_path("tests::no_path_stage_start_needs_ajv");
    }

    #[test]
    #[ignore = "run by stage_refuses_to_start_without_ajv"]
    fn no_path_stage_start_needs_ajv() {
        if !in_child() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let args = RunArgs {
            dry_run: false,
            stage: Some(dir.path().join("plan.json")),
        };
        let err = stage::start(dir.path(), &args).unwrap_err();
        assert_eq!(
            err.to_string(),
            "--stage needs `npx ajv` (ajv-cli + ajv-formats) to validate the plans it writes"
        );
    }

    #[test]
    fn the_schema_gate_is_mandatory_only_when_required() {
        run_without_path("tests::no_path_write_and_gate_schemas");
    }

    #[test]
    #[ignore = "run by the_schema_gate_is_mandatory_only_when_required"]
    fn no_path_write_and_gate_schemas() {
        if !in_child() {
            return;
        }
        // No git either: a plain catalog dir, built per case.
        let cases = [
            (Schemas::Skip, true),
            (Schemas::IfAvailable, true),
            (Schemas::Require, false),
        ];
        for (schemas, ok) in cases {
            let dir = tempfile::tempdir().unwrap();
            let p = dir.path();
            write_tool(p, None, TEMPLATE);
            let result = stage::write_and_gate(p, &rendered_plan(p), schemas);
            match (ok, result) {
                (true, Ok(link)) => assert_eq!(link, None),
                (false, Err(e)) => assert!(
                    format!("{e:#}").starts_with("running `npx ajv validate"),
                    "{schemas:?}: got {e:#}"
                ),
                (_, r) => panic!("{schemas:?}: got {r:?}"),
            }
        }
    }

    #[test]
    fn the_pr_upstream_link_comes_from_the_checkout_not_the_plan() {
        // write_and_gate reads the link before writing the plan's index, so
        // even an index the plan controls can't change it.
        let repo = catalog_repo();
        let p = repo.path();
        write_tool(p, Some("https://github.com/o/foo"), TEMPLATE);
        let mut plan = rendered_plan(p);
        let index = plan.files.last_mut().unwrap();
        assert_eq!(index.path, "tools/foo/index.json");
        index.contents = index.contents.replace(
            "https://github.com/o/foo",
            "[click](https://evil.test) @everyone",
        );
        let link = stage::write_and_gate(p, &plan, Schemas::Skip).unwrap();
        assert_eq!(link.as_deref(), Some("https://github.com/o/foo"));
        let on_disk = std::fs::read_to_string(p.join("tools/foo/index.json")).unwrap();
        assert!(on_disk.contains("evil.test"), "got {on_disk}");
    }

    #[test]
    fn delivery_refuses_files_that_differ_from_the_checkouts_render() {
        // The attack the re-render closes: the staging job swaps an install
        // URL (or anything else) in a file whose path and shape are fine.
        let repo = catalog_repo();
        let p = repo.path();
        let mut plan = rendered_plan(p);
        plan.check().unwrap();
        let version = &mut plan.files[0];
        assert_eq!(version.path, "tools/foo/versions/1.0.0.json");
        let swapped = version
            .contents
            .replace("https://x/0.9.0", "https://evil.test/x");
        assert_ne!(swapped, version.contents, "the swap must change the file");
        version.contents = swapped;
        let err = stage::deliver_plan(p, &plan, no_pr).unwrap_err();
        assert_eq!(
            err.to_string(),
            "plan for `foo`: staged files differ from what this checkout renders for its releases"
        );
        assert_eq!(git(p, &["status", "--porcelain"]), "");
    }

    #[test]
    fn a_plan_for_an_unknown_tool_files_nothing_but_fails_the_run() {
        // Vetted before base.guard and before `gh`: no issue or PR for a
        // tool id the checkout doesn't have. Also stops before `gh` here.
        let repo = catalog_repo();
        let p = repo.path();
        let mut manifest = Manifest::new(pr::head_sha(p).unwrap());
        for id in ["ghost", "@everyone"] {
            let mut plan = rendered_plan(p);
            plan.tool_id = id.into();
            manifest.plans.push(plan);
        }
        let err = deliver_refused(p, &manifest);
        assert_eq!(err.to_string(), "scan failed for: \"ghost\", \"@everyone\"");
        assert_eq!(git(p, &["status", "--porcelain"]), "");
    }

    #[test]
    fn delivery_refuses_to_overwrite_a_cataloged_version() {
        let repo = catalog_repo();
        let p = repo.path();
        let plan = rendered_plan(p);
        let original = "{\"version\": \"1.0.0\"}\n";
        std::fs::write(p.join("tools/foo/versions/1.0.0.json"), original).unwrap();
        git(p, &["add", "."]);
        git(p, &["commit", "-q", "-m", "catalog 1.0.0"]);
        let err = stage::deliver_plan(p, &plan, no_pr).unwrap_err();
        assert_eq!(
            err.to_string(),
            "plan for `foo`: refusing to overwrite already-cataloged version(s) [\"1.0.0\"]"
        );
        assert_eq!(git(p, &["status", "--porcelain"]), "");
        assert_eq!(
            std::fs::read_to_string(p.join("tools/foo/versions/1.0.0.json")).unwrap(),
            original
        );
    }

    #[test]
    fn delivery_writes_nothing_for_a_plan_outside_its_tool() {
        let repo = catalog_repo();
        let p = repo.path();
        let mut plan = rendered_plan(p);
        plan.files[0].path = ".github/workflows/scan.yml".into();
        let err = stage::deliver_plan(p, &plan, no_pr).unwrap_err();
        assert!(
            err.to_string()
                .starts_with("plan for `foo`: refusing to write"),
            "got {err:#}"
        );
        assert_eq!(git(p, &["status", "--porcelain"]), "");
    }

    #[test]
    fn delivery_restores_the_tree_when_a_plan_fails_the_rules() {
        // A genuine render of a checkout whose template breaks the rules:
        // it passes re-render, writes, and fails the gate.
        let repo = catalog_repo();
        let p = repo.path();
        write_tool(p, None, RULE_BREAKING_TEMPLATE);
        git(p, &["commit", "-qam", "break the template"]);
        let err = stage::deliver_plan(p, &rendered_plan(p), no_pr).unwrap_err();
        assert!(format!("{err:#}").contains("ghost"), "got {err:#}");
        assert_eq!(git(p, &["status", "--porcelain"]), "");
    }

    #[test]
    fn stage_and_dry_run_flags_are_exclusive() {
        let err =
            Cli::try_parse_from(["scanner", "due", "--dry-run", "--stage", "p.json"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
        let cli = Cli::try_parse_from([
            "scanner", "backfill", "rust", "--from", "1.82.0", "--stage", "p.json",
        ])
        .unwrap();
        let Command::Backfill { run, .. } = cli.command else {
            panic!("expected backfill");
        };
        assert_eq!(run.stage.as_deref(), Some(Path::new("p.json")));
        assert!(!run.dry_run);
    }

    fn at(year: i32, month: u32, day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, 0, 0, 0).unwrap()
    }

    fn make_index() -> ToolIndex {
        ToolIndex {
            id: "rust".to_string(),
            source_repo: None,
            raw: json!({
                "available": [
                    { "version": "1.93.0" },
                    { "version": "1.94.0" },
                    { "version": "1.95.0" }
                ]
            }),
            path: PathBuf::from("/tmp/index.json"),
        }
    }

    #[test]
    fn fail_if_any_passes_a_clean_run() {
        assert!(fail_if_any(&[]).is_ok());
    }

    #[test]
    fn fail_if_any_names_every_failed_tool() {
        let err = fail_if_any(&["rust".to_string(), "gcc".to_string()]).unwrap_err();
        assert_eq!(err.to_string(), "scan failed for: rust, gcc");
    }

    #[test]
    fn diff_new_versions_skips_existing_and_sorts_by_release_date() {
        let upstream = vec![
            UpstreamVersion {
                version: "1.95.0".to_string(),
                released_at: at(2026, 4, 16),
                channel: "stable".to_string(),
            },
            UpstreamVersion {
                version: "1.96.0".to_string(),
                released_at: at(2026, 5, 28),
                channel: "stable".to_string(),
            },
            UpstreamVersion {
                version: "1.95.1".to_string(),
                released_at: at(2026, 4, 30),
                channel: "stable".to_string(),
            },
        ];
        let new = diff_new_versions(&make_index(), &upstream, None);
        let ids: Vec<&str> = new.iter().map(|v| v.version.as_str()).collect();
        assert_eq!(ids, vec!["1.95.1", "1.96.0"]);
    }

    #[test]
    fn diff_new_versions_applies_release_date_cutoff() {
        // Regression test for the live dry-run finding: upstream lists
        // every version it ever shipped, and without a cutoff the
        // scanner would treat every pre-Jan-2026 release as "new".
        // Cutoff = latest cataloged release date; entries on or before
        // the cutoff are filtered out.
        let cutoff = NaiveDate::from_ymd_opt(2026, 4, 16).unwrap();
        let upstream = vec![
            UpstreamVersion {
                version: "1.30.0".to_string(),
                released_at: at(2018, 10, 25),
                channel: "stable".to_string(),
            },
            UpstreamVersion {
                version: "1.94.5".to_string(),
                released_at: at(2026, 4, 17),
                channel: "stable".to_string(),
            },
            UpstreamVersion {
                version: "1.96.0".to_string(),
                released_at: at(2026, 5, 28),
                channel: "stable".to_string(),
            },
        ];
        let new = diff_new_versions(&make_index(), &upstream, Some(cutoff));
        let ids: Vec<&str> = new.iter().map(|v| v.version.as_str()).collect();
        assert_eq!(ids, vec!["1.94.5", "1.96.0"]);
    }
}
