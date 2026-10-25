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

#[test]
fn version_file_tool_field_matches_its_directory() {
    let mismatched: Vec<String> = version_files()
        .iter()
        .filter_map(|p| {
            let v = parse(p).ok()?;
            let dir = version_file_tool(p);
            (v.tool != dir).then(|| format!("{}: tool = {:?}", p.display(), v.tool))
        })
        .collect();
    assert!(mismatched.is_empty(), "{}", mismatched.join("\n"));
}

#[test]
fn tool_version_rejects_an_unknown_top_level_field() {
    let mut value = first_go_version();
    value
        .as_object_mut()
        .unwrap()
        .insert("not_a_catalog_field".into(), true.into());
    let err = serde_json::from_value::<ToolVersion>(value)
        .unwrap_err()
        .to_string();
    assert!(err.contains("not_a_catalog_field"), "{err}");
}

#[test]
fn tool_version_rejects_an_unknown_install_method_field() {
    // `build_steps` is in the schema (#461) but not in the pinned
    // `InstallMethod`; this pins that the walk above would catch its first use.
    let mut value = first_go_version();
    value["install_methods"][0]
        .as_object_mut()
        .expect("go version has an install method")
        .insert(
            "build_steps".into(),
            serde_json::json!([{ "command": "make" }]),
        );
    let err = serde_json::from_value::<ToolVersion>(value)
        .unwrap_err()
        .to_string();
    assert!(err.contains("build_steps"), "{err}");
}
