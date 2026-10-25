//! Every shipped `tools/<id>/index.json` must deserialize into the pinned
//! `containers_common::tooldb::Tool`, which is what luggage reads.
//!
//! `Tool` is `#[serde(deny_unknown_fields)]`, so a catalog field the ajv
//! schema accepts but the pinned containers-common rev does not know (e.g.
//! `version_args` before the #811 bump) passes schema validation yet fails
//! luggage's catalog parse. Nothing else in this repo parses indexes as
//! `Tool` (the scanner and validator use their own lenient structs), so
//! without this test that skew would only surface downstream.

mod common;

use std::fs;
use std::path::Path;

use containers_common::tooldb::Tool;

use common::{repo_root, tool_dirs, tool_id, tool_indexes};

fn parse(path: &Path) -> Result<Tool, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str::<Tool>(&text).map_err(|e| format!("{}: {e}", path.display()))
}

#[test]
fn every_tool_index_deserializes_as_tool() {
    let paths = tool_indexes();
    // One index per tool directory (tool_indexes panics on a missing one),
    // and the walk is rooted where the catalog actually is.
    assert_eq!(paths.len(), tool_dirs().len());
    let ids: Vec<String> = tool_dirs().iter().map(|d| tool_id(d)).collect();
    for known in ["go", "rust"] {
        assert!(
            ids.iter().any(|id| id == known),
            "walk missed tools/{known}/: {ids:?}"
        );
    }
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

#[test]
fn tool_rejects_an_unknown_index_field() {
    let text = fs::read_to_string(repo_root().join("tools/go/index.json")).unwrap();
    let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
    // The unmutated index parses, so the rejection below is the injected field.
    serde_json::from_value::<Tool>(value.clone()).expect("pristine go index parses");
    value
        .as_object_mut()
        .unwrap()
        .insert("not_a_catalog_field".into(), true.into());
    let err = serde_json::from_value::<Tool>(value)
        .unwrap_err()
        .to_string();
    assert!(err.contains("not_a_catalog_field"), "{err}");
}
