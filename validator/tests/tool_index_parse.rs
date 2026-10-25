//! Every shipped `tools/<id>/index.json` must deserialize into the pinned
//! `containers_common::tooldb::Tool`, which is what luggage reads.
//!
//! `Tool` and every struct nested in it are `#[serde(deny_unknown_fields)]`,
//! so a catalog field the ajv schema accepts but the pinned containers-common
//! rev does not know (e.g. `version_args` before the #811 bump) passes schema
//! validation yet fails luggage's catalog parse. Nothing else in this repo
//! parses indexes as `Tool` (the scanner and validator use their own lenient
//! structs), so without this test that skew would only surface downstream.

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
    // tool_indexes panics on a tool dir with no index (catalog_walk.rs pins
    // that); this guards a wrong root — a walk of some other directory.
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

/// `tools/<tool>/index.json` as a JSON value, checked to parse as `Tool`
/// unmutated, so a rejection after mutation is the injected field.
fn pristine_index(tool: &str) -> serde_json::Value {
    let path = repo_root().join(format!("tools/{tool}/index.json"));
    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    serde_json::from_value::<Tool>(value.clone())
        .unwrap_or_else(|e| panic!("pristine {tool} index parses: {e}"));
    value
}

#[test]
fn tool_rejects_an_unknown_index_field() {
    let mut value = pristine_index("go");
    value
        .as_object_mut()
        .unwrap()
        .insert("not_a_catalog_field".into(), true.into());
    let err = serde_json::from_value::<Tool>(value)
        .unwrap_err()
        .to_string();
    assert!(err.contains("not_a_catalog_field"), "{err}");
}

#[test]
fn tool_rejects_an_unknown_field_at_every_nested_strict_level() {
    // Every nested `deny_unknown_fields` struct of `Tool` at the pinned rev
    // (7118f81). No one index reaches them all: go has no alternatives or
    // system_package, and the system-package tools have no activity signals,
    // channels, ordering or available rows.
    let cases = [
        ("go", "/activity"),                              // Activity
        ("go", "/activity/signals"),                      // ActivitySignals
        ("go", "/validation_tiers"),                      // ValidationTiers
        ("go", "/validation_tiers/tier3"),                // TierSummary
        ("go", "/channels/stable"),                       // Channel
        ("go", "/ordering"),                              // Ordering
        ("go", "/available/0"),                           // AvailableEntry
        ("libc_dev", "/alternatives/0"),                  // Alternative
        ("libc_dev", "/system_package"),                  // SystemPackage
        ("libc_dev", "/system_package/platforms/debian"), // SystemPackagePlatform
    ];
    for (tool, pointer) in cases {
        let mut value = pristine_index(tool);
        value
            .pointer_mut(pointer)
            .and_then(|v| v.as_object_mut())
            .unwrap_or_else(|| panic!("{tool}: no object at {pointer:?}"))
            .insert("not_a_catalog_field".into(), true.into());
        let err = serde_json::from_value::<Tool>(value)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("not_a_catalog_field"),
            "{tool} {pointer:?}: {err}"
        );
    }
}
