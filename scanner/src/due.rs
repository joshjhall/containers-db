//! Walk every `tools/<id>/index.json` and decide which tools are due
//! for a scan. A tool is due when
//! `activity.scanned_at + scan_cadence_days * 1d ≤ now`, or when it has
//! never been scanned (`scanned_at` absent).

use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;

/// Default cadence applied when a tool's `index.json` omits
/// `activity.scan_cadence_days`. Matches the `maintained` tier in
/// [`crate::activity`].
const DEFAULT_CADENCE_DAYS: u32 = 30;

#[derive(Debug)]
pub struct DueTool {
    pub id: String,
    pub index_path: PathBuf,
    pub scanned_at: Option<DateTime<Utc>>,
    pub scan_cadence_days: u32,
}

#[derive(Debug, Deserialize)]
struct RawIndex {
    id: Option<String>,
    #[serde(default)]
    activity: Option<RawActivity>,
}

#[derive(Debug, Default, Deserialize)]
struct RawActivity {
    #[serde(default)]
    scanned_at: Option<DateTime<Utc>>,
    #[serde(default)]
    scan_cadence_days: Option<u32>,
}

/// Walk `tools/*/index.json` under `repo_root` and return every tool
/// whose next scheduled scan is at or before `now`.
pub fn find_due_tools(repo_root: &Path, now: DateTime<Utc>) -> anyhow::Result<Vec<DueTool>> {
    let tools_root = repo_root.join("tools");
    if !tools_root.exists() {
        return Ok(Vec::new());
    }

    let mut due = Vec::new();
    for entry in std::fs::read_dir(&tools_root)? {
        let entry = entry?;
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let index_path = dir.join("index.json");
        if !index_path.exists() {
            continue;
        }
        let bytes = std::fs::read(&index_path)?;
        let raw: RawIndex = serde_json::from_slice(&bytes)
            .map_err(|e| anyhow::anyhow!("parsing {}: {e}", index_path.display()))?;
        let Some(id) = raw.id else {
            continue;
        };
        let activity = raw.activity.unwrap_or_default();
        let scan_cadence_days = activity.scan_cadence_days.unwrap_or(DEFAULT_CADENCE_DAYS);
        if is_due(activity.scanned_at, scan_cadence_days, now) {
            due.push(DueTool {
                id,
                index_path,
                scanned_at: activity.scanned_at,
                scan_cadence_days,
            });
        }
    }
    Ok(due)
}

/// `true` if the tool has never been scanned, or if
/// `scanned_at + cadence_days ≤ now`.
pub fn is_due(scanned_at: Option<DateTime<Utc>>, cadence_days: u32, now: DateTime<Utc>) -> bool {
    let Some(scanned_at) = scanned_at else {
        return true;
    };
    let next = scanned_at + Duration::days(i64::from(cadence_days));
    next <= now
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::fs;
    use tempfile::TempDir;

    fn at(year: i32, month: u32, day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, 0, 0, 0).unwrap()
    }

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn never_scanned_is_due() {
        assert!(is_due(None, 30, at(2026, 5, 1)));
    }

    #[test]
    fn just_scanned_is_not_due() {
        assert!(!is_due(Some(at(2026, 4, 30)), 30, at(2026, 5, 1)));
    }

    #[test]
    fn exactly_cadence_old_is_due() {
        // ≤ semantics: scanned exactly cadence_days ago → due now.
        assert!(is_due(Some(at(2026, 4, 1)), 30, at(2026, 5, 1)));
    }

    #[test]
    fn one_minute_overdue_is_due() {
        let scanned = Utc.with_ymd_and_hms(2026, 4, 1, 0, 0, 0).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 1).unwrap();
        assert!(is_due(Some(scanned), 30, now));
    }

    #[test]
    fn finds_due_tools_under_tools_directory() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(
            &root.join("tools/fresh/index.json"),
            r#"{
                "schemaVersion": 1, "id": "fresh", "kind": "cli",
                "activity": {
                    "score": "very-active",
                    "scan_cadence_days": 1,
                    "scanned_at": "2026-04-30T23:00:00Z"
                }
            }"#,
        );
        write(
            &root.join("tools/stale/index.json"),
            r#"{
                "schemaVersion": 1, "id": "stale", "kind": "cli",
                "activity": {
                    "score": "maintained",
                    "scan_cadence_days": 30,
                    "scanned_at": "2026-03-01T00:00:00Z"
                }
            }"#,
        );
        write(
            &root.join("tools/unscanned/index.json"),
            r#"{
                "schemaVersion": 1, "id": "unscanned", "kind": "cli",
                "activity": { "score": "maintained", "scanned_at": null }
            }"#,
        );
        // Sample-tool fixtures live under fixtures/, not tools/, so they
        // must NOT show up here.
        write(
            &root.join("fixtures/sample-tool/index.json"),
            r#"{ "schemaVersion": 1, "id": "sample", "kind": "cli" }"#,
        );

        let due = find_due_tools(root, at(2026, 5, 1)).unwrap();
        let ids: Vec<&str> = due.iter().map(|d| d.id.as_str()).collect();
        assert!(ids.contains(&"stale"));
        assert!(ids.contains(&"unscanned"));
        assert!(!ids.contains(&"fresh"));
        assert!(!ids.contains(&"sample"));
    }

    #[test]
    fn missing_cadence_defaults_to_thirty_days() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(
            &root.join("tools/x/index.json"),
            r#"{
                "schemaVersion": 1, "id": "x", "kind": "cli",
                "activity": { "score": "maintained", "scanned_at": "2026-03-31T00:00:00Z" }
            }"#,
        );
        let due = find_due_tools(root, at(2026, 5, 1)).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].scan_cadence_days, DEFAULT_CADENCE_DAYS);
    }

    #[test]
    fn skips_index_without_id() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(
            &root.join("tools/anon/index.json"),
            r#"{ "schemaVersion": 1, "kind": "cli" }"#,
        );
        let due = find_due_tools(root, at(2026, 5, 1)).unwrap();
        assert!(due.is_empty());
    }
}
