//! Integration tests for `db_validator::rules` covering the rules
//! that require the live `containers_common::version` parser:
//!
//!   - Rule 4: parseable version + constraint
//!   - Rule 5: mutual exclusion of `version` and `version_constraint`
//!   - Rule 6: requires[] intersection across the catalog
//!
//! Rules 1-3 are exercised by the negative fixture loop in CI plus the
//! `catalog::tests` unit tests.

use std::fs;
use std::path::Path;

use db_validator::{catalog, rules};
use tempfile::TempDir;

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

/// Set up a minimal repo with two tools (`cli_a`, `cli_b`) plus a
/// system_package (`gcc` on debian/alpine). Returns the tempdir so
/// callers can drop fixtures in.
fn fixture_repo() -> TempDir {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(
        &root.join("tools/cli_a/index.json"),
        r#"{ "schemaVersion": 1, "id": "cli_a", "kind": "cli" }"#,
    );
    write(
        &root.join("tools/cli_b/index.json"),
        r#"{ "schemaVersion": 1, "id": "cli_b", "kind": "cli" }"#,
    );
    write(
        &root.join("tools/gcc/index.json"),
        r#"{
            "schemaVersion": 1, "id": "gcc", "kind": "system_package",
            "system_package": { "platforms": {
                "debian": { "name": "gcc" },
                "alpine": { "name": "gcc" }
            } }
        }"#,
    );
    tmp
}

#[test]
fn well_formed_constraint_passes() {
    let tmp = fixture_repo();
    let root = tmp.path();
    write(
        &root.join("tools/cli_a/versions/1.0.0.json"),
        r#"{
            "schemaVersion": 1, "tool": "cli_a", "version": "1.0.0",
            "install_methods": [
                { "name": "tarball",
                  "verification": { "tier": 4, "algorithm": "sha256", "tofu": true },
                  "source_url_template": "https://x/{version}",
                  "dependencies": [
                      { "tool": "gcc", "version_constraint": ">=10, <14" }
                  ] }
            ]
        }"#,
    );
    let cat = catalog::load(root).unwrap();
    let diags = rules::check_all(root, &cat, None).unwrap();
    assert!(diags.is_empty(), "expected no diagnostics, got: {diags:?}");
}

#[test]
fn star_constraint_passes() {
    let tmp = fixture_repo();
    let root = tmp.path();
    write(
        &root.join("tools/cli_a/versions/1.0.0.json"),
        r#"{
            "schemaVersion": 1, "tool": "cli_a", "version": "1.0.0",
            "requires": [
                { "tool": "cli_b", "version_constraint": "*" }
            ],
            "install_methods": [
                { "name": "x",
                  "verification": { "tier": 4, "algorithm": "sha256", "tofu": true },
                  "source_url_template": "https://x" }
            ]
        }"#,
    );
    let cat = catalog::load(root).unwrap();
    let diags = rules::check_all(root, &cat, None).unwrap();
    assert!(diags.is_empty(), "got: {diags:?}");
}

#[test]
fn unparseable_constraint_fails_with_parser_error() {
    let tmp = fixture_repo();
    let root = tmp.path();
    write(
        &root.join("tools/cli_a/versions/1.0.0.json"),
        r#"{
            "schemaVersion": 1, "tool": "cli_a", "version": "1.0.0",
            "requires": [
                { "tool": "cli_b", "version_constraint": ">>1.7" }
            ],
            "install_methods": [
                { "name": "x",
                  "verification": { "tier": 4, "algorithm": "sha256", "tofu": true },
                  "source_url_template": "https://x" }
            ]
        }"#,
    );
    let cat = catalog::load(root).unwrap();
    let diags = rules::check_all(root, &cat, None).unwrap();
    assert_eq!(diags.len(), 1, "got: {diags:?}");
    let msg = diags[0].to_string();
    assert!(
        msg.contains("unparseable constraint") && msg.contains(">>1.7"),
        "expected parser error to surface the bad constraint, got: {msg}",
    );
}

#[test]
fn empty_constraint_fails() {
    let tmp = fixture_repo();
    let root = tmp.path();
    write(
        &root.join("tools/cli_a/versions/1.0.0.json"),
        r#"{
            "schemaVersion": 1, "tool": "cli_a", "version": "1.0.0",
            "requires": [
                { "tool": "cli_b", "version_constraint": "" }
            ],
            "install_methods": [
                { "name": "x",
                  "verification": { "tier": 4, "algorithm": "sha256", "tofu": true },
                  "source_url_template": "https://x" }
            ]
        }"#,
    );
    let cat = catalog::load(root).unwrap();
    let diags = rules::check_all(root, &cat, None).unwrap();
    assert!(!diags.is_empty(), "expected the empty constraint to fail");
}

#[test]
fn unparseable_version_pin_fails() {
    let tmp = fixture_repo();
    let root = tmp.path();
    write(
        &root.join("tools/cli_a/versions/1.0.0.json"),
        r#"{
            "schemaVersion": 1, "tool": "cli_a", "version": "1.0.0",
            "install_methods": [
                { "name": "x",
                  "verification": { "tier": 4, "algorithm": "sha256", "tofu": true },
                  "source_url_template": "https://x",
                  "dependencies": [
                      { "tool": "cli_b", "version": "not.a.version" }
                  ] }
            ]
        }"#,
    );
    let cat = catalog::load(root).unwrap();
    let diags = rules::check_all(root, &cat, None).unwrap();
    let msg = diags.iter().map(|d| d.to_string()).collect::<Vec<_>>().join("\n");
    assert!(msg.contains("unparseable version literal"), "got: {msg}");
}

