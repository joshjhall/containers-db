//! Integration tests for rule 7 of `db_validator::rules`: every
//! `verification.gpg_keyring` resolves to a regular file under
//! `keyrings/` (against `fixtures/` for fixture files).

mod common;

use std::fs;
use std::path::Path;

use common::repo_root;
use db_validator::{catalog, rules};
use tempfile::TempDir;

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

/// A version doc whose one tier-1 install method pins `keyring`.
fn version_doc(keyring: &str) -> String {
    format!(
        r#"{{
            "schemaVersion": 1, "tool": "cli_a", "version": "1.0.0",
            "install_methods": [
                {{ "name": "tarball",
                   "verification": {{
                       "tier": 1, "algorithm": "gpg",
                       "gpg_keyring": "{keyring}",
                       "signature_url_template": "https://x/{{version}}.asc"
                   }},
                   "source_url_template": "https://x/{{version}}" }}
            ]
        }}"#
    )
}

/// Minimal repo: one tool, `keyrings/real.asc`, and a version file
/// pinning `keyring`.
fn repo_with_keyring(keyring: &str) -> TempDir {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(
        &root.join("tools/cli_a/index.json"),
        r#"{ "schemaVersion": 1, "id": "cli_a", "kind": "cli" }"#,
    );
    write(&root.join("keyrings/real.asc"), "stub\n");
    write(
        &root.join("tools/cli_a/versions/1.0.0.json"),
        &version_doc(keyring),
    );
    tmp
}

fn all_diagnostics(root: &Path, only: Option<&Path>) -> Vec<String> {
    let cat = catalog::load(root).unwrap();
    rules::check_all(root, &cat, only)
        .unwrap()
        .into_iter()
        .map(|d| d.to_string())
        .collect()
}

fn keyring_diagnostics(root: &Path, only: Option<&Path>) -> Vec<String> {
    all_diagnostics(root, only)
        .into_iter()
        .filter(|d| d.contains("gpg_keyring"))
        .collect()
}

#[test]
fn existing_keyring_passes() {
    // Unfiltered: a doc that failed to parse or never reached rule 7
    // would surface some other diagnostic here instead of passing.
    let tmp = repo_with_keyring("keyrings/real.asc");
    assert_eq!(all_diagnostics(tmp.path(), None), Vec::<String>::new());
}

#[test]
fn missing_keyring_fails_naming_the_path() {
    let tmp = repo_with_keyring("keyrings/typo.asc");
    let diags = keyring_diagnostics(tmp.path(), None);
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(
        diags[0],
        "tools/cli_a/versions/1.0.0.json:/install_methods/0/verification/gpg_keyring: \
         gpg_keyring \"keyrings/typo.asc\" does not exist (resolved against the catalog root)"
    );
}

#[cfg(unix)]
#[test]
fn symlink_escaping_keyrings_fails() {
    let tmp = repo_with_keyring("keyrings/escape.asc");
    let root = tmp.path();
    write(&root.join("outside.asc"), "stub\n");
    std::os::unix::fs::symlink("../outside.asc", root.join("keyrings/escape.asc")).unwrap();

    let diags = keyring_diagnostics(root, None);
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert!(
        diags[0].contains("gpg_keyring \"keyrings/escape.asc\" resolves outside keyrings/"),
        "{diags:?}"
    );
}

#[test]
fn dotdot_traversal_out_of_keyrings_fails() {
    // The schema pattern rejects `..`, but the rule must not lean on that.
    let tmp = repo_with_keyring("keyrings/../outside.asc");
    let root = tmp.path();
    write(&root.join("outside.asc"), "stub\n");

    let diags = keyring_diagnostics(root, None);
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert!(
        diags[0].contains("gpg_keyring \"keyrings/../outside.asc\" resolves outside keyrings/"),
        "{diags:?}"
    );
}

#[cfg(unix)]
#[test]
fn symlink_within_keyrings_passes() {
    let tmp = repo_with_keyring("keyrings/alias.asc");
    let root = tmp.path();
    std::os::unix::fs::symlink("real.asc", root.join("keyrings/alias.asc")).unwrap();
    assert_eq!(all_diagnostics(root, None), Vec::<String>::new());
}

#[cfg(unix)]
#[test]
fn symlink_loop_is_not_reported_as_missing() {
    let tmp = repo_with_keyring("keyrings/loop.asc");
    let root = tmp.path();
    std::os::unix::fs::symlink("loop.asc", root.join("keyrings/loop.asc")).unwrap();

    let diags = keyring_diagnostics(root, None);
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert!(
        diags[0].contains(
            "gpg_keyring \"keyrings/loop.asc\" could not be resolved against the catalog root: "
        ) && !diags[0].contains("does not exist"),
        "{diags:?}"
    );
}

#[cfg(unix)]
#[test]
fn dangling_symlink_in_keyrings_does_not_exist() {
    let tmp = repo_with_keyring("keyrings/dangling.asc");
    let root = tmp.path();
    std::os::unix::fs::symlink("missing.asc", root.join("keyrings/dangling.asc")).unwrap();

    let diags = keyring_diagnostics(root, None);
    assert_eq!(
        diags,
        vec![
            "tools/cli_a/versions/1.0.0.json:/install_methods/0/verification/gpg_keyring: \
             gpg_keyring \"keyrings/dangling.asc\" does not exist (resolved against the catalog root)"
                .to_string()
        ]
    );
}

