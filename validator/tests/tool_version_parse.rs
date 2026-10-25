//! Every shipped `tools/<id>/versions/*.json` must deserialize into the
//! pinned `containers_common::tooldb::ToolVersion`, which is what luggage's
//! `Catalog::load` reads.
//!
//! `ToolVersion`, `InstallMethod`, `SupportEntry` and `TestEntry` are all
//! `#[serde(deny_unknown_fields)]`. A field the ajv schema accepts but the
//! pinned rev does not know (e.g. `install_methods[].build_steps` before the
//! pin moves past #809) passes schema validation yet fails luggage's catalog
//! parse; this is the version-file sibling of `tool_index_parse.rs`.

mod common;

use std::fs;
use std::path::Path;

use containers_common::tooldb::ToolVersion;

use common::{version_file_tool, version_files};

fn parse(path: &Path) -> Result<ToolVersion, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str::<ToolVersion>(&text).map_err(|e| format!("{}: {e}", path.display()))
}

fn first_go_version() -> serde_json::Value {
    let path = version_files()
        .into_iter()
        .find(|p| version_file_tool(p) == "go")
        .expect("a go version file");
    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    // The unmutated file parses, so a rejection after mutation is the injected field.
    serde_json::from_value::<ToolVersion>(value.clone()).expect("pristine go version parses");
    value
}

#[test]
fn every_version_file_deserializes_as_tool_version() {
    let paths = version_files();
    // Guard against a walk that found nothing for a tool known to ship versions.
    for known in ["go", "rust"] {
        assert!(
            paths.iter().any(|p| version_file_tool(p) == known),
            "walk found no tools/{known}/versions/*.json"
        );
    }
    let failures: Vec<String> = paths.iter().filter_map(|p| parse(p).err()).collect();
    assert!(
        failures.is_empty(),
        "{} of {} version files rejected by ToolVersion:\n{}",
        failures.len(),
        paths.len(),
        failures.join("\n")
    );
}

/// `None` when `v.tool` names the directory `path` lives in; otherwise the
/// mismatch, so an unparseable file is reported rather than skipped.
fn tool_mismatch(path: &Path, v: Result<ToolVersion, String>) -> Option<String> {
    match v {
        Ok(v) if v.tool == version_file_tool(path) => None,
        Ok(v) => Some(format!("{}: tool = {:?}", path.display(), v.tool)),
        Err(e) => Some(e),
    }
}

#[test]
fn version_file_tool_field_matches_its_directory() {
    let mismatched: Vec<String> = version_files()
        .iter()
        .filter_map(|p| tool_mismatch(p, parse(p)))
        .collect();
    assert!(mismatched.is_empty(), "{}", mismatched.join("\n"));
}

#[test]
fn tool_mismatch_flags_a_foreign_tool_id() {
    let path = version_files()
        .into_iter()
        .find(|p| version_file_tool(p) == "go")
        .expect("a go version file");
    let mut value = first_go_version();
    assert_eq!(
        tool_mismatch(&path, Ok(serde_json::from_value(value.clone()).unwrap())),
        None
    );
    value["tool"] = "rust".into();
    let msg = tool_mismatch(&path, Ok(serde_json::from_value(value).unwrap())).unwrap();
    assert!(msg.contains("tool = \"rust\""), "{msg}");
    assert_eq!(
        tool_mismatch(&path, Err("unparseable".into())),
        Some("unparseable".to_owned())
    );
}

/// The first shipped version file carrying every strict nested struct
/// (`install_methods`, `support_matrix`, and `tested` rows), so one fixture
/// reaches each `deny_unknown_fields` level the module doc names.
fn fully_populated_version() -> serde_json::Value {
    let value = version_files()
        .into_iter()
        .map(|p| {
            serde_json::from_str::<serde_json::Value>(&fs::read_to_string(p).unwrap()).unwrap()
        })
        .find(|v| {
            ["install_methods", "support_matrix", "tested"]
                .iter()
                .all(|k| v[k].as_array().is_some_and(|a| !a.is_empty()))
        })
        .expect("a version file with install_methods, support_matrix and tested rows");
    // The unmutated file parses, so a rejection after mutation is the injected field.
    serde_json::from_value::<ToolVersion>(value.clone()).expect("pristine version parses");
    value
}

#[test]
fn tool_version_rejects_an_unknown_field_at_every_strict_level() {
    // `""` is ToolVersion itself; the rest are InstallMethod, SupportEntry and
    // TestEntry. InstallMethod is what catches the live skew — `build_steps`
    // is in the schema (#461) but not the pinned struct — but a
    // permanently-invalid name keeps this true after the pin moves past #809.
    for pointer in ["", "/install_methods/0", "/support_matrix/0", "/tested/0"] {
        let mut value = fully_populated_version();
        value
            .pointer_mut(pointer)
            .and_then(|v| v.as_object_mut())
            .unwrap_or_else(|| panic!("no object at {pointer:?}"))
            .insert("not_a_catalog_field".into(), true.into());
        let err = serde_json::from_value::<ToolVersion>(value)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not_a_catalog_field"), "{pointer:?}: {err}");
    }
}