#[test]
fn version_and_constraint_on_same_edge_is_mutually_exclusive() {
    let tmp = fixture_repo();
    let root = tmp.path();
    write(
        &root.join("tools/cli_a/versions/1.0.0.json"),
        r#"{
            "schemaVersion": 1, "tool": "cli_a", "version": "1.0.0",
            "install_methods": [
                { "name": "x",
                  "verification": { "tier": 4, "algorithm": "sha256", "tofu": true },
                  "source_url_template": "https://x",
                  "dependencies": [
                      { "tool": "cli_b", "version": "1.0.0", "version_constraint": ">=1.0" }
                  ] }
            ]
        }"#,
    );
    let cat = catalog::load(root).unwrap();
    let diags = rules::check_all(root, &cat, None).unwrap();
    let msg = diags.iter().map(|d| d.to_string()).collect::<Vec<_>>().join("\n");
    assert!(
        msg.contains("mutually exclusive"),
        "expected mutual-exclusion diagnostic, got: {msg}",
    );
}

#[test]
fn requires_intersection_across_catalog_passes_when_compatible() {
    let tmp = fixture_repo();
    let root = tmp.path();
    write(
        &root.join("tools/cli_a/versions/1.0.0.json"),
        r#"{
            "schemaVersion": 1, "tool": "cli_a", "version": "1.0.0",
            "requires": [
                { "tool": "cli_b", "version_constraint": ">=1.7, <3" }
            ],
            "install_methods": [
                { "name": "x",
                  "verification": { "tier": 4, "algorithm": "sha256", "tofu": true },
                  "source_url_template": "https://x" }
            ]
        }"#,
    );
    write(
        &root.join("fixtures/another/versions/0.5.0.json"),
        r#"{
            "schemaVersion": 1, "tool": "another", "version": "0.5.0",
            "requires": [
                { "tool": "cli_b", "version_constraint": ">=1.8.5" }
            ],
            "install_methods": [
                { "name": "x",
                  "verification": { "tier": 4, "algorithm": "sha256", "tofu": true },
                  "source_url_template": "https://x" }
            ]
        }"#,
    );
    let cat = catalog::load(root).unwrap();
    let diags = rules::check_all(root, &cat, None).unwrap();
    assert!(diags.is_empty(), "expected compatible ranges to pass: {diags:?}");
}

#[test]
fn requires_intersection_fails_on_disjoint_ranges() {
    let tmp = fixture_repo();
    let root = tmp.path();
    write(
        &root.join("tools/cli_a/versions/1.0.0.json"),
        r#"{
            "schemaVersion": 1, "tool": "cli_a", "version": "1.0.0",
            "requires": [
                { "tool": "cli_b", "version_constraint": ">=2.0" }
            ],
            "install_methods": [
                { "name": "x",
                  "verification": { "tier": 4, "algorithm": "sha256", "tofu": true },
                  "source_url_template": "https://x" }
            ]
        }"#,
    );
    write(
        &root.join("fixtures/another/versions/0.5.0.json"),
        r#"{
            "schemaVersion": 1, "tool": "another", "version": "0.5.0",
            "requires": [
                { "tool": "cli_b", "version_constraint": "<1.0" }
            ],
            "install_methods": [
                { "name": "x",
                  "verification": { "tier": 4, "algorithm": "sha256", "tofu": true },
                  "source_url_template": "https://x" }
            ]
        }"#,
    );
    let cat = catalog::load(root).unwrap();
    let diags = rules::check_all(root, &cat, None).unwrap();
    let msg = diags.iter().map(|d| d.to_string()).collect::<Vec<_>>().join("\n");
    assert!(
        msg.contains("requires[] intersection on `cli_b` is unsatisfiable"),
        "expected unsatisfiable-intersection diagnostic, got: {msg}",
    );
}

#[test]
fn only_mode_skips_intersection_check() {
    let tmp = fixture_repo();
    let root = tmp.path();
    let path = root.join("tools/cli_a/versions/1.0.0.json");
    write(
        &path,
        r#"{
            "schemaVersion": 1, "tool": "cli_a", "version": "1.0.0",
            "requires": [
                { "tool": "cli_b", "version_constraint": ">=2.0" }
            ],
            "install_methods": [
                { "name": "x",
                  "verification": { "tier": 4, "algorithm": "sha256", "tofu": true },
                  "source_url_template": "https://x" }
            ]
        }"#,
    );
    let cat = catalog::load(root).unwrap();
    // Even though >=2.0 alone would never cross the intersection check
    // (only one edge), confirm `--only` mode short-circuits the
    // aggregation path and only reports per-edge violations.
    let diags = rules::check_all(root, &cat, Some(&path)).unwrap();
    assert!(diags.is_empty(), "got: {diags:?}");
}
