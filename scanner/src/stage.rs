//! Two-phase scan (#416). A job with no secret runs `--stage <path>`: it
//! generates and validates each tool's changes and writes them, as a
//! [`Manifest`], to `<path>`. The job holding `SCANNER_TOKEN` runs
//! `scanner deliver --plan <path>`, which writes those files back and
//! pushes / opens PRs. ajv and npm never run where the token is.
//!
//! The manifest crosses a trust boundary: the staging job runs third-party
//! code, so a plan carries no free text and the token job writes none of
//! its bytes. A plan names which releases to add; the token job re-renders
//! every file from its own checkout ([`render_files`]) and refuses the plan
//! unless the staged files match. Branch, PR title, and body are likewise
//! rendered from the plan's typed fields.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::activity::{ActivitySignals, Score};
use crate::adapter::{ToolIndex, UpstreamVersion};
use crate::{Outcome, RunArgs, generate, outcome, pr, validate, which_ajv};

/// Manifest format version; [`Manifest::read`] rejects any other.
const MANIFEST_VERSION: u32 = 1;

/// Longest staged error text a tracking issue will carry.
const MAX_ERROR_CHARS: usize = 8_000;

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

/// One tool's validated changes. The PR that carries them is derived:
/// see [`Plan::prefix`], [`Plan::branch`], [`Plan::title`], [`Plan::body`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub tool_id: String,
    pub kind: Kind,
    pub new_versions: Vec<String>,
    /// The upstream releases behind `new_versions`, in the same order:
    /// what [`render_files`] needs besides the checkout.
    pub releases: Vec<UpstreamVersion>,
    /// The scan's clock, stamped into `added_at` / `scanned_at`.
    pub now: DateTime<Utc>,
    pub activity: Activity,
    /// What the staging job wrote and validated. The token job never
    /// writes these bytes: it re-renders them from its own checkout and
    /// refuses the plan unless they match ([`deliver_plan`]).
    pub files: Vec<StagedFile>,
}

/// Which command produced a plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub enum Kind {
    /// A forward scan, branched as `scan/<id>/<date>`.
    Scan { date: chrono::NaiveDate },
    /// `scanner backfill --from <from>`.
    Backfill { from: String },
}

/// The recomputed activity block the PR body reports.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Activity {
    pub score: Score,
    pub cadence_days: u32,
    pub signals: ActivitySignals,
}

/// A file to write, by `/`-separated repo-relative path, with its exact bytes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StagedFile {
    pub path: String,
    pub contents: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Failure {
    pub tool_id: String,
    pub error: String,
}

/// A plain tool id: lowercase ASCII letters, digits, `_`, `-`.
fn check_tool_id(id: &str) -> Result<()> {
    anyhow::ensure!(
        !id.is_empty()
            && id
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-'),
        "plan: invalid tool id {id:?}"
    );
    Ok(())
}

/// A plain token for a version or backfill floor: no path, ref, or
/// markdown syntax can ride along into a branch name or PR text. Starts
/// alphanumeric and doesn't end in `.`, so it is also a valid git ref
/// component and file name.
fn check_token(id: &str, what: &str, v: &str) -> Result<()> {
    anyhow::ensure!(
        v.starts_with(|c: char| c.is_ascii_alphanumeric())
            && !v.ends_with('.')
            && v.len() <= 64
            && !v.contains("..")
            && !v.ends_with(".lock")
            && v.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+')),
        "plan for `{id}`: {what} {v:?} is not a plain version"
    );
    Ok(())
}

/// A plain tool id that the delivering checkout actually has. Required
/// before any tracking issue or PR names it.
fn check_known_tool(repo_root: &Path, id: &str) -> Result<()> {
    check_tool_id(id)?;
    let index = repo_root.join("tools").join(id).join("index.json");
    anyhow::ensure!(
        index.is_file(),
        "staged entry names `{id}`, which has no tools/{id}/index.json"
    );
    Ok(())
}

