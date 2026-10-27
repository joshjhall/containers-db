//! Pins node's platform shape: the 18-cell base-image matrix, the per-arch
//! glibc tarball methods, and the alpine source-build method (#483).
//!
//! The scanner has no per-tool version template — `render_version` clones
//! the newest cataloged version file — so the catalog data *is* the template.
//! A cataloged file that drops a cell, or a build dependency that loses its
//! `purpose: build` scope, would be carried into every release the scanner
//! adds after it. These tests read the real catalog rather than a fixture for
//! exactly that reason. See docs/source-build-musl.md.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use chrono::{TimeZone, Utc};
use scanner::adapter::{ToolIndex, UpstreamVersion};
use scanner::generate::render_version;
use serde_json::{Value, json};

const GLIBC_OS: [&str; 3] = ["debian", "ubuntu", "rhel"];

/// Every `(os, os_version, arch)` cell node must claim. Alpine cells may be
/// `unsupported` (the python floor), but never absent.
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

fn node_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../tools/node")
}

fn read_json(path: &Path) -> Value {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_slice(&bytes).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// `(tool, purpose)` pairs of a method's dependencies, in order. An absent
/// `purpose` is reported as the schema default, `both`.
fn deps(method: &Value, label: &str) -> Vec<(String, String)> {
    method["dependencies"]
        .as_array()
        .unwrap_or_else(|| panic!("{label}: no dependencies"))
        .iter()
        .map(|d| {
            (
                d["tool"].as_str().unwrap().to_string(),
                d["purpose"].as_str().unwrap_or("both").to_string(),
            )
        })
        .collect()
}

fn owned(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(t, p)| (t.to_string(), p.to_string()))
        .collect()
}

fn assert_node_shape(doc: &Value, label: &str) {
    let rows = doc["support_matrix"]
        .as_array()
        .unwrap_or_else(|| panic!("{label}: no support_matrix"));
    let cells: BTreeSet<_> = rows
        .iter()
        .map(|r| {
            let os = r["os"].as_str().unwrap();
            // glibc cells are always claimed. An alpine cell may be refused
            // by the python floor, but then it must say why: luggage surfaces
            // the reason in its UnsupportedPlatform error.
            match r["status"].as_str().unwrap() {
                "supported" => {}
                "unsupported" if os == "alpine" => assert!(
                    r["reason"].as_str().is_some_and(|s| !s.is_empty()),
                    "{label}: unsupported alpine row has no reason: {r}"
                ),
                other => panic!("{label}: {os} row has status {other}: {r}"),
            }
            (
                os.to_string(),
                r["os_version"].as_str().unwrap().to_string(),
                r["arch"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(cells, expected_cells(), "{label}: support_matrix cells");
    assert_eq!(rows.len(), 18, "{label}: duplicate support_matrix rows");

    let methods = doc["install_methods"]
        .as_array()
        .unwrap_or_else(|| panic!("{label}: no install_methods"));
    let names: Vec<&str> = methods
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["node-tarball-amd64", "node-tarball-arm64", "node-source"],
        "{label}: install_methods"
    );

    // glibc: the official tarball, one method per arch. libatomic only on
    // arm64, where node 26+ links libatomic.so.1 (#481).
    for (method, arch) in methods[..2].iter().zip(["amd64", "arm64"]) {
        let name = format!("{label}: {}", method["name"]);
        assert_eq!(method["method_kind"], "binary-tarball", "{name}");
        assert_eq!(method["platform"]["os"], json!(GLIBC_OS), "{name} os");
        assert_eq!(method["platform"]["arch"], json!([arch]), "{name} arch");
        let want: &[(&str, &str)] = if arch == "arm64" {
            &[("ca_certificates", "both"), ("libatomic", "runtime")]
        } else {
            &[("ca_certificates", "both")]
        };
        assert_eq!(deps(method, &name), owned(want), "{name} dependencies");
    }

    // alpine: nodejs.org publishes no musl tarball, so build from the
    // official source tarball, verified against the same SHASUMS256.txt.
    let source = &methods[2];
    let name = format!("{label}: node-source");
    assert_eq!(source["method_kind"], "source-build", "{name}");
    assert_eq!(source["platform"]["os"], "alpine", "{name} os");
    assert_eq!(
        source["platform"]["arch"],
        json!(["amd64", "arm64"]),
        "{name} arch"
    );
    assert_eq!(
        source["source_url_template"], "https://nodejs.org/dist/v{version}/node-v{version}.tar.xz",
        "{name} source_url_template"
    );
    assert_eq!(
        source["verification"], methods[0]["verification"],
        "{name} must verify exactly as the glibc tarball does"
    );
    assert!(
        source["build_steps"]
            .as_array()
            .is_some_and(|s| !s.is_empty()),
        "{name}: no build_steps"
    );
    // The toolchain is build-only; the musl binary needs no libatomic, unlike
    // the glibc arm64 tarball (measured, see the #483 spike).
    assert_eq!(
        deps(source, &name),
        owned(&[
            ("ca_certificates", "both"),
            ("gxx", "build"),
            ("make", "build"),
            ("linux_headers", "build"),
            ("python3", "build"),
            ("libstdcxx", "runtime"),
            ("libgcc", "runtime"),
        ]),
        "{name} dependencies"
    );
}

#[test]
fn every_cataloged_node_version_has_the_full_matrix() {
    let dir = node_dir().join("versions");
    let mut seen = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) == Some("json") {
            assert_node_shape(&read_json(&path), &path.display().to_string());
            seen += 1;
        }
    }
    assert!(seen > 0, "no node version files under {}", dir.display());
}

#[test]
fn a_newly_generated_node_version_keeps_the_full_matrix() {
    // Pick the template exactly as `scan_one` does: the last `available`
    // entry of the tool index.
    let index = ToolIndex::from_path(&node_dir().join("index.json")).unwrap();
    let template_version = index.available_versions().last().cloned().unwrap();
    let template = read_json(
        &node_dir()
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
    assert_node_shape(&rendered, "rendered node 99.0.0");
}
