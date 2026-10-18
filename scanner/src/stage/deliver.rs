//! Staging and delivery I/O: render a plan's files, write and gate them,
//! and push / open PRs from a vetted [`Manifest`].

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;

use super::plan::{
    Activity, Kind, Manifest, Plan, StagedFile, check_additive, check_known_tool, check_rerender,
};
use crate::adapter::{ToolIndex, UpstreamVersion};
use crate::pr::{Delivery, PrRequest};
use crate::{Outcome, RunArgs, generate, outcome, pr, validate, which_ajv};

/// Which schema gate [`write_and_gate`] runs after the in-process rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Schemas {
    /// Delivery: ajv must not run in the token job.
    Skip,
    /// A local one-shot run: ajv if it is installed.
    IfAvailable,
    /// Staging: the token job will not re-check, so ajv is mandatory.
    Require,
}

/// The exact bytes [`crate::write_file`] writes for `value`.
pub(crate) fn render(value: &Value) -> Result<String> {
    let mut s = serde_json::to_string_pretty(value)?;
    s.push('\n');
    Ok(s)
}

/// Turn absolute-path `(path, value)` pairs under `repo_root` into
/// [`StagedFile`]s.
pub(crate) fn staged_files(
    repo_root: &Path,
    files: &[(PathBuf, Value)],
) -> Result<Vec<StagedFile>> {
    files
        .iter()
        .map(|(path, value)| {
            let rel = path.strip_prefix(repo_root).with_context(|| {
                format!("{} is outside {}", path.display(), repo_root.display())
            })?;
            let parts: Vec<String> = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            Ok(StagedFile {
                path: parts.join("/"),
                contents: render(value)?,
            })
        })
        .collect()
}

/// The template a plan's version files are cloned from, as the checkout
/// has it: the newest cataloged version for a scan, the oldest for a
/// backfill (whose support matrix best describes older releases).
fn template_version(index: &ToolIndex, kind: &Kind) -> Option<String> {
    let mut existing = index.available_versions();
    match kind {
        Kind::Scan { .. } => existing.pop(),
        Kind::Backfill { .. } => {
            existing.sort_by_cached_key(|v| generate::version_key(Some(v)));
            existing.into_iter().next()
        }
    }
}

/// Render a plan's files from the checkout at `repo_root`: each version
/// file from the tool's template, then the index with the new versions and
/// activity. Version files first, index last. The staging job builds its
/// plan's `files` with this, and the token job calls it again rather than
/// trusting those bytes, so both sides run one code path.
pub(crate) fn render_files(
    repo_root: &Path,
    tool_id: &str,
    kind: &Kind,
    releases: &[UpstreamVersion],
    activity: &Activity,
    now: DateTime<Utc>,
) -> Result<Vec<StagedFile>> {
    let tool_dir = repo_root.join("tools").join(tool_id);
    let mut index = ToolIndex::from_path(&tool_dir.join("index.json"))?;
    let template_version = template_version(&index, kind).ok_or_else(|| {
        anyhow::anyhow!("tool `{tool_id}` has no existing versions to template from")
    })?;
    let template_path = tool_dir
        .join("versions")
        .join(format!("{template_version}.json"));
    let template_bytes = std::fs::read(&template_path)
        .map_err(|e| anyhow::anyhow!("reading template {}: {e}", template_path.display()))?;
    let template: Value = serde_json::from_slice(&template_bytes)
        .map_err(|e| anyhow::anyhow!("parsing template {}: {e}", template_path.display()))?;

    let mut generated: Vec<(PathBuf, Value)> = releases
        .iter()
        .map(|nv| {
            (
                tool_dir
                    .join("versions")
                    .join(format!("{}.json", nv.version)),
                generate::render_version(&template, &template_version, nv, now),
            )
        })
        .collect();
    let versions: Vec<String> = releases.iter().map(|r| r.version.clone()).collect();
    generate::update_index(
        &mut index,
        &versions,
        &activity.signals,
        activity.score,
        activity.cadence_days,
        now,
    );
    generated.push((index.path.clone(), index.raw));
    staged_files(repo_root, &generated)
}

