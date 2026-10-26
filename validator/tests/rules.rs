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
    let msg = diags
        .iter()
        .map(|d| d.to_string())
        .collect::<Vec<_>>()
        .join("\n");
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
    let msg = diags
        .iter()
        .map(|d| d.to_string())
        .collect::<Vec<_>>()
        .join("\n");
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
    assert!(
        diags.is_empty(),
        "expected compatible ranges to pass: {diags:?}"
    );
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
    let msg = diags
        .iter()
        .map(|d| d.to_string())
        .collect::<Vec<_>>()
        .join("\n");
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

/// Write cli_a@1.0.0 depending on `gcc` narrowed to `platforms`.
fn dep_with_platforms(root: &Path, platforms: &str) {
    write(
        &root.join("tools/cli_a/versions/1.0.0.json"),
        &format!(
            r#"{{
            "schemaVersion": 1, "tool": "cli_a", "version": "1.0.0",
            "install_methods": [
                {{ "name": "tarball",
                  "verification": {{ "tier": 4, "algorithm": "sha256", "tofu": true }},
                  "source_url_template": "https://x/{{version}}",
                  "dependencies": [ {{ "tool": "gcc", "platforms": {platforms} }} ] }}
            ]
        }}"#
        ),
    );
}

#[test]
fn platform_selector_with_release_checks_only_the_distro() {
    // `debian:12` narrows to one Debian release; gcc's package map is keyed
    // per distro, and debian is declared.
    let tmp = fixture_repo();
    let root = tmp.path();
    dep_with_platforms(root, r#"["debian:12", "alpine"]"#);
    let cat = catalog::load(root).unwrap();
    let diags = rules::check_all(root, &cat, None).unwrap();
    assert!(diags.is_empty(), "expected no diagnostics, got: {diags:?}");
}

#[test]
fn platform_selector_with_release_on_undeclared_distro_fails() {
    // gcc declares debian + alpine only; `rhel:10` names an undeclared distro.
    let tmp = fixture_repo();
    let root = tmp.path();
    dep_with_platforms(root, r#"["rhel:10"]"#);
    let cat = catalog::load(root).unwrap();
    let diags = rules::check_all(root, &cat, None).unwrap();
    assert_eq!(diags.len(), 1, "expected one diagnostic, got: {diags:?}");
    let msg = &diags[0].message;
    assert!(
        msg.contains("`rhel:10`") && msg.contains("distro `rhel`"),
        "message should name the selector and its distro: {msg}"
    );
}

/// Write cli_a@1.0.0 with the per-arch method pair node uses (#481): an
/// amd64 method with no runtime dep, and an arm64 method whose `platform.arch`
/// scopes a runtime dependency on `gcc` (standing in for libatomic) narrowed
/// to `platforms`.
fn arm64_scoped_dep(root: &Path, platforms: &str) {
    write(
        &root.join("tools/cli_a/versions/1.0.0.json"),
        &format!(
            r#"{{
            "schemaVersion": 1, "tool": "cli_a", "version": "1.0.0",
            "install_methods": [
                {{ "name": "tarball-amd64",
                  "platform": {{ "os": ["debian"], "arch": ["amd64"] }},
                  "verification": {{ "tier": 4, "algorithm": "sha256", "tofu": true }},
                  "source_url_template": "https://x/{{version}}/amd64" }},
                {{ "name": "tarball-arm64",
                  "platform": {{ "os": ["debian"], "arch": ["arm64"] }},
                  "verification": {{ "tier": 4, "algorithm": "sha256", "tofu": true }},
                  "source_url_template": "https://x/{{version}}/arm64",
                  "dependencies": [
                    {{ "tool": "gcc", "purpose": "runtime", "platforms": {platforms} }}
                  ] }}
            ]
        }}"#
        ),
    );
}

#[test]
fn arm64_scoped_runtime_dependency_passes() {
    // The arch scope is the enclosing method's `platform.arch`; the
    // Dependency itself carries no arch field and needs none.
    let tmp = fixture_repo();
    let root = tmp.path();
    arm64_scoped_dep(root, r#"["debian", "alpine"]"#);
    let cat = catalog::load(root).unwrap();
    let diags = rules::check_all(root, &cat, None).unwrap();
    assert!(diags.is_empty(), "expected no diagnostics, got: {diags:?}");
}

#[test]
fn arm64_scoped_dependency_is_checked_on_its_own_method() {
    // The dep lives only on the second (arm64) method. A bad narrowing there
    // must be reported at that method's pointer, proving per-method deps are
    // walked rather than only the first method's.
    let tmp = fixture_repo();
    let root = tmp.path();
    arm64_scoped_dep(root, r#"["rhel"]"#);
    let cat = catalog::load(root).unwrap();
    let diags = rules::check_all(root, &cat, None).unwrap();
    assert_eq!(diags.len(), 1, "expected one diagnostic, got: {diags:?}");
    assert_eq!(diags[0].pointer, "/install_methods/1/dependencies/0");
    assert!(
        diags[0].message.contains("distro `rhel`"),
        "message should name the undeclared distro: {}",
        diags[0].message
    );
}