#[test]
fn pointer_names_the_offending_method_and_skips_keyringless_ones() {
    // Method 0 has no verification, method 1 a checksum one without
    // gpg_keyring; only method 2's bad keyring may be reported, at index 2.
    let tmp = repo_with_keyring("keyrings/real.asc");
    let root = tmp.path();
    write(
        &root.join("tools/cli_a/versions/1.0.0.json"),
        r#"{
            "schemaVersion": 1, "tool": "cli_a", "version": "1.0.0",
            "install_methods": [
                { "name": "plain", "source_url_template": "https://x/{version}" },
                { "name": "checked",
                  "verification": { "tier": 2, "algorithm": "sha256" },
                  "source_url_template": "https://x/{version}" },
                { "name": "signed",
                  "verification": {
                      "tier": 1, "algorithm": "gpg",
                      "gpg_keyring": "keyrings/typo.asc",
                      "signature_url_template": "https://x/{version}.asc"
                  },
                  "source_url_template": "https://x/{version}" }
            ]
        }"#,
    );

    let diags = keyring_diagnostics(root, None);
    assert_eq!(
        diags,
        vec![
            "tools/cli_a/versions/1.0.0.json:/install_methods/2/verification/gpg_keyring: \
             gpg_keyring \"keyrings/typo.asc\" does not exist (resolved against the catalog root)"
                .to_string()
        ]
    );
}

#[test]
fn directory_is_not_a_regular_file() {
    let tmp = repo_with_keyring("keyrings/dir.asc");
    fs::create_dir(tmp.path().join("keyrings/dir.asc")).unwrap();
    let diags = keyring_diagnostics(tmp.path(), None);
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert!(
        diags[0].ends_with("gpg_keyring \"keyrings/dir.asc\" is not a regular file"),
        "{diags:?}"
    );
}

#[test]
fn fixture_files_resolve_against_fixtures_dir() {
    // keyrings/real.asc exists at the repo root but NOT under fixtures/,
    // so a fixture naming it must fail; one naming a fixtures/keyrings/
    // stub must pass.
    let tmp = repo_with_keyring("keyrings/real.asc");
    let root = tmp.path();
    write(
        &root.join("fixtures/tier1-a-example.json"),
        &version_doc("keyrings/real.asc"),
    );
    write(&root.join("fixtures/keyrings/stub.asc"), "stub\n");
    write(
        &root.join("fixtures/tier1-b-example.json"),
        &version_doc("keyrings/stub.asc"),
    );

    let diags = keyring_diagnostics(root, None);
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert!(
        diags[0].starts_with("fixtures/tier1-a-example.json:")
            && diags[0].contains("(resolved against fixtures/)"),
        "{diags:?}"
    );
}

#[test]
fn missing_keyrings_dir_reports_outside() {
    // fixtures/ has the named file but no keyrings/ subdir, so nothing
    // can resolve under it; the existing target must be reported as outside.
    let tmp = repo_with_keyring("keyrings/real.asc");
    let root = tmp.path();
    write(&root.join("fixtures/stray.asc"), "stub\n");
    write(
        &root.join("fixtures/tier1-a-example.json"),
        &version_doc("stray.asc"),
    );

    let diags = keyring_diagnostics(root, None);
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert!(
        diags[0].starts_with("fixtures/tier1-a-example.json:")
            && diags[0].contains("gpg_keyring \"stray.asc\" resolves outside keyrings/"),
        "{diags:?}"
    );
}

#[test]
fn real_catalog_keyrings_resolve() {
    // Covers keyrings/nodejs.asc + keyrings/golang.asc and the fixture stub.
    let root = repo_root();
    assert_eq!(keyring_diagnostics(&root, None), Vec::<String>::new());

    // Guard against a vacuous pass: the catalog must actually reference
    // both pinned keyrings, or the assertion above proves nothing.
    for (tool, keyring) in [
        ("node", "keyrings/nodejs.asc"),
        ("go", "keyrings/golang.asc"),
    ] {
        let needle = format!("\"gpg_keyring\": \"{keyring}\"");
        let hits = fs::read_dir(root.join("tools").join(tool).join("versions"))
            .unwrap()
            .filter(|e| {
                fs::read_to_string(e.as_ref().unwrap().path())
                    .unwrap()
                    .contains(&needle)
            })
            .count();
        assert!(hits > 0, "no tools/{tool} version file pins {keyring}");
    }
    let fixture =
        fs::read_to_string(root.join("fixtures/tier1-pinned-keyring-example.json")).unwrap();
    assert!(
        fixture.contains("\"gpg_keyring\": \"keyrings/sample-signed.asc\""),
        "tier1-pinned-keyring-example.json no longer pins the fixture stub"
    );
}

#[test]
fn negative_fixture_missing_keyring_fails() {
    let root = repo_root();
    let fixture = root.join("fixtures/_negative/tier1-keyring-missing.json");
    let diags = keyring_diagnostics(&root, Some(&fixture));
    assert_eq!(
        diags,
        vec![
            "fixtures/_negative/tier1-keyring-missing.json:/install_methods/0/verification/gpg_keyring: \
             gpg_keyring \"keyrings/does-not-exist.asc\" does not exist (resolved against fixtures/)"
                .to_string()
        ]
    );
}