/// Write the plan's files and run the pre-PR gates against the result.
///
/// Returns the tool's `source_repo` as the checkout had it *before* the
/// write, for the PR body: the plan's own index.json is staging-job input.
pub(crate) fn write_and_gate(
    repo_root: &Path,
    plan: &Plan,
    schemas: Schemas,
) -> Result<Option<String>> {
    let source_repo = base_source_repo(repo_root, &plan.tool_id)?;
    for file in &plan.files {
        let path = repo_root.join(&file.path);
        std::fs::write(&path, &file.contents)
            .with_context(|| format!("writing {}", path.display()))?;
    }
    // In-process semantic gate (cheap, hermetic).
    validate::validate_rules(repo_root)?;
    // Schema gate (shells out to ajv). Best effort for a local run — if
    // ajv isn't on PATH (e.g., dev machine without node), the in-process
    // pass and validate.yml's ajv step are the safety nets.
    match schemas {
        Schemas::Skip => {}
        Schemas::IfAvailable => {
            if which_ajv() {
                validate::validate_schemas(repo_root)?;
            }
        }
        Schemas::Require => validate::validate_schemas(repo_root)?,
    }
    Ok(source_repo)
}

/// Start a `--stage` run's manifest, or `None` for any other run.
pub(crate) fn start(repo_root: &Path, args: &RunArgs) -> anyhow::Result<Option<Manifest>> {
    if args.stage.is_none() {
        return Ok(None);
    }
    // A staging run must be able to check the plans it writes; fail
    // before any adapter runs rather than after every tool fails.
    anyhow::ensure!(
        which_ajv(),
        "--stage needs `npx ajv` (ajv-cli + ajv-formats) to validate the plans it writes"
    );
    Ok(Some(Manifest::new(pr::head_sha(repo_root)?)))
}

/// Write a `--stage` run's manifest to its `--stage` path.
pub(crate) fn finish(args: &RunArgs, staging: Option<Manifest>) -> anyhow::Result<()> {
    let (Some(path), Some(manifest)) = (&args.stage, staging) else {
        return Ok(());
    };
    manifest.write(path)?;
    println!(
        "scanner: staged {} plan(s) and {} failure(s) to {}",
        manifest.plans.len(),
        manifest.failures.len(),
        path.display()
    );
    Ok(())
}

/// `scanner deliver`: act on a staging run's manifest from the token job.
///
/// Refuses to start unless the checkout is at the commit the plans were
/// generated against. Per tool it then behaves like a `due` run: a failure
/// files a tracking issue and the run continues, and the run exits
/// non-zero if any tool — staged or delivered — failed.
pub(crate) fn deliver_manifest(
    repo_root: &Path,
    manifest: &Manifest,
    now: DateTime<Utc>,
) -> anyhow::Result<()> {
    deliver_manifest_with(
        repo_root,
        manifest,
        |tool_id, error| crate::report_failure(repo_root, tool_id, error, now),
        |req| pr::deliver(repo_root, req),
        outcome::step_summary_path().as_deref(),
    )
}

/// [`deliver_manifest`] with its two `gh` steps — filing a tracking issue
/// and opening/updating a PR — and its step-summary path injected, so the
/// loop is testable without `gh` or the real job summary.
pub(crate) fn deliver_manifest_with(
    repo_root: &Path,
    manifest: &Manifest,
    mut report: impl FnMut(&str, &str),
    mut deliver: impl FnMut(&PrRequest) -> Result<Delivery>,
    summary: Option<&Path>,
) -> anyhow::Result<()> {
    let sha = pr::head_sha(repo_root)?;
    anyhow::ensure!(
        sha == manifest.base_sha,
        "plan was staged at {} but the checkout is at {sha}; refusing to deliver",
        manifest.base_sha
    );
    let base = pr::RunBase::capture(repo_root, false)?;
    let mut failed = Vec::new();
    for f in &manifest.failures {
        // Debug-escaped: raw staged text in this job's log could start a
        // line with `::` and be read as a workflow command.
        let tool = format!("{:?}", f.tool_id);
        // An unchecked failure would let the staging job file an issue
        // about any tool, with any text. A bad entry still fails the run,
        // but files nothing and doesn't stop the plans below.
        match f.check(repo_root) {
            Ok(()) => {
                eprintln!("scanner: {} FAILED while staging: {:?}", f.tool_id, f.error);
                report(&f.tool_id, &f.error);
            }
            Err(e) => eprintln!("scanner: refusing staged failure for {tool}: {e:#}"),
        }
        failed.push(tool);
    }
    for plan in &manifest.plans {
        // Plan text is staging-job input: everything logged from it here is
        // debug-escaped, and an issue is filed only for a vetted tool id.
        let tool = format!("{:?}", plan.tool_id);
        if let Err(e) = check_known_tool(repo_root, &plan.tool_id) {
            eprintln!("scanner: refusing plan for {tool}: {:?}", format!("{e:#}"));
            failed.push(tool);
            continue;
        }
        base.guard(repo_root, &plan.tool_id, &failed)?;
        match deliver_plan(repo_root, plan, &mut deliver) {
            Ok(outcome) => {
                println!("scanner: {}: {outcome:?}", plan.tool_id);
                outcome::report_step_summary_to(summary, &plan.tool_id, &outcome);
            }
            Err(e) => {
                let error = format!("{e:#}");
                eprintln!("scanner: {} FAILED: {error:?}", plan.tool_id);
                report(&plan.tool_id, &error);
                failed.push(plan.tool_id.clone());
            }
        }
    }
    crate::fail_if_any(&failed)
}

