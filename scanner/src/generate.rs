//! Render new `versions/<v>.json` files from a tool's most recent
//! existing version, and refresh the `activity` block in
//! `tools/<id>/index.json`.
//!
//! Approach: deep-clone the template, swap top-level fields
//! (`version`, `released`, `metadata.{added,updated}_at`), then walk
//! the rest of the JSON and replace any remaining literal occurrence of
//! the old version string. That last step catches places like
//! `install_methods[].invoke.args[N]` where rust hard-codes the version
//! in the rustup invocation, without forcing those callsites to use a
//! template placeholder.
//!
//! Why we don't fetch checksums: every install_method in tools/rust/
//! today uses Tier 3 (`checksum_url_template`), which means the actual
//! sha256 lives at the publisher and is fetched at install time. Tier 2
//! (pinned checksum stored in-file) is not yet exercised by any tool;
//! when it lands, this module gains a checksum-fetch step gated on
//! `verification.tier == 2`.

use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Value, json};

use crate::activity::{ActivitySignals, Score};
use crate::adapter::{ToolIndex, UpstreamVersion};

/// Build a new `versions/<v>.json` JSON value from `template` (the most
/// recent existing version file for the same tool) and the upstream
/// release we want to record.
pub fn render_version(
    template: &Value,
    old_version: &str,
    new: &UpstreamVersion,
    now: DateTime<Utc>,
) -> Value {
    let mut out = template.clone();
    let now_str = now.to_rfc3339_opts(SecondsFormat::Secs, true);

    if let Some(obj) = out.as_object_mut() {
        obj.insert("version".into(), Value::String(new.version.clone()));
        obj.insert(
            "released".into(),
            Value::String(new.released_at.format("%Y-%m-%d").to_string()),
        );
        let metadata = obj.entry("metadata").or_insert(json!({}));
        if let Some(m) = metadata.as_object_mut() {
            m.insert("added_at".into(), Value::String(now_str.clone()));
            m.insert("updated_at".into(), Value::String(now_str));
            m.entry("schema_version").or_insert(json!(1));
        }
    }

    // Catch literal version strings buried in install_methods (e.g.,
    // `invoke.args[N]`). The `version` field above is also rewritten
    // here but redundantly — explicit set + deep walk is cheap and
    // tolerant of template variations.
    rewrite_string_matches(&mut out, old_version, &new.version);
    out
}

/// Mutate `index.raw` in place: append new versions to `available[]`
/// and replace the entire `activity` block with the recomputed values.
pub fn update_index(
    index: &mut ToolIndex,
    new_versions: &[String],
    signals: &ActivitySignals,
    score: Score,
    cadence_days: u32,
    now: DateTime<Utc>,
) {
    let now_str = now.to_rfc3339_opts(SecondsFormat::Secs, true);

    let Some(obj) = index.raw.as_object_mut() else {
        return;
    };

    let available = obj
        .entry("available")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .expect("available is array");
    for v in new_versions {
        let already = available
            .iter()
            .any(|e| e.get("version").and_then(Value::as_str) == Some(v.as_str()));
        if !already {
            available.push(json!({ "version": v }));
        }
    }

    obj.insert(
        "activity".into(),
        json!({
            "score": score,
            "signals": signals,
            "scan_cadence_days": cadence_days,
            "scanned_at": now_str,
        }),
    );
}

