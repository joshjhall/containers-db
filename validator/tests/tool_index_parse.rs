//! Every shipped `tools/<id>/index.json` must deserialize into the pinned
//! `containers_common::tooldb::Tool`, which is what luggage reads.
//!
//! `Tool` is `#[serde(deny_unknown_fields)]`, so a catalog field the ajv
//! schema accepts but the pinned containers-common rev does not know (e.g.
//! `version_args` before the #811 bump) passes schema validation yet fails
//! luggage's catalog parse. Nothing else in this repo parses indexes as
//! `Tool` (the scanner and validator use their own lenient structs), so
//! without this test that skew would only surface downstream.

use std::fs;
use std::path::{Path, PathBuf};

use containers_common::tooldb::Tool;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn tool_indexes() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = fs::read_dir(repo_root().join("tools"))
        .expect("read tools/")
        .map(|e| e.expect("tools/ entry").path().join("index.json"))
        .filter(|p| p.is_file())
        .collect();
    paths.sort();
    paths
}

fn parse(path: &Path) -> Result<Tool, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str::<Tool>(&text).map_err(|e| format!("{}: {e}", path.display()))
}

#[test]
fn every_tool_index_deserializes_as_tool() {
    let paths = tool_indexes();
    // Guard against a silently empty walk (wrong root) passing vacuously.
    assert!(
        paths.len() >= 2,
        "expected the shipped tool indexes, found {paths:?}"
    );
    let failures: Vec<String> = paths.iter().filter_map(|p| parse(p).err()).collect();
    assert!(
        failures.is_empty(),
        "indexes rejected by Tool:\n{}",
        failures.join("\n")
    );
}

#[test]
fn go_index_carries_version_args() {
    let go = parse(&repo_root().join("tools/go/index.json")).unwrap();
    assert_eq!(go.version_args, Some(vec!["version".to_owned()]));
}
