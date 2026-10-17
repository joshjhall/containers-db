//! Two-phase scan (#416). A job with no secret runs `--stage <path>`: it
//! generates and validates each tool's changes and writes them, as a
//! [`Manifest`], to `<path>`. The job holding `SCANNER_TOKEN` runs
//! `scanner deliver --plan <path>`, which writes those files back and
//! pushes / opens PRs. ajv and npm never run where the token is.
//!
//! The manifest crosses a trust boundary: the staging job runs third-party
//! code, so [`Plan::check`] confines every plan to its own `tools/<id>/`
//! directory and its own branch prefix before anything is written.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Outcome, RunArgs, outcome, pr, validate, which_ajv};

/// Manifest format version; [`Manifest::read`] rejects any other.
const MANIFEST_VERSION: u32 = 1;

/// Everything the token job needs to deliver one staging run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    /// Commit the plans were generated against. Delivery refuses to run
    /// from any other, or a PR would mix in changes the gates never saw.
    pub base_sha: String,
    pub plans: Vec<Plan>,
    /// Tools whose scan failed while staging; the token job files their
    /// tracking issues.
    pub failures: Vec<Failure>,
}

/// One tool's validated changes and the PR that should carry them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub tool_id: String,
    /// Dedup prefix: `scan/<id>/` or `backfill/<id>/`.
    pub prefix: String,
    pub branch: String,
    pub new_versions: Vec<String>,
    pub title: String,
    pub body: String,
    pub files: Vec<StagedFile>,
}

/// A file to write, by `/`-separated repo-relative path, with its exact bytes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StagedFile {
    pub path: String,
    pub contents: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Failure {
    pub tool_id: String,
    pub error: String,
}

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

impl Manifest {
    pub fn new(base_sha: String) -> Self {
        Manifest {
            version: MANIFEST_VERSION,
            base_sha,
            plans: Vec::new(),
            failures: Vec::new(),
        }
    }

    pub fn read(path: &Path) -> Result<Self> {
        let bytes =
            std::fs::read(path).with_context(|| format!("reading plan {}", path.display()))?;
        let manifest: Manifest = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing plan {}", path.display()))?;
        anyhow::ensure!(
            manifest.version == MANIFEST_VERSION,
            "plan {} has version {}, expected {MANIFEST_VERSION}",
            path.display(),
            manifest.version
        );
        Ok(manifest)
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        let mut bytes = serde_json::to_vec_pretty(self)?;
        bytes.push(b'\n');
        std::fs::write(path, bytes).with_context(|| format!("writing plan {}", path.display()))
    }
}

impl Plan {
    /// Refuse a plan that reaches outside its own tool. The staging job is
    /// not trusted, so a tampered plan must not be able to write anywhere
    /// but `tools/<tool_id>/` or push to a branch it doesn't own.
    pub fn check(&self) -> Result<()> {
        let id = &self.tool_id;
        anyhow::ensure!(
            !id.is_empty()
                && id
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-'),
            "plan: invalid tool id {id:?}"
        );
        anyhow::ensure!(
            self.prefix == format!("scan/{id}/") || self.prefix == format!("backfill/{id}/"),
            "plan for `{id}`: prefix {:?} is not scan/{id}/ or backfill/{id}/",
            self.prefix
        );
        let suffix = self.branch.strip_prefix(&self.prefix).unwrap_or("");
        anyhow::ensure!(
            !suffix.is_empty()
                && !suffix.contains("..")
                && !suffix.ends_with(".lock")
                && suffix
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')),
            "plan for `{id}`: branch {:?} is not a plain name under {}",
            self.branch,
            self.prefix
        );
        anyhow::ensure!(
            !self.new_versions.is_empty(),
            "plan for `{id}`: no new versions"
        );
        anyhow::ensure!(!self.files.is_empty(), "plan for `{id}`: no files");
        for file in &self.files {
            check_path(id, &file.path)?;
            serde_json::from_str::<Value>(&file.contents)
                .with_context(|| format!("plan for `{id}`: {} is not JSON", file.path))?;
        }
        Ok(())
    }
}