/// A scan or backfill only ever adds versions, so a plan may not rewrite a
/// release the checkout already has — which a PR titled "add versions"
/// would otherwise slip past a reviewer.
fn check_additive(repo_root: &Path, plan: &Plan) -> Result<()> {
    let id = &plan.tool_id;
    let existing: Vec<&str> = plan
        .new_versions
        .iter()
        .map(String::as_str)
        .filter(|v| {
            repo_root
                .join("tools")
                .join(id)
                .join("versions")
                .join(format!("{v}.json"))
                .exists()
        })
        .collect();
    anyhow::ensure!(
        existing.is_empty(),
        "plan for `{id}`: refusing to overwrite already-cataloged version(s) {existing:?}"
    );
    Ok(())
}

/// Re-render the plan from this checkout and require the staged bytes to
/// match exactly. A plan can then pick only *which* releases to add (and
/// their dates), never what its files say: install URLs, checksums, and
/// every index field come from the checkout's template. Same commit, same
/// inputs, same code, so an untampered plan always matches.
fn check_rerender(repo_root: &Path, plan: &Plan) -> Result<()> {
    let id = &plan.tool_id;
    let rendered = render_files(
        repo_root,
        id,
        &plan.kind,
        &plan.releases,
        &plan.activity,
        plan.now,
    )?;
    anyhow::ensure!(
        rendered == plan.files,
        "plan for `{id}`: staged files differ from what this checkout renders for its releases"
    );
    Ok(())
}

impl Failure {
    /// Refuse a staged failure for a tool the checkout doesn't have, and
    /// cap its error text, before it becomes a tracking issue.
    pub fn check(&self, repo_root: &Path) -> Result<()> {
        check_known_tool(repo_root, &self.tool_id)?;
        anyhow::ensure!(
            self.error.chars().count() <= MAX_ERROR_CHARS,
            "staged failure for `{}`: error text over {MAX_ERROR_CHARS} chars",
            self.tool_id
        );
        Ok(())
    }
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
    /// Dedup prefix: `scan/<id>/` or `backfill/<id>/`.
    pub fn prefix(&self) -> String {
        match &self.kind {
            Kind::Scan { .. } => format!("scan/{}/", self.tool_id),
            Kind::Backfill { .. } => format!("backfill/{}/", self.tool_id),
        }
    }

    /// Branch for a new PR; an open one under [`Plan::prefix`] is reused.
    pub fn branch(&self) -> String {
        match &self.kind {
            Kind::Scan { date } => format!("{}{}", self.prefix(), date.format("%Y-%m-%d")),
            Kind::Backfill { from } => format!("{}{from}", self.prefix()),
        }
    }

    pub fn title(&self) -> String {
        match &self.kind {
            Kind::Scan { .. } => pr::pr_title(&self.tool_id, &self.new_versions),
            Kind::Backfill { from } => {
                let last = self.new_versions.last().map(String::as_str).unwrap_or("");
                format!("feat({}): backfill versions {from} to {last}", self.tool_id)
            }
        }
    }

    /// PR body. `source_repo` comes from the delivering checkout's own
    /// `index.json`, never from the plan.
    pub fn body(&self, source_repo: Option<&str>) -> String {
        pr::pr_body(
            &self.tool_id,
            &self.new_versions,
            source_repo,
            self.activity.score,
            &self.activity.signals,
            self.activity.cadence_days,
        )
    }

