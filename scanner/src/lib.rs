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
    let mut index = ToolIndex::from_path(&index_path)?;

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

    let mut generated: Vec<(PathBuf, Value)> = Vec::new();
    for nv in &new_versions {
        let path = repo_root
            .join("tools")
            .join(tool_id)
            .join("versions")
            .join(format!("{}.json", nv.version));
        generated.push((
            path,
            generate::render_version(&template, &template_version, nv, now),
        ));
    }

    let new_version_strings: Vec<String> = new_versions.iter().map(|v| v.version.clone()).collect();
    generate::update_index(
        &mut index,
        &new_version_strings,
        &result.signals,
        score,
        cadence_days,
        now,
    );
    // Index first, as the single-process path always wrote it.
    generated.insert(0, (index.path.clone(), index.raw.clone()));

    Ok(Scanned::Plan(Plan {
        tool_id: tool_id.to_string(),
        kind: Kind::Scan {
            date: now.date_naive(),
        },
        activity: Activity {
            score,
            cadence_days,
            signals: result.signals,
        },
        files: stage::staged_files(repo_root, &generated)?,
        new_versions: new_version_strings,
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
    use crate::stage::StagedFile;
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
        let tool = p.join("tools").join("foo");
        std::fs::create_dir_all(tool.join("versions")).unwrap();
        std::fs::write(
            tool.join("index.json"),
            r#"{ "schemaVersion": 1, "id": "foo", "kind": "cli" }"#,
        )
        .unwrap();
        git(p, &["add", "."]);
        git(p, &["commit", "-q", "-m", "init"]);
        dir
    }

    /// A plan for `foo` whose version file references an unknown tool, so
    /// the in-process rules reject it before ajv would ever run.
    fn rule_breaking_plan() -> Plan {
        Plan {
            tool_id: "foo".into(),
            kind: Kind::Scan {
                date: NaiveDate::from_ymd_opt(2026, 10, 17).unwrap(),
            },
            new_versions: vec!["1.0.0".into()],
            activity: Activity {
                score: activity::Score::Active,
                cadence_days: 7,
                signals: activity::ActivitySignals::default(),
            },
            files: vec![
                StagedFile {
                    path: "tools/foo/index.json".into(),
                    contents: r#"{ "schemaVersion": 1, "id": "foo", "kind": "cli" }"#.into(),
                },
                StagedFile {
                    path: "tools/foo/versions/1.0.0.json".into(),
                    contents: r#"{
                    "schemaVersion": 1, "tool": "foo", "version": "1.0.0",
                    "install_methods": [
                        { "name": "x",
                          "verification": { "tier": 4, "algorithm": "sha256", "tofu": true },
                          "source_url_template": "https://x",
                          "dependencies": [ { "tool": "ghost" } ] }
                    ]
                }"#
                    .into(),
                },
            ],
        }
    }

    #[test]
    fn a_staged_tool_that_fails_its_gate_is_recorded_and_the_tree_restored() {
        let repo = catalog_repo();
        let p = repo.path();
        let mut manifest = Manifest::new(pr::head_sha(p).unwrap());
        let ok = apply_with_failure_reporting(
            p,
            "foo",
            Ok(Scanned::Plan(rule_breaking_plan())),
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
        manifest.plans.push(rule_breaking_plan());
        manifest.failures.push(Failure {
            tool_id: "foo".into(),
            error: "boom".into(),
        });
        let err = stage::deliver_manifest(p, &manifest, at(2026, 10, 17)).unwrap_err();
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
    fn deliver_refuses_a_staged_failure_for_an_unknown_tool() {
        // Checked before `gh` is ever called: no issue is filed for a tool
        // the checkout doesn't have, and no plan after it is delivered.
        let repo = catalog_repo();
        let p = repo.path();
        let mut manifest = Manifest::new(pr::head_sha(p).unwrap());
        manifest.failures.push(Failure {
            tool_id: "ghost".into(),
            error: "@everyone".into(),
        });
        manifest.plans.push(rule_breaking_plan());
        let err = stage::deliver_manifest(p, &manifest, at(2026, 10, 17)).unwrap_err();
        assert_eq!(
            err.to_string(),
            "staged failure names `ghost`, which has no tools/ghost/index.json"
        );
        assert_eq!(git(p, &["status", "--porcelain"]), "");
    }

    #[test]
    fn the_pr_upstream_link_comes_from_the_checkout_not_the_plan() {
        let repo = catalog_repo();
        let p = repo.path();
        std::fs::write(
            p.join("tools/foo/index.json"),
            r#"{ "schemaVersion": 1, "id": "foo", "kind": "cli", "source_repo": "https://github.com/o/foo" }"#,
        )
        .unwrap();
        let mut plan = rule_breaking_plan();
        // A version file the rules accept, so the gate passes and the
        // returned link is what delivery would put in the PR body.
        plan.files[1].contents = r#"{
            "schemaVersion": 1, "tool": "foo", "version": "1.0.0",
            "install_methods": [
                { "name": "x",
                  "verification": { "tier": 4, "algorithm": "sha256", "tofu": true },
                  "source_url_template": "https://x" }
            ]
        }"#
        .into();
        plan.files[0].contents = r#"{ "schemaVersion": 1, "id": "foo", "kind": "cli", "source_repo": "[click](https://evil.test) @everyone" }"#.into();
        let link = stage::write_and_gate(p, &plan, Schemas::Skip).unwrap();
        assert_eq!(link.as_deref(), Some("https://github.com/o/foo"));
        // The plan's index was written; only the link stayed the base's.
        let on_disk = std::fs::read_to_string(p.join("tools/foo/index.json")).unwrap();
        assert!(on_disk.contains("evil.test"), "got {on_disk}");
    }

    #[test]
    fn delivery_writes_nothing_for_a_plan_outside_its_tool() {
        let repo = catalog_repo();
        let p = repo.path();
        let mut plan = rule_breaking_plan();
        plan.files[1].path = ".github/workflows/scan.yml".into();
        let err = stage::deliver_plan(p, &plan).unwrap_err();
        assert!(
            err.to_string()
                .starts_with("plan for `foo`: refusing to write"),
            "got {err:#}"
        );
        assert_eq!(git(p, &["status", "--porcelain"]), "");
    }

    #[test]
    fn delivery_restores_the_tree_when_a_plan_fails_the_rules() {
        let repo = catalog_repo();
        let p = repo.path();
        let err = stage::deliver_plan(p, &rule_breaking_plan()).unwrap_err();
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