/// `path` must be `tools/<id>/index.json` or `tools/<id>/versions/<v>.json`.
fn check_path(id: &str, path: &str) -> Result<()> {
    let parts: Vec<&str> = path.split('/').collect();
    let normal = Path::new(path)
        .components()
        .all(|c| matches!(c, Component::Normal(_)));
    let shaped = match parts.as_slice() {
        ["tools", t, "index.json"] => t == &id,
        ["tools", t, "versions", name] => {
            t == &id && name.len() > ".json".len() && name.ends_with(".json")
        }
        _ => false,
    };
    anyhow::ensure!(
        normal && shaped && !path.contains('\\'),
        "plan for `{id}`: refusing to write {path:?} (outside tools/{id}/index.json and tools/{id}/versions/)"
    );
    Ok(())
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

/// Write the plan's files and run the pre-PR gates against the result.
pub(crate) fn write_and_gate(repo_root: &Path, plan: &Plan, schemas: Schemas) -> Result<()> {
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
    Ok(())
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
    let sha = pr::head_sha(repo_root)?;
    anyhow::ensure!(
        sha == manifest.base_sha,
        "plan was staged at {} but the checkout is at {sha}; refusing to deliver",
        manifest.base_sha
    );
    let base = pr::RunBase::capture(repo_root, false)?;
    let mut failed = Vec::new();
    for f in &manifest.failures {
        eprintln!("scanner: {} FAILED while staging: {}", f.tool_id, f.error);
        crate::report_failure(repo_root, &f.tool_id, &f.error, now);
        failed.push(f.tool_id.clone());
    }
    for plan in &manifest.plans {
        base.guard(repo_root, &plan.tool_id, &failed)?;
        match deliver_plan(repo_root, plan) {
            Ok(outcome) => {
                println!("scanner: {}: {outcome:?}", plan.tool_id);
                outcome::report_step_summary(&plan.tool_id, &outcome);
            }
            Err(e) => {
                eprintln!("scanner: {} FAILED: {e:#}", plan.tool_id);
                crate::report_failure(repo_root, &plan.tool_id, &format!("{e:#}"), now);
                failed.push(plan.tool_id.clone());
            }
        }
    }
    crate::fail_if_any(&failed)
}

/// Deliver one plan from the token job: check it, write its files, re-run
/// the in-process rules, then open, update, or skip its PR. ajv is not
/// re-run here — the staging job already required it.
pub(crate) fn deliver_plan(repo_root: &Path, plan: &Plan) -> Result<Outcome> {
    plan.check()?;
    if let Err(e) = write_and_gate(repo_root, plan, Schemas::Skip) {
        if let Err(d) = pr::discard_generated(repo_root, &plan.tool_id) {
            eprintln!(
                "scanner: {}: discarding files after a failed gate: {d:#}",
                plan.tool_id
            );
        }
        return Err(e);
    }
    let delivery = pr::deliver(
        repo_root,
        &plan.tool_id,
        &plan.prefix,
        &plan.branch,
        &plan.new_versions,
        &plan.title,
        &plan.body,
    )?;
    Ok(Outcome::delivered(delivery, plan.new_versions.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> Plan {
        Plan {
            tool_id: "rust".into(),
            prefix: "scan/rust/".into(),
            branch: "scan/rust/2026-10-17".into(),
            new_versions: vec!["1.96.0".into()],
            title: "feat(rust): add upstream versions 1.96.0".into(),
            body: "## New versions\n\n- `1.96.0`\n".into(),
            files: vec![
                StagedFile {
                    path: "tools/rust/index.json".into(),
                    contents: "{}\n".into(),
                },
                StagedFile {
                    path: "tools/rust/versions/1.96.0.json".into(),
                    contents: "{}\n".into(),
                },
            ],
        }
    }

    fn check_err(p: &Plan) -> String {
        format!("{:#}", p.check().unwrap_err())
    }

    #[test]
    fn check_accepts_a_generated_scan_plan() {
        plan().check().unwrap();
    }

    #[test]
    fn check_accepts_a_backfill_plan() {
        let mut p = plan();
        p.prefix = "backfill/rust/".into();
        p.branch = "backfill/rust/1.82.0".into();
        p.check().unwrap();
    }

    #[test]
    fn check_rejects_a_path_outside_the_tool() {
        for path in [
            "tools/gcc/index.json",
            "tools/rust/../gcc/index.json",
            "tools/rust/versions/../../gcc/index.json",
            "/tools/rust/index.json",
            ".github/workflows/scan.yml",
            "tools/rust/versions/sub/1.0.0.json",
            "tools/rust/versions/.json",
            "tools/rust/versions/1.0.0.sh",
            "tools/rust/other.json",
            "tools/rust/versions\\1.0.0.json",
        ] {
            let mut p = plan();
            p.files[1].path = path.into();
            assert_eq!(
                check_err(&p),
                format!(
                    "plan for `rust`: refusing to write {path:?} (outside tools/rust/index.json and tools/rust/versions/)"
                ),
            );
        }
    }

    #[test]
    fn check_rejects_a_foreign_prefix_or_branch() {
        let mut p = plan();
        p.prefix = "scan/gcc/".into();
        p.branch = "scan/gcc/2026-10-17".into();
        assert_eq!(
            check_err(&p),
            "plan for `rust`: prefix \"scan/gcc/\" is not scan/rust/ or backfill/rust/"
        );
        for branch in [
            "main",
            "scan/rust/",
            "scan/rust/a/b",
            "scan/rust/a..b",
            "scan/rust/x.lock",
            "scan/rust/x y",
        ] {
            let mut p = plan();
            p.branch = branch.into();
            assert_eq!(
                check_err(&p),
                format!("plan for `rust`: branch {branch:?} is not a plain name under scan/rust/"),
            );
        }
    }

    #[test]
    fn check_rejects_a_bad_tool_id() {
        let mut p = plan();
        p.tool_id = "../rust".into();
        assert_eq!(check_err(&p), "plan: invalid tool id \"../rust\"");
    }

    #[test]
    fn check_rejects_an_empty_plan_and_non_json_contents() {
        let mut p = plan();
        p.new_versions.clear();
        assert_eq!(check_err(&p), "plan for `rust`: no new versions");
        let mut p = plan();
        p.files.clear();
        assert_eq!(check_err(&p), "plan for `rust`: no files");
        let mut p = plan();
        p.files[0].contents = "not json".into();
        assert!(
            check_err(&p).starts_with("plan for `rust`: tools/rust/index.json is not JSON: "),
            "got {}",
            check_err(&p)
        );
    }

    #[test]
    fn manifest_round_trips_and_rejects_another_version() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("plan.json");
        let mut m = Manifest::new("abc123".into());
        m.plans.push(plan());
        m.failures.push(Failure {
            tool_id: "gcc".into(),
            error: "boom".into(),
        });
        m.write(&path).unwrap();
        assert_eq!(Manifest::read(&path).unwrap(), m);

        m.version = 2;
        m.write(&path).unwrap();
        assert_eq!(
            format!("{:#}", Manifest::read(&path).unwrap_err()),
            format!("plan {} has version 2, expected 1", path.display())
        );
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