/// Deliver one plan from the token job: check it, write its files, re-run
/// the in-process rules, then open, update, or skip its PR. ajv is not
/// re-run here — the staging job already required it.
/// `deliver` is the PR step: [`pr::deliver`] outside tests.
pub(crate) fn deliver_plan(
    repo_root: &Path,
    plan: &Plan,
    deliver: impl FnOnce(&PrRequest) -> Result<Delivery>,
) -> Result<Outcome> {
    plan.check()?;
    check_additive(repo_root, plan)?;
    check_rerender(repo_root, plan)?;
    let source_repo = match write_and_gate(repo_root, plan, Schemas::Skip) {
        Ok(source_repo) => source_repo,
        Err(e) => {
            if let Err(d) = pr::discard_generated(repo_root, &plan.tool_id) {
                eprintln!(
                    "scanner: {}: discarding files after a failed gate: {d:#}",
                    plan.tool_id
                );
            }
            return Err(e);
        }
    };
    deliver_checked_with(plan, source_repo.as_deref(), deliver)
}

/// The tool's `source_repo` as the checkout has it. `None` for a tool new
/// on the base or without one.
fn base_source_repo(repo_root: &Path, tool_id: &str) -> Result<Option<String>> {
    let index_path = repo_root.join("tools").join(tool_id).join("index.json");
    if !index_path.is_file() {
        return Ok(None);
    }
    Ok(ToolIndex::from_path(&index_path)?.source_repo)
}

/// Open, update, or skip the PR for a plan whose files are written and
/// gated. `source_repo` is what [`write_and_gate`] returned.
pub(crate) fn deliver_checked(
    repo_root: &Path,
    plan: &Plan,
    source_repo: Option<&str>,
) -> Result<Outcome> {
    deliver_checked_with(plan, source_repo, |req| pr::deliver(repo_root, req))
}

fn deliver_checked_with(
    plan: &Plan,
    source_repo: Option<&str>,
    deliver: impl FnOnce(&PrRequest) -> Result<Delivery>,
) -> Result<Outcome> {
    let delivery = deliver(&PrRequest {
        tool_id: plan.tool_id.clone(),
        prefix: plan.prefix(),
        branch: plan.branch(),
        new_versions: plan.new_versions.clone(),
        title: plan.title(),
        body: plan.body(source_repo),
    })?;
    Ok(Outcome::delivered(delivery, plan.new_versions.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stage::Failure;
    use crate::stage::plan::tests::plan;

    #[test]
    fn finish_writes_a_manifest_that_reads_back() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("plan.json");
        let mut manifest = Manifest::new("a".repeat(40));
        manifest.plans.push(plan());
        manifest.failures.push(Failure {
            tool_id: "gcc".into(),
            error: "boom".into(),
        });
        let args = RunArgs {
            dry_run: false,
            stage: Some(path.clone()),
        };
        finish(&args, Some(manifest.clone())).unwrap();
        assert_eq!(Manifest::read(&path).unwrap(), manifest);
    }

    #[test]
    fn finish_writes_nothing_without_a_started_manifest() {
        // `--stage` given but staging never started (e.g. nothing due).
        // The no-`--stage` case has no path to observe, so it isn't pinned.
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("plan.json");
        let staged = RunArgs {
            dry_run: false,
            stage: Some(path.clone()),
        };
        finish(&staged, None).unwrap();
        assert!(std::fs::read_dir(tmp.path()).unwrap().next().is_none());
    }

    #[test]
    fn staged_files_are_repo_relative_with_the_written_bytes() {
        let root = Path::new("/repo");
        let value = serde_json::json!({ "a": 1 });
        let files = staged_files(
            root,
            &[(
                root.join("tools").join("rust").join("index.json"),
                value.clone(),
            )],
        )
        .unwrap();
        assert_eq!(
            files,
            vec![StagedFile {
                path: "tools/rust/index.json".into(),
                contents: "{\n  \"a\": 1\n}\n".into(),
            }]
        );
        assert!(staged_files(root, &[(PathBuf::from("/elsewhere/x.json"), value)]).is_err());
    }
}