    /// Refuse a plan that reaches outside its own tool. The staging job is
    /// not trusted, so a tampered plan must not be able to write anywhere
    /// but `tools/<tool_id>/`, push to a branch it doesn't own, or put
    /// text of its choosing in a PR.
    pub fn check(&self) -> Result<()> {
        check_tool_id(&self.tool_id)?;
        let id = &self.tool_id;
        if let Kind::Backfill { from } = &self.kind {
            check_token(id, "backfill floor", from)?;
        }
        anyhow::ensure!(
            !self.new_versions.is_empty(),
            "plan for `{id}`: no new versions"
        );
        for v in &self.new_versions {
            check_token(id, "version", v)?;
        }
        let released: Vec<&str> = self.releases.iter().map(|r| r.version.as_str()).collect();
        let announced: Vec<&str> = self.new_versions.iter().map(String::as_str).collect();
        anyhow::ensure!(
            released == announced,
            "plan for `{id}`: releases {released:?} don't match new_versions {announced:?}"
        );
        let mut indexes = 0;
        let mut versions = Vec::new();
        for file in &self.files {
            match check_path(id, &file.path)? {
                None => indexes += 1,
                Some(v) => versions.push(v),
            }
            serde_json::from_str::<Value>(&file.contents)
                .with_context(|| format!("plan for `{id}`: {} is not JSON", file.path))?;
        }
        let mut expected: Vec<&str> = self.new_versions.iter().map(String::as_str).collect();
        expected.sort_unstable();
        versions.sort_unstable();
        anyhow::ensure!(
            indexes == 1 && versions == expected,
            "plan for `{id}`: files must be one index.json plus versions/<v>.json for exactly {:?}",
            self.new_versions
        );
        Ok(())
    }
}

