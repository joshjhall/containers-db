//! Daily catalog scanner for containers-db.
//!
//! Walks each tool's `index.json`, decides which are due based on
//! `activity.scan_cadence_days`, dispatches to a per-tool
//! [`adapter::Adapter`] for upstream signals and new versions,
//! recomputes activity, generates `versions/<v>.json` files, and opens
//! one PR per tool. Failures surface as issues so a broken adapter
//! never produces malformed catalog data.

pub mod activity;
pub mod adapter;
pub mod adapters;
pub mod due;
pub mod failure;
pub mod generate;
pub mod pr;
pub mod validate;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, NaiveDate, Utc};
use clap::Parser;
use serde_json::Value;

use crate::activity::compute_tier;
use crate::adapter::{Adapter, ToolIndex, UpstreamVersion};

#[derive(Debug, Parser)]
#[command(name = "scanner", about = "containers-db catalog scanner")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, clap::Subcommand)]
pub enum Command {
    /// Scan every tool whose `activity.scanned_at + scan_cadence_days`
    /// is in the past.
    Due {
        /// Print the plan but don't write files or open PRs/issues.
        #[arg(long)]
        dry_run: bool,
    },
    /// Force-scan a single tool by id (skips the cadence check).
    Tool {
        /// Tool id, e.g. `rust`.
        id: String,
        /// Print the plan but don't write files or open PRs/issues.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Debug)]
pub enum Outcome {
    /// Adapter ran successfully but no new versions were found. The
    /// activity block is *not* written back in this case — the next
    /// scheduled run will reattempt.
    NoChange,
    /// `--dry-run` printed the plan and exited without writing anything.
    DryRun { new_versions: Vec<String> },
    /// One PR was opened with new version files + a refreshed
    /// activity block.
    PrOpened {
        url: String,
        new_versions: Vec<String>,
    },
    /// Tool has no registered adapter — the orchestrator skipped it.
    NoAdapter,
}

pub async fn run(cli: Cli) -> anyhow::Result<()> {
    let repo_root = std::env::current_dir()?;
    let registry = adapters::register_all();
    let now = Utc::now();

    match cli.command {
        Command::Due { dry_run } => {
            let due_tools = due::find_due_tools(&repo_root, now)?;
            if due_tools.is_empty() {
                println!("scanner: no tools due for scanning");
                return Ok(());
            }
            for tool in due_tools {
                let Some(adapter) = registry.get(&tool.id).cloned() else {
                    println!("scanner: {}: no adapter registered, skipping", tool.id);
                    continue;
                };
                scan_one_with_failure_reporting(&repo_root, &tool.id, adapter, now, dry_run).await;
            }
        }
        Command::Tool { id, dry_run } => {
            let adapter = registry
                .get(&id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no adapter registered for `{id}`"))?;
            scan_one_with_failure_reporting(&repo_root, &id, adapter, now, dry_run).await;
        }
    }
    Ok(())
}

/// Wrap [`scan_one`] so any error surfaces as a tracking issue (unless
/// we're in dry-run mode). The orchestrator continues to the next tool
/// — one broken adapter shouldn't take down the whole scan.
async fn scan_one_with_failure_reporting(
    repo_root: &Path,
    tool_id: &str,
    adapter: Arc<dyn Adapter>,
    now: DateTime<Utc>,
    dry_run: bool,
) {
    match scan_one(repo_root, tool_id, adapter, now, dry_run).await {
        Ok(outcome) => println!("scanner: {tool_id}: {outcome:?}"),
        Err(e) => {
            eprintln!("scanner: {tool_id} FAILED: {e:#}");
            if !dry_run
                && let Err(report_err) =
                    failure::report_failure(repo_root, tool_id, &format!("{e:#}"), now)
            {
                eprintln!("scanner: {tool_id}: failure-reporting itself failed: {report_err:#}");
            }
        }
    }
}

async fn scan_one(
    repo_root: &Path,
    tool_id: &str,
    adapter: Arc<dyn Adapter>,
    now: DateTime<Utc>,
    dry_run: bool,
) -> anyhow::Result<Outcome> {
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
        return Ok(Outcome::NoChange);
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

    if dry_run {
        println!(
            "scanner: {tool_id} (dry-run): {} new version(s)",
            new_versions.len()
        );
        for v in &new_version_strings {
            println!("  + {v}");
        }
        println!("  score={}, cadence={cadence_days}d", score_str(score));
        return Ok(Outcome::DryRun {
            new_versions: new_version_strings,
        });
    }

    write_file(&index.path, &index.raw)?;
    for (path, value) in &generated {
        write_file(path, value)?;
    }

    // In-process semantic gate (cheap, hermetic).
    validate::validate_rules(repo_root)?;
    // Schema gate (shells out to ajv). Best effort — if ajv isn't on
    // PATH (e.g., dev machine without node), the in-process pass and
    // the CI workflow's separate ajv step are the safety nets.
    if which_ajv() {
        validate::validate_schemas(repo_root)?;
    }

    let branch = pr::branch_name(tool_id, now);
    let title = pr::pr_title(tool_id, &new_version_strings);
    let body = pr::pr_body(
        tool_id,
        &new_version_strings,
        index.source_repo.as_deref(),
        score,
        &result.signals,
        cadence_days,
    );
    let url = pr::open_pr(repo_root, tool_id, &branch, &title, &body, "main")?;

    Ok(Outcome::PrOpened {
        url,
        new_versions: new_version_strings,
    })
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

fn write_file(path: &Path, value: &Value) -> anyhow::Result<()> {
    let pretty = serde_json::to_string_pretty(value)?;
    let mut bytes = pretty.into_bytes();
    bytes.push(b'\n');
    std::fs::write(path, bytes).map_err(|e| anyhow::anyhow!("writing {}: {e}", path.display()))
}

fn which_ajv() -> bool {
    std::process::Command::new("npx")
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
