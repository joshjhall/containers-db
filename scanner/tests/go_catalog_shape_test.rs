//! Pins go's platform shape: the 18-cell base-image matrix and the
//! glibc/alpine install-method split (#482).
//!
//! The scanner has no per-tool version template — `render_version` clones
//! the newest cataloged version file — so the catalog data *is* the template.
//! A cataloged file that drops a cell would be carried into every release the
//! scanner adds after it. These tests read the real catalog rather than a
//! fixture for exactly that reason.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use chrono::{TimeZone, Utc};
use scanner::adapter::{ToolIndex, UpstreamVersion};
use scanner::generate::render_version;
use serde_json::Value;

const GLIBC_OS: [&str; 3] = ["debian", "ubuntu", "rhel"];

/// Every `(os, os_version, arch)` cell go must claim.
fn expected_cells() -> BTreeSet<(String, String, String)> {
    let releases = [
        ("debian", "12"),
        ("debian", "13"),
        ("ubuntu", "24.04"),
        ("ubuntu", "26.04"),
        ("alpine", "3.22"),
        ("alpine", "3.23"),
        ("alpine", "3.24"),
        ("rhel", "9"),
        ("rhel", "10"),
    ];
    releases
        .iter()
        .flat_map(|(os, v)| {
            ["amd64", "arm64"].map(|arch| (os.to_string(), v.to_string(), arch.to_string()))
        })
        .collect()
}

fn go_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../tools/go")
}

fn read_json(path: &Path) -> Value {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_slice(&bytes).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// `platform.os` is a string or an array of strings.
fn method_os(method: &Value) -> BTreeSet<String> {
    match &method["platform"]["os"] {
        Value::String(s) => BTreeSet::from([s.clone()]),
        Value::Array(a) => a
            .iter()
            .map(|v| v.as_str().expect("os entry is a string").to_string())
            .collect(),
        other => panic!("unexpected platform.os: {other}"),
    }
}

fn assert_go_shape(doc: &Value, label: &str) {
    let cells: BTreeSet<_> = doc["support_matrix"]
        .as_array()
        .unwrap_or_else(|| panic!("{label}: no support_matrix"))
        .iter()
        .map(|r| {
            assert_eq!(r["status"], "supported", "{label}: {r}");
            (
                r["os"].as_str().unwrap().to_string(),
                r["os_version"].as_str().unwrap().to_string(),
                r["arch"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(cells, expected_cells(), "{label}: support_matrix cells");
    assert_eq!(
        doc["support_matrix"].as_array().unwrap().len(),
        18,
        "{label}: duplicate support_matrix rows"
    );

    // A musl-only change must never touch the glibc method, so alpine lives
    // in a method of its own.
    let methods: Vec<BTreeSet<String>> = doc["install_methods"]
        .as_array()
        .unwrap_or_else(|| panic!("{label}: no install_methods"))
        .iter()
        .map(method_os)
        .collect();
    let glibc: BTreeSet<String> = GLIBC_OS.iter().map(|s| s.to_string()).collect();
    assert_eq!(
        methods,
        vec![glibc, BTreeSet::from(["alpine".to_string()])],
        "{label}: install_methods must be [glibc, alpine]"
    );

    // Both sides of the split are the same tarball install: no C toolchain
    // (cgo is out, see docs/binary-tarball-glibc-musl.md) and both arches.
    for method in doc["install_methods"].as_array().unwrap() {
        let name = method_os(method).into_iter().collect::<Vec<_>>().join("+");
        assert_eq!(
            method["method_kind"], "binary-tarball",
            "{label}: {name} method_kind"
        );
        let deps: Vec<&str> = method["dependencies"]
            .as_array()
            .unwrap_or_else(|| panic!("{label}: {name} has no dependencies"))
            .iter()
            .map(|d| d["tool"].as_str().unwrap())
            .collect();
        assert_eq!(deps, ["ca_certificates"], "{label}: {name} dependencies");
        assert_eq!(
            method["platform"]["arch"],
            serde_json::json!(["amd64", "arm64"]),
            "{label}: {name} arch"
        );
    }
}

#[test]
fn every_cataloged_go_version_has_the_full_matrix() {
    let dir = go_dir().join("versions");
    let mut seen = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) == Some("json") {
            assert_go_shape(&read_json(&path), &path.display().to_string());
            seen += 1;
        }
    }
    assert!(seen > 0, "no go version files under {}", dir.display());
}

#[test]
fn a_newly_generated_go_version_keeps_the_full_matrix() {
    // Pick the template exactly as `scan_one` does: the last `available`
    // entry of the tool index.
    let index = ToolIndex::from_path(&go_dir().join("index.json")).unwrap();
    let template_version = index.available_versions().last().cloned().unwrap();
    let template = read_json(
        &go_dir()
            .join("versions")
            .join(format!("{template_version}.json")),
    );

    let new = UpstreamVersion {
        version: "99.0.0".to_string(),
        released_at: Utc.with_ymd_and_hms(2099, 1, 1, 0, 0, 0).unwrap(),
        channel: "stable".to_string(),
    };
    let rendered = render_version(&template, &template_version, &new, Utc::now());

    assert_eq!(rendered["version"], "99.0.0");
    assert_go_shape(&rendered, "rendered go 99.0.0");
}
