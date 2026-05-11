//! Pre-PR validation: in-process [`db_validator::rules`] checks plus a
//! shell-out to `ajv` for JSON Schema enforcement.
//!
//! The in-process pass catches semantic regressions (unknown
//! `dependencies[].tool`, unparseable constraints, etc.) before any
//! file leaves the working tree. The ajv pass mirrors the gates in
//! `.github/workflows/validate.yml` exactly so anything green locally
//! stays green in CI.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};
use db_validator::{catalog, rules};

/// Run every available pre-PR check against `repo_root`. Stops at the
/// first failing layer and returns a descriptive error.
pub fn validate_all(repo_root: &Path) -> Result<()> {
    validate_rules(repo_root)?;
    validate_schemas(repo_root)?;
    Ok(())
}

/// Run only the in-process semantic checks. Fast and hermetic — safe
/// for unit-test fixtures that don't have `npx ajv` available.
pub fn validate_rules(repo_root: &Path) -> Result<()> {
    let cat = catalog::load(repo_root)
        .with_context(|| format!("loading catalog at {}", repo_root.display()))?;
    let diagnostics =
        rules::check_all(repo_root, &cat, None).with_context(|| "running cross-file rules")?;
    if diagnostics.is_empty() {
        return Ok(());
    }
    let summary = diagnostics
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n  ");
    anyhow::bail!("semantic rules failed:\n  {summary}");
}

/// Shell out to `npx ajv` against the same schemas + targets the CI
/// workflow validates. Requires `ajv-cli` and `ajv-formats` installed
/// in the current working directory's `node_modules` or globally.
pub fn validate_schemas(repo_root: &Path) -> Result<()> {
    run_ajv(repo_root, "schema/tool.schema.json", "tools/*/index.json")?;
    run_ajv(
        repo_root,
        "schema/version.schema.json",
        "tools/*/versions/*.json",
    )?;
    Ok(())
}

fn run_ajv(repo_root: &Path, schema: &str, data_glob: &str) -> Result<()> {
    let output = Command::new("npx")
        .current_dir(repo_root)
        .args([
            "--no-install",
            "ajv",
            "validate",
            "--spec=draft2020",
            "-c",
            "ajv-formats",
            "-s",
            schema,
            "-d",
            data_glob,
        ])
        .output()
        .with_context(|| format!("running `npx ajv validate -s {schema} -d {data_glob}`"))?;
    if !output.status.success() {
        anyhow::bail!(
            "ajv validate -s {schema} -d {data_glob} failed:\n--stdout--\n{}\n--stderr--\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn rules_pass_on_well_formed_minimal_catalog() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        // Catalog with one CLI tool, one version, no dependencies — the
        // simplest shape that satisfies db_validator.
        write(
            &root.join("tools/foo/index.json"),
            r#"{ "schemaVersion": 1, "id": "foo", "kind": "cli" }"#,
        );
        write(
            &root.join("tools/foo/versions/1.0.0.json"),
            r#"{
                "schemaVersion": 1, "tool": "foo", "version": "1.0.0",
                "install_methods": [
                    { "name": "x",
                      "verification": { "tier": 4, "algorithm": "sha256", "tofu": true },
                      "source_url_template": "https://x" }
                ]
            }"#,
        );
        validate_rules(root).expect("clean catalog should validate");
    }

    #[test]
    fn rules_reject_unknown_tool_reference() {
        // Hand-craft a regression: a dependency that points at a tool id
        // not present in the catalog. db_validator's rule 1 must catch
        // it via validate_rules.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(
            &root.join("tools/foo/index.json"),
            r#"{ "schemaVersion": 1, "id": "foo", "kind": "cli" }"#,
        );
        write(
            &root.join("tools/foo/versions/1.0.0.json"),
            r#"{
                "schemaVersion": 1, "tool": "foo", "version": "1.0.0",
                "install_methods": [
                    { "name": "x",
                      "verification": { "tier": 4, "algorithm": "sha256", "tofu": true },
                      "source_url_template": "https://x",
                      "dependencies": [ { "tool": "ghost" } ] }
                ]
            }"#,
        );
        let err = validate_rules(root).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("ghost"),
            "expected unknown-tool diagnostic, got: {msg}"
        );
    }
}
