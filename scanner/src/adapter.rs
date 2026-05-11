//! Adapter contract — every per-tool source plugin implements
//! [`Adapter`]. Adapters fetch upstream truth (versions + activity
//! signals); orchestration (diffing against `available[]`, generation,
//! PR/issue creation, tier mapping) lives outside the trait.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::activity::ActivitySignals;

/// The current state of a `tools/<id>/index.json` file. Adapters read
/// `source_repo` and the existing `available[]` set; the scanner mutates
/// `raw` in-place before writing the file back.
#[derive(Debug, Clone)]
pub struct ToolIndex {
    pub id: String,
    pub source_repo: Option<String>,
    /// Full parsed JSON of the index file.
    pub raw: serde_json::Value,
    /// Absolute path on disk; used by the scanner when writing the
    /// updated index back.
    pub path: PathBuf,
}

impl ToolIndex {
    pub fn from_path(path: &Path) -> anyhow::Result<Self> {
        let bytes =
            std::fs::read(path).map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
        let raw: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|e| anyhow::anyhow!("parsing {}: {e}", path.display()))?;
        let id = raw
            .get("id")
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| anyhow::anyhow!("{}: missing `id`", path.display()))?;
        let source_repo = raw
            .get("source_repo")
            .and_then(|v| v.as_str())
            .map(String::from);
        Ok(Self {
            id,
            source_repo,
            raw,
            path: path.to_path_buf(),
        })
    }

    /// Versions currently listed under `available[].version`, in file
    /// order. Used by the scanner to diff against [`AdapterResult::upstream_versions`].
    pub fn available_versions(&self) -> Vec<String> {
        self.raw
            .get("available")
            .and_then(serde_json::Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.get("version").and_then(|s| s.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// One upstream release, normalized across source types.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpstreamVersion {
    /// Bare version string (no `v` prefix), e.g. `"1.95.0"`.
    pub version: String,
    pub released_at: DateTime<Utc>,
    /// Release channel — `"stable"`, `"beta"`, etc. The scanner only
    /// auto-opens PRs for `"stable"`; other channels are surfaced in
    /// the result so adapters can return them for inspection.
    pub channel: String,
}

#[derive(Debug, Clone)]
pub struct AdapterResult {
    pub upstream_versions: Vec<UpstreamVersion>,
    pub signals: ActivitySignals,
    /// `true` when the upstream source is archived, disabled, or
    /// otherwise declared EOL — promotes the tool to [`crate::activity::Score::Abandoned`].
    pub source_archived: bool,
}

#[async_trait]
pub trait Adapter: Send + Sync {
    async fn fetch(&self, index: &ToolIndex) -> anyhow::Result<AdapterResult>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn parses_id_source_repo_and_available_versions() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("index.json");
        fs::write(
            &path,
            r#"{
                "schemaVersion": 1,
                "id": "rust",
                "source_repo": "https://github.com/rust-lang/rust",
                "available": [
                    { "version": "1.93.0" },
                    { "version": "1.94.0" },
                    { "version": "1.95.0" }
                ]
            }"#,
        )
        .unwrap();
        let idx = ToolIndex::from_path(&path).unwrap();
        assert_eq!(idx.id, "rust");
        assert_eq!(
            idx.source_repo.as_deref(),
            Some("https://github.com/rust-lang/rust")
        );
        assert_eq!(
            idx.available_versions(),
            vec![
                "1.93.0".to_string(),
                "1.94.0".to_string(),
                "1.95.0".to_string()
            ]
        );
    }

    #[test]
    fn missing_id_is_an_error() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("index.json");
        fs::write(&path, r#"{ "schemaVersion": 1, "kind": "cli" }"#).unwrap();
        let err = ToolIndex::from_path(&path).unwrap_err();
        assert!(err.to_string().contains("missing `id`"));
    }
}