/// `path` must be `tools/<id>/index.json` (→ `None`) or
/// `tools/<id>/versions/<v>.json` (→ `Some(v)`).
fn check_path<'a>(id: &str, path: &'a str) -> Result<Option<&'a str>> {
    let parts: Vec<&str> = path.split('/').collect();
    let normal = Path::new(path)
        .components()
        .all(|c| matches!(c, Component::Normal(_)));
    let shaped = match parts.as_slice() {
        ["tools", t, "index.json"] if t == &id => Some(None),
        ["tools", t, "versions", name] if t == &id => name
            .strip_suffix(".json")
            .filter(|v| !v.is_empty())
            .map(Some),
        _ => None,
    };
    match shaped {
        Some(version) if normal && !path.contains('\\') => Ok(version),
        _ => anyhow::bail!(
            "plan for `{id}`: refusing to write {path:?} (outside tools/{id}/index.json and tools/{id}/versions/)"
        ),
    }
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
                crate::report_failure(repo_root, &f.tool_id, &f.error, now);
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
        match deliver_plan(repo_root, plan) {
            Ok(outcome) => {
                println!("scanner: {}: {outcome:?}", plan.tool_id);
                outcome::report_step_summary(&plan.tool_id, &outcome);
            }
            Err(e) => {
                let error = format!("{e:#}");
                eprintln!("scanner: {} FAILED: {error:?}", plan.tool_id);
                crate::report_failure(repo_root, &plan.tool_id, &error, now);
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
    deliver_checked(repo_root, plan, source_repo.as_deref())
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
    let delivery = pr::deliver(
        repo_root,
        &plan.tool_id,
        &plan.prefix(),
        &plan.branch(),
        &plan.new_versions,
        &plan.title(),
        &plan.body(source_repo),
    )?;
    Ok(Outcome::delivered(delivery, plan.new_versions.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn activity() -> Activity {
        Activity {
            score: Score::Active,
            cadence_days: 7,
            signals: ActivitySignals::default(),
        }
    }

    fn plan() -> Plan {
        Plan {
            tool_id: "rust".into(),
            kind: Kind::Scan {
                date: chrono::NaiveDate::from_ymd_opt(2026, 10, 17).unwrap(),
            },
            new_versions: vec!["1.96.0".into()],
            releases: vec![UpstreamVersion {
                version: "1.96.0".into(),
                released_at: chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 10, 1, 0, 0, 0)
                    .unwrap(),
                channel: "stable".into(),
            }],
            now: chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 10, 17, 0, 0, 0).unwrap(),
            activity: activity(),
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
    fn scan_plan_derives_its_branch_and_title() {
        let p = plan();
        assert_eq!(p.prefix(), "scan/rust/");
        assert_eq!(p.branch(), "scan/rust/2026-10-17");
        assert_eq!(p.title(), "feat(rust): add upstream versions 1.96.0");
        let body = p.body(Some("https://github.com/rust-lang/rust"));
        assert_eq!(
            body,
            pr::pr_body(
                "rust",
                &["1.96.0".to_string()],
                Some("https://github.com/rust-lang/rust"),
                Score::Active,
                &ActivitySignals::default(),
                7,
            )
        );
    }

    #[test]
    fn check_accepts_a_backfill_plan() {
        let mut p = plan();
        p.kind = Kind::Backfill {
            from: "1.82.0".into(),
        };
        p.check().unwrap();
        assert_eq!(p.branch(), "backfill/rust/1.82.0");
        assert_eq!(p.title(), "feat(rust): backfill versions 1.82.0 to 1.96.0");
    }

    #[test]
    fn plans_carry_no_free_text_fields() {
        // A staged plan with a `title` (or any field the token job doesn't
        // derive) is refused at parse time, not silently ignored.
        let mut v = serde_json::to_value(plan()).unwrap();
        v["title"] = "@everyone click here".into();
        let err = serde_json::from_value::<Plan>(v).unwrap_err().to_string();
        assert!(err.starts_with("unknown field `title`"), "got {err}");
    }

    #[test]
    fn check_rejects_versions_and_floors_that_are_not_plain_tokens() {
        for v in [
            "", "1.0/../x", "1..0", "v.lock", "1.0 ", "`x`", "[a](b)", "1.0.", ".x", "-x", "+1",
        ] {
            let mut p = plan();
            p.new_versions = vec![v.into()];
            assert_eq!(
                check_err(&p),
                format!("plan for `rust`: version {v:?} is not a plain version")
            );
            let mut p = plan();
            p.kind = Kind::Backfill { from: v.into() };
            assert_eq!(
                check_err(&p),
                format!("plan for `rust`: backfill floor {v:?} is not a plain version")
            );
        }
    }

    #[test]
    fn check_ties_the_releases_to_the_new_versions() {
        let mut p = plan();
        p.releases[0].version = "1.97.0".into();
        assert_eq!(
            check_err(&p),
            "plan for `rust`: releases [\"1.97.0\"] don't match new_versions [\"1.96.0\"]"
        );
    }

    #[test]
    fn check_ties_the_files_to_the_new_versions() {
        let want = "plan for `rust`: files must be one index.json plus versions/<v>.json for exactly [\"1.96.0\"]";
        // Missing index.
        let mut p = plan();
        p.files.remove(0);
        assert_eq!(check_err(&p), want);
        // A version file the plan doesn't announce.
        let mut p = plan();
        p.files[1].path = "tools/rust/versions/1.97.0.json".into();
        assert_eq!(check_err(&p), want);
        // Two indexes.
        let mut p = plan();
        p.files.push(p.files[0].clone());
        assert_eq!(check_err(&p), want);
        // No files at all.
        let mut p = plan();
        p.files.clear();
        assert_eq!(check_err(&p), want);
    }

    #[test]
    fn failure_check_needs_a_real_tool_and_bounded_text() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("tools/rust")).unwrap();
        std::fs::write(root.join("tools/rust/index.json"), "{}").unwrap();
        let ok = Failure {
            tool_id: "rust".into(),
            error: "boom".into(),
        };
        ok.check(root).unwrap();
        let ghost = Failure {
            tool_id: "ghost".into(),
            ..ok.clone()
        };
        assert_eq!(
            ghost.check(root).unwrap_err().to_string(),
            "staged entry names `ghost`, which has no tools/ghost/index.json"
        );
        let bad = Failure {
            tool_id: "../rust".into(),
            ..ok.clone()
        };
        assert_eq!(
            bad.check(root).unwrap_err().to_string(),
            "plan: invalid tool id \"../rust\""
        );
        let long = Failure {
            error: "x".repeat(MAX_ERROR_CHARS + 1),
            ..ok
        };
        assert_eq!(
            long.check(root).unwrap_err().to_string(),
            format!("staged failure for `rust`: error text over {MAX_ERROR_CHARS} chars")
        );
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
