//! Catalog walks shared by the `containers_common` parse tests.
//!
//! Each walk fails loudly on a shape it does not expect instead of filtering
//! it out: a filter that silently drops an entry shrinks the walk, and a
//! shrunken walk still passes.

#![allow(dead_code)] // each test binary uses a different subset

use std::fs;
use std::path::{Path, PathBuf};

pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

/// Every `tools/<id>/` directory, sorted.
pub fn tool_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = fs::read_dir(repo_root().join("tools"))
        .expect("read tools/")
        .map(|e| e.expect("tools/ entry").path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    dirs
}

/// The tool id of a `tools/<id>/...` path's `<id>` component.
pub fn tool_id(dir: &Path) -> String {
    dir.file_name().unwrap().to_string_lossy().into_owned()
}

/// `tools/<id>/index.json` for every tool directory. Panics if any directory
/// has no index, so a skipped tool cannot pass as a smaller catalog.
pub fn tool_indexes() -> Vec<PathBuf> {
    tool_dirs()
        .into_iter()
        .map(|dir| {
            let index = dir.join("index.json");
            assert!(index.is_file(), "{} has no index.json", dir.display());
            index
        })
        .collect()
}

/// Every `tools/<id>/versions/*.json`, sorted. Panics on any non-`.json`
/// entry in a `versions/` directory rather than skipping it.
pub fn version_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    for dir in tool_dirs() {
        let versions = dir.join("versions");
        if !versions.exists() {
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

/// The tool id owning a `tools/<id>/versions/<v>.json` path.
pub fn version_file_tool(path: &Path) -> String {
    tool_id(path.parent().unwrap().parent().unwrap())
}
