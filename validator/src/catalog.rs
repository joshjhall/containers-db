//! Catalog discovery: walk `tools/` and `fixtures/` (excluding
//! `_negative/`), parse every `index.json` into a [`ToolMeta`] map.
//!
//! The catalog is the resolution surface for [`crate::rules`]:
//!   - Rule 1 (unknown tool) needs to know which ids exist.
//!   - Rule 2 (no pin on system_package) needs each tool's `kind`.
//!   - Rule 3 (platform narrowing) needs the system_package platform map.
//!   - Rule 4 (parseable constraint) needs each tool's `version_style`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use containers_common::version::VersionStyle;
use serde::Deserialize;
use walkdir::WalkDir;

/// Per-tool metadata extracted from `tools/<id>/index.json` and
/// `fixtures/sample-tool/index.json`.
#[derive(Debug, Clone)]
pub struct ToolMeta {
    pub id: String,
    pub kind: String,
    pub version_style: VersionStyle,
    /// Set when `kind == "system_package"`; otherwise `None`.
    pub system_package_platforms: Option<BTreeSet<String>>,
    /// Path to the `index.json` this metadata came from. Used for
    /// diagnostic output.
    pub source: PathBuf,
}

/// Loaded catalog: id → metadata.
#[derive(Debug, Default)]
pub struct Catalog {
    entries: BTreeMap<String, ToolMeta>,
}

impl Catalog {
    pub fn get(&self, id: &str) -> Option<&ToolMeta> {
        self.entries.get(id)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[derive(Debug, Deserialize)]
struct RawIndex {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    version_style: Option<String>,
    #[serde(default)]
    system_package: Option<RawSystemPackage>,
}

#[derive(Debug, Deserialize)]
struct RawSystemPackage {
    #[serde(default)]
    platforms: BTreeMap<String, serde_json::Value>,
}

/// Walk the repo and load every catalog `index.json`.
///
/// Skips `_negative/` (intentionally broken fixtures), `node_modules/`,
/// and any `target/` (Rust build artifacts).
pub fn load(repo_root: &Path) -> Result<Catalog, CatalogError> {
    let mut catalog = Catalog::default();

    for root in [repo_root.join("tools"), repo_root.join("fixtures")] {
        if !root.exists() {
            continue;
        }
        for entry in WalkDir::new(&root).into_iter().filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            !(name == "_negative" || name == "node_modules" || name == "target")
        }) {
            let entry = entry.map_err(|e| CatalogError::Walk(e.to_string()))?;
            if !entry.file_type().is_file() {
                continue;
            }
            if entry.file_name() != "index.json" {
                continue;
            }

            let path = entry.into_path();
            let bytes = std::fs::read(&path).map_err(|e| CatalogError::Read {
                path: path.clone(),
                source: e,
            })?;
            let raw: RawIndex =
                serde_json::from_slice(&bytes).map_err(|e| CatalogError::Parse {
                    path: path.clone(),
                    source: e,
                })?;

            // Tool indexes without an `id` are scaffolding stubs — silently
            // skip (matches the JS behavior).
            let Some(id) = raw.id else {
                continue;
            };
            let kind = raw.kind.unwrap_or_default();

            let version_style = match raw.version_style.as_deref() {
                Some("semver") | None => VersionStyle::Semver,
                Some("prefix") => VersionStyle::Prefix,
                Some("calver") => VersionStyle::Calver,
                Some("opaque") => VersionStyle::Opaque,
                Some(other) => {
                    return Err(CatalogError::UnknownVersionStyle {
                        path,
                        style: other.to_string(),
                    });
                }
            };

            let system_package_platforms = if kind == "system_package" {
                Some(
                    raw.system_package
                        .map(|sp| sp.platforms.keys().cloned().collect())
                        .unwrap_or_default(),
                )
            } else {
                None
            };

            catalog.entries.insert(
                id.clone(),
                ToolMeta {
                    id,
                    kind,
                    version_style,
                    system_package_platforms,
                    source: path,
                },
            );
        }
    }

    Ok(catalog)
}

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("walk error: {0}")]
    Walk(String),

    #[error("failed to read {}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse {}", path.display())]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("{}: unknown version_style `{style}`", path.display())]
    UnknownVersionStyle { path: PathBuf, style: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn loads_cli_and_system_package_indexes() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(
            &root.join("tools/foo/index.json"),
            r#"{ "schemaVersion": 1, "id": "foo", "kind": "cli" }"#,
        );
        write(
            &root.join("tools/bar/index.json"),
            r#"{
                "schemaVersion": 1,
                "id": "bar",
                "kind": "system_package",
                "system_package": {
                    "platforms": {
                        "debian": { "name": "bar" },
                        "alpine": { "name": "bar" }
                    }
                }
            }"#,
        );
        let catalog = load(root).unwrap();
        assert_eq!(catalog.len(), 2);
        assert_eq!(catalog.get("foo").unwrap().kind, "cli");
        assert!(catalog.get("foo").unwrap().system_package_platforms.is_none());
        let bar = catalog.get("bar").unwrap();
        assert_eq!(bar.kind, "system_package");
        let plats = bar.system_package_platforms.as_ref().unwrap();
        assert!(plats.contains("debian") && plats.contains("alpine"));
    }

    #[test]
    fn skips_indexes_without_an_id() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(
            &root.join("tools/orphan/index.json"),
            r#"{ "schemaVersion": 1, "kind": "cli" }"#,
        );
        let catalog = load(root).unwrap();
        assert_eq!(catalog.len(), 0);
    }

    #[test]
    fn skips_negative_fixture_directory() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(
            &root.join("fixtures/_negative/should-not-load/index.json"),
            r#"{ "schemaVersion": 1, "id": "ghost", "kind": "cli" }"#,
        );
        write(
            &root.join("tools/real/index.json"),
            r#"{ "schemaVersion": 1, "id": "real", "kind": "cli" }"#,
        );
        let catalog = load(root).unwrap();
        assert!(catalog.get("ghost").is_none());
        assert!(catalog.get("real").is_some());
    }

    #[test]
    fn defaults_version_style_to_semver_when_omitted() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(
            &root.join("tools/foo/index.json"),
            r#"{ "schemaVersion": 1, "id": "foo", "kind": "cli" }"#,
        );
        let catalog = load(root).unwrap();
        assert_eq!(catalog.get("foo").unwrap().version_style, VersionStyle::Semver);
    }

    #[test]
    fn rejects_unknown_version_style() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(
            &root.join("tools/foo/index.json"),
            r#"{ "schemaVersion": 1, "id": "foo", "kind": "cli", "version_style": "lunar" }"#,
        );
        let err = load(root).unwrap_err();
        assert!(
            matches!(err, CatalogError::UnknownVersionStyle { ref style, .. } if style == "lunar"),
        );
    }

    #[test]
    fn surfaces_path_in_parse_error() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let bad = root.join("tools/broken/index.json");
        write(&bad, "{ not valid json");
        let err = load(root).unwrap_err();
        match err {
            CatalogError::Parse { path, .. } => assert_eq!(path, bad),
            other => panic!("expected Parse error, got {other:?}"),
        }
    }
}
