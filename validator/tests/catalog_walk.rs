//! The catalog walks in `common` must refuse every shape that would shrink
//! them. Each case builds a tempdir catalog, proves the pristine tree walks,
//! then applies one mutation and asserts the walk panics naming it.

mod common;

use std::fs;
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;

use common::{tool_indexes_in, version_files_in};

/// A minimal catalog: one versioned language tool and one system package.
fn pristine() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let go = root.path().join("tools/go");
    fs::create_dir_all(go.join("versions")).unwrap();
    fs::write(go.join("index.json"), r#"{"kind":"language"}"#).unwrap();
    fs::write(go.join("versions/1.0.0.json"), "{}").unwrap();
    fs::create_dir_all(go.join("recipes")).unwrap();
    fs::write(go.join("recipes/go.json"), "{}").unwrap();
    let bash = root.path().join("tools/bash");
    fs::create_dir_all(&bash).unwrap();
    fs::write(bash.join("index.json"), r#"{"kind":"system_package"}"#).unwrap();

    assert_eq!(tool_indexes_in(root.path()).len(), 2);
    assert_eq!(version_files_in(root.path()).len(), 1);
    root
}

/// Run `walk` and return its panic message; fails the test if it returns.
fn panic_message(walk: impl FnOnce()) -> String {
    let payload = panic::catch_unwind(AssertUnwindSafe(walk)).expect_err("walk did not panic");
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .unwrap_or_default()
}

fn walk_all(root: &Path) {
    tool_indexes_in(root);
    version_files_in(root);
}

#[test]
fn tool_dir_without_index_panics() {
    let root = pristine();
    fs::remove_file(root.path().join("tools/bash/index.json")).unwrap();
    let msg = panic_message(|| walk_all(root.path()));
    assert!(msg.contains("bash has no index.json"), "{msg}");
}

#[test]
fn stray_file_under_tools_panics() {
    let root = pristine();
    fs::write(root.path().join("tools/README.md"), "").unwrap();
    let msg = panic_message(|| walk_all(root.path()));
    assert!(msg.contains("unexpected entry in tools/"), "{msg}");
}

#[test]
fn non_json_entry_in_versions_panics() {
    let root = pristine();
    fs::write(root.path().join("tools/go/versions/README.md"), "").unwrap();
    let msg = panic_message(|| walk_all(root.path()));
    assert!(msg.contains("unexpected entry in versions/"), "{msg}");
}

#[test]
fn directory_in_versions_panics() {
    // A `.json`-named directory isolates the is_file() half of the guard
    // from the extension half that non_json_entry_in_versions_panics drives.
    let root = pristine();
    fs::create_dir(root.path().join("tools/go/versions/nested.json")).unwrap();
    let msg = panic_message(|| walk_all(root.path()));
    assert!(msg.contains("unexpected entry in versions/"), "{msg}");
}

#[test]
fn versioned_tool_missing_versions_dir_panics() {
    let root = pristine();
    fs::remove_dir_all(root.path().join("tools/go/versions")).unwrap();
    let msg = panic_message(|| walk_all(root.path()));
    assert!(msg.contains("versions/ presence disagrees"), "{msg}");
}

#[test]
fn versioned_tool_with_misnamed_versions_dir_panics() {
    // The allow-list names the typo itself rather than leaving the
    // presence guard to report a generic missing versions/.
    let root = pristine();
    fs::rename(
        root.path().join("tools/go/versions"),
        root.path().join("tools/go/version"),
    )
    .unwrap();
    let msg = panic_message(|| walk_all(root.path()));
    assert!(msg.contains("unexpected entry in tool dir"), "{msg}");
    assert!(msg.ends_with("go/version"), "{msg}");
}

#[test]
fn versioned_tool_with_empty_versions_dir_panics() {
    let root = pristine();
    fs::remove_file(root.path().join("tools/go/versions/1.0.0.json")).unwrap();
    let msg = panic_message(|| walk_all(root.path()));
    assert!(msg.contains("versions/ is empty"), "{msg}");
}

#[test]
fn index_without_kind_panics() {
    let root = pristine();
    fs::write(root.path().join("tools/bash/index.json"), "{}").unwrap();
    let msg = panic_message(|| walk_all(root.path()));
    assert!(msg.contains("index has no string `kind`"), "{msg}");
}

#[test]
fn index_with_non_string_kind_panics() {
    let root = pristine();
    fs::write(root.path().join("tools/bash/index.json"), r#"{"kind":1}"#).unwrap();
    let msg = panic_message(|| walk_all(root.path()));
    assert!(msg.contains("index has no string `kind`"), "{msg}");
}

#[test]
fn malformed_index_panics_naming_it() {
    let root = pristine();
    fs::write(root.path().join("tools/bash/index.json"), "not json").unwrap();
    let msg = panic_message(|| walk_all(root.path()));
    assert!(msg.contains("bash/index.json: expected"), "{msg}");
}

#[test]
fn system_package_with_versions_dir_panics() {
    let root = pristine();
    fs::create_dir(root.path().join("tools/bash/versions")).unwrap();
    let msg = panic_message(|| walk_all(root.path()));
    assert!(msg.contains("versions/ presence disagrees"), "{msg}");
}

#[test]
fn misnamed_dir_in_tool_dir_panics() {
    // On a system_package tool the versions/-presence guard has nothing to
    // say about `vershuns/`, so only the tool-dir allow-list can catch it.
    let root = pristine();
    fs::create_dir(root.path().join("tools/bash/vershuns")).unwrap();
    let msg = panic_message(|| walk_all(root.path()));
    assert!(msg.contains("unexpected entry in tool dir"), "{msg}");
    assert!(msg.contains("vershuns"), "{msg}");
}

#[test]
fn stray_file_in_tool_dir_panics() {
    let root = pristine();
    fs::write(root.path().join("tools/go/README.md"), "").unwrap();
    let msg = panic_message(|| walk_all(root.path()));
    assert!(msg.contains("unexpected entry in tool dir"), "{msg}");
    assert!(msg.contains("README.md"), "{msg}");
}

#[test]
fn recipes_as_file_panics() {
    let root = pristine();
    fs::write(root.path().join("tools/bash/recipes"), "").unwrap();
    let msg = panic_message(|| walk_all(root.path()));
    assert!(msg.contains("unexpected entry in tool dir"), "{msg}");
    assert!(msg.contains("bash/recipes"), "{msg}");
}

#[test]
fn versions_as_file_panics() {
    // Fires before the versions/-presence guard, which would otherwise
    // report a file named `versions` as a missing directory.
    let root = pristine();
    fs::remove_dir_all(root.path().join("tools/go/versions")).unwrap();
    fs::write(root.path().join("tools/go/versions"), "").unwrap();
    let msg = panic_message(|| walk_all(root.path()));
    assert!(msg.contains("unexpected entry in tool dir"), "{msg}");
    assert!(msg.contains("go/versions"), "{msg}");
}

#[test]
fn index_json_as_dir_panics() {
    let root = pristine();
    fs::remove_file(root.path().join("tools/bash/index.json")).unwrap();
    fs::create_dir(root.path().join("tools/bash/index.json")).unwrap();
    let msg = panic_message(|| walk_all(root.path()));
    assert!(msg.contains("unexpected entry in tool dir"), "{msg}");
    assert!(msg.contains("bash/index.json"), "{msg}");
}