/// Walk every string in `value` and replace whole-string matches of
/// `from` with `to`. Substring matches are deliberately ignored to
/// avoid corrupting URL templates that contain version-shaped text.
fn rewrite_string_matches(value: &mut Value, from: &str, to: &str) {
    match value {
        Value::String(s) if s == from => *s = to.to_string(),
        Value::String(_) => {}
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                rewrite_string_matches(v, from, to);
            }
        }
        Value::Object(map) => {
            for v in map.values_mut() {
                rewrite_string_matches(v, from, to);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::path::PathBuf;

    fn at(year: i32, month: u32, day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, 0, 0, 0).unwrap()
    }

    fn template() -> Value {
        json!({
            "schemaVersion": 1,
            "tool": "rust",
            "version": "1.95.0",
            "released": "2026-04-16",
            "channel": "stable",
            "install_methods": [
                {
                    "name": "rustup-init",
                    "invoke": {
                        "args": ["-y", "--default-toolchain", "1.95.0", "--profile", "default"]
                    }
                }
            ],
            "metadata": {
                "added_at": "2026-04-27T22:00:00Z",
                "updated_at": "2026-04-27T22:00:00Z",
                "schema_version": 1
            }
        })
    }

    #[test]
    fn render_version_replaces_top_level_fields_and_invoke_args() {
        let upstream = UpstreamVersion {
            version: "1.96.0".to_string(),
            released_at: Utc.with_ymd_and_hms(2026, 5, 28, 0, 0, 0).unwrap(),
            channel: "stable".to_string(),
        };
        let out = render_version(&template(), "1.95.0", &upstream, at(2026, 5, 28));

        assert_eq!(out["version"], "1.96.0");
        assert_eq!(out["released"], "2026-05-28");
        assert_eq!(
            out["install_methods"][0]["invoke"]["args"][2], "1.96.0",
            "version literal in rustup args must be rewritten"
        );
        assert_eq!(out["metadata"]["added_at"], "2026-05-28T00:00:00Z");
        assert_eq!(out["metadata"]["updated_at"], "2026-05-28T00:00:00Z");
        assert_eq!(out["metadata"]["schema_version"], 1);
    }

    #[test]
    fn render_version_preserves_static_fields() {
        let upstream = UpstreamVersion {
            version: "1.96.0".to_string(),
            released_at: Utc.with_ymd_and_hms(2026, 5, 28, 0, 0, 0).unwrap(),
            channel: "stable".to_string(),
        };
        let out = render_version(&template(), "1.95.0", &upstream, at(2026, 5, 28));
        assert_eq!(out["schemaVersion"], 1);
        assert_eq!(out["tool"], "rust");
        assert_eq!(out["channel"], "stable");
        assert_eq!(out["install_methods"][0]["name"], "rustup-init");
    }

    #[test]
    fn update_index_appends_new_versions_without_duplicates() {
        let mut index = ToolIndex {
            id: "rust".to_string(),
            source_repo: None,
            raw: json!({
                "id": "rust",
                "available": [
                    { "version": "1.93.0" },
                    { "version": "1.94.0" }
                ]
            }),
            path: PathBuf::from("/tmp/index.json"),
        };
        let signals = ActivitySignals {
            releases_last_90d: Some(2),
            ..ActivitySignals::default()
        };
        update_index(
            &mut index,
            &["1.94.0".to_string(), "1.95.0".to_string()],
            &signals,
            Score::Active,
            7,
            at(2026, 5, 1),
        );
        let versions: Vec<&str> = index.raw["available"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["version"].as_str().unwrap())
            .collect();
        assert_eq!(versions, vec!["1.93.0", "1.94.0", "1.95.0"]);
    }

    #[test]
    fn update_index_writes_full_activity_block() {
        let mut index = ToolIndex {
            id: "rust".to_string(),
            source_repo: None,
            raw: json!({ "id": "rust" }),
            path: PathBuf::from("/tmp/index.json"),
        };
        let signals = ActivitySignals {
            releases_last_90d: Some(4),
            last_release_at: Some(at(2026, 4, 25)),
            ..ActivitySignals::default()
        };
        update_index(
            &mut index,
            &["1.96.0".to_string()],
            &signals,
            Score::VeryActive,
            1,
            at(2026, 5, 1),
        );
        assert_eq!(index.raw["activity"]["score"], "very-active");
        assert_eq!(index.raw["activity"]["scan_cadence_days"], 1);
        assert_eq!(index.raw["activity"]["scanned_at"], "2026-05-01T00:00:00Z");
        assert_eq!(index.raw["activity"]["signals"]["releases_last_90d"], 4);
    }

    #[test]
    fn rewrite_string_matches_only_replaces_whole_strings() {
        let mut v = json!({
            "exact": "1.95.0",
            "substr": "version 1.95.0 released",
            "nested": ["1.95.0", "unrelated"]
        });
        rewrite_string_matches(&mut v, "1.95.0", "1.96.0");
        assert_eq!(v["exact"], "1.96.0");
        // Substring is not replaced to avoid corrupting prose / URL fragments.
        assert_eq!(v["substr"], "version 1.95.0 released");
        assert_eq!(v["nested"][0], "1.96.0");
        assert_eq!(v["nested"][1], "unrelated");
    }
}
