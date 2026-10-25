//! Catalog walks shared by the `containers_common` parse tests.
//!
//! Each walk fails loudly on a shape it does not expect instead of filtering
//! it out: a filter that silently drops an entry shrinks the walk, and a
//! shrunken walk still passes. The `*_in` forms take the catalog root so
//! `catalog_walk.rs` can drive each guard against a tempdir tree.

#![allow(dead_code)] // each test binary uses a different subset

use std::fs;
use std::path::{Path, PathBuf};

pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

/// Every `<root>/tools/<id>/` directory, sorted. Panics on any non-directory
/// entry directly under `tools/`.
pub fn tool_dirs_in(root: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = fs::read_dir(root.join("tools"))
        .expect("read tools/")
        .map(|e| {
            let path = e.expect("tools/ entry").path();
            assert!(
                path.is_dir(),
                "unexpected entry in tools/: {}",
                path.display()
            );
            path
        })
        .collect();
    dirs.sort();
    dirs
}

pub fn tool_dirs() -> Vec<PathBuf> {
    tool_dirs_in(&repo_root())
}

/// The tool id of a `tools/<id>/...` path's `<id>` component.
pub fn tool_id(dir: &Path) -> String {
    dir.file_name().unwrap().to_string_lossy().into_owned()
}

/// `tools/<id>/index.json` for every tool directory. Panics if any directory
/// has no index, so a skipped tool cannot pass as a smaller catalog.
pub fn tool_indexes_in(root: &Path) -> Vec<PathBuf> {
    tool_dirs_in(root)
        .into_iter()
        .map(|dir| {
            let index = dir.join("index.json");
            assert!(index.is_file(), "{} has no index.json", dir.display());
            index
        })
        .collect()
}

pub fn tool_indexes() -> Vec<PathBuf> {
    tool_indexes_in(&repo_root())
}

/// Whether a tool ships per-version files. A `system_package` tool is
/// resolved through the distro and has none; every other kind must have a
/// `versions/` directory, so a missing or misnamed one fails the walk
/// instead of silently contributing zero files.
fn expects_versions(index: &Path) -> bool {
    let text = fs::read_to_string(index).unwrap_or_else(|e| panic!("{}: {e}", index.display()));
    let value: serde_json::Value =
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", index.display()));
    value["kind"] != "system_package"
}

/// Every `tools/<id>/versions/*.json`, sorted. Panics on any non-`.json`
/// entry in a `versions/` directory, and on a `versions/` directory that is
/// missing where the tool's kind requires one (or present where it does not).
pub fn version_files_in(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for index in tool_indexes_in(root) {
        let dir = index.parent().unwrap();
        let versions = dir.join("versions");
        assert_eq!(
            versions.is_dir(),
            expects_versions(&index),
            "{}: versions/ presence disagrees with the index's kind",
            dir.display()
        );
        if !versions.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&versions).expect("read versions/") {
            let path = entry.expect("versions/ entry").path();
            assert!(
                path.is_file() && path.extension().is_some_and(|e| e == "json"),
                "unexpected entry in versions/: {}",
                path.display()
            );
            files.push(path);
        }
    }
    files.sort();
    files
}

pub fn version_files() -> Vec<PathBuf> {
    version_files_in(&repo_root())
}

/// The tool id owning a `tools/<id>/versions/<v>.json` path.
pub fn version_file_tool(path: &Path) -> String {
    tool_id(path.parent().unwrap().parent().unwrap())
}
