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
//! Why we don't fetch checksums: no install_method stores a checksum
//! in-file. tools/rust/ uses Tier 3 (`checksum_url_template`), where the
//! sha256 lives at the publisher and is fetched at install time. tools/node/
//! and tools/go/ use Tier 1 (`algorithm: "gpg"` + `gpg_keyring` +
//! `signature_url_template`); the keyring path and URL templates are
//! version-agnostic, so those blocks carry forward unchanged through the
//! template clone. Tier 2 (pinned checksum stored in-file) is not yet
//! exercised by any tool; when it lands, this module gains a checksum-fetch
//! step gated on `verification.tier == 2`.

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
        // Evidence belongs to the version it was recorded for. The template
        // is an existing entry, often one evidence-run has already proven, so
        // carrying its rows over would make a never-tested release claim
        // another version's results.
        obj.insert("tested".into(), json!([]));
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

/// Mutate `index.raw` in place: add new versions to `available[]`, keep it
/// in ascending version order, replace the entire `activity` block with
/// the recomputed values, and set `default_version` when the adapter chose
/// one.
///
/// A `default_version` missing from `available[]` (after the merge) is
/// ignored, so a release the adapter held back can never become the
/// default before its version file exists.
pub fn update_index(
    index: &mut ToolIndex,
    new_versions: &[String],
    signals: &ActivitySignals,
    score: Score,
    cadence_days: u32,
    default_version: Option<&str>,
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
    // A backfill adds versions older than every existing entry; a plain push
    // would list 1.82.0 after 1.99.0.
    available.sort_by_cached_key(|e| version_key(e.get("version").and_then(Value::as_str)));

    if let Some(dv) = default_version {
        let listed = available
            .iter()
            .any(|e| e.get("version").and_then(Value::as_str) == Some(dv));
        if listed {
            obj.insert("default_version".into(), Value::String(dv.to_string()));
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

/// The cataloged version a scanned `version` should be rendered from: the
/// one sharing the most leading numeric segments with it, newest on ties.
/// A release on an existing line therefore clones that line's newest file
/// (node 22.24.0 ← newest 22.x, go 1.27.4 ← newest 1.27.x) rather than the
/// newest file overall, whose dependencies and platform statuses can belong
/// to a different line (node 26's `libatomic`). With no shared segment it
/// falls back to the newest overall. `None` only for an empty catalog.
pub fn template_for(available: &[String], version: &str) -> Option<String> {
    let target = version_key(Some(version));
    available
        .iter()
        .max_by_key(|v| {
            let key = version_key(Some(v));
            let shared = key.iter().zip(&target).take_while(|(a, b)| a == b).count();
            (shared, key)
        })
        .cloned()
}

/// Sort key for a dotted numeric version: `1.9.0` < `1.10.0`. Non-numeric
/// segments sort as 0, and a missing version sorts first.
pub(crate) fn version_key(v: Option<&str>) -> Vec<u64> {
    v.unwrap_or("")
        .split('.')
        .map(|p| p.parse().unwrap_or(0))
        .collect()
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
    fn render_version_drops_template_evidence() {
        let mut t = template();
        t["tested"] = json!([{ "os": "debian", "arch": "amd64", "result": "pass" }]);
        let upstream = UpstreamVersion {
            version: "1.95.0".into(),
            released_at: at(2026, 5, 28),
            channel: "stable".into(),
        };
        let out = render_version(&t, "1.94.0", &upstream, at(2026, 5, 28));
        assert_eq!(
            out["tested"],
            json!([]),
            "a new version must not inherit evidence"
        );
    }

    #[test]
    fn update_index_keeps_available_in_version_order() {
        let mut idx = ToolIndex {
            id: "rust".into(),
            source_repo: None,
            raw: json!({ "available": [{ "version": "1.93.0" }, { "version": "1.99.0" }] }),
            path: std::path::PathBuf::from("/tmp/index.json"),
        };
        let signals = ActivitySignals::default();
        update_index(
            &mut idx,
            &["1.9.0".into(), "1.82.0".into()],
            &signals,
            Score::VeryActive,
            1,
            None,
            at(2026, 10, 12),
        );
        let order: Vec<&str> = idx.raw["available"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["version"].as_str().unwrap())
            .collect();
        assert_eq!(order, vec!["1.9.0", "1.82.0", "1.93.0", "1.99.0"]);
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
            None,
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
            None,
            at(2026, 5, 1),
        );
        assert_eq!(index.raw["activity"]["score"], "very-active");
        assert_eq!(index.raw["activity"]["scan_cadence_days"], 1);
        assert_eq!(index.raw["activity"]["scanned_at"], "2026-05-01T00:00:00Z");
        assert_eq!(index.raw["activity"]["signals"]["releases_last_90d"], 4);
    }

    fn index_with(available: &[&str], default_version: &str) -> ToolIndex {
        let available: Vec<Value> = available.iter().map(|v| json!({ "version": v })).collect();
        ToolIndex {
            id: "node".to_string(),
            source_repo: None,
            raw: json!({ "default_version": default_version, "available": available }),
            path: PathBuf::from("/tmp/index.json"),
        }
    }

    #[test]
    fn update_index_sets_a_listed_default_version() {
        let mut index = index_with(&["22.23.4", "24.22.1"], "22.23.4");
        update_index(
            &mut index,
            &["24.22.2".to_string()],
            &ActivitySignals::default(),
            Score::VeryActive,
            1,
            // Listed only because this same call adds it.
            Some("24.22.2"),
            at(2026, 11, 1),
        );
        assert_eq!(index.raw["default_version"], "24.22.2");
    }

    #[test]
    fn update_index_ignores_an_unlisted_default_version() {
        let mut index = index_with(&["22.23.4", "24.22.1"], "22.23.4");
        update_index(
            &mut index,
            &["24.22.2".to_string()],
            &ActivitySignals::default(),
            Score::VeryActive,
            1,
            Some("24.22.3"),
            at(2026, 11, 1),
        );
        assert_eq!(index.raw["default_version"], "22.23.4");
    }

    #[test]
    fn update_index_without_a_default_version_leaves_it_alone() {
        let mut index = index_with(&["22.23.4"], "22.23.4");
        update_index(
            &mut index,
            &["24.22.2".to_string()],
            &ActivitySignals::default(),
            Score::VeryActive,
            1,
            None,
            at(2026, 11, 1),
        );
        assert_eq!(index.raw["default_version"], "22.23.4");
    }

    fn owned(vs: &[&str]) -> Vec<String> {
        vs.iter().map(|v| v.to_string()).collect()
    }

    #[test]
    fn template_for_prefers_the_newest_file_on_the_same_line() {
        let available = owned(&["22.23.3", "22.23.4", "24.22.1", "26.12.0", "26.12.1"]);
        assert_eq!(
            template_for(&available, "22.23.5").as_deref(),
            Some("22.23.4")
        );
        assert_eq!(
            template_for(&available, "22.24.0").as_deref(),
            Some("22.23.4")
        );
        assert_eq!(
            template_for(&available, "24.23.0").as_deref(),
            Some("24.22.1")
        );
        assert_eq!(
            template_for(&available, "26.13.0").as_deref(),
            Some("26.12.1")
        );
    }

    #[test]
    fn template_for_matches_the_deepest_shared_prefix() {
        // go: a patch on the older supported branch clones that branch.
        let available = owned(&["1.26.8", "1.26.9", "1.27.2", "1.27.3"]);
        assert_eq!(
            template_for(&available, "1.26.10").as_deref(),
            Some("1.26.9")
        );
        assert_eq!(
            template_for(&available, "1.27.4").as_deref(),
            Some("1.27.3")
        );
        // A new minor shares only the major: newest on that major.
        assert_eq!(
            template_for(&available, "1.28.0").as_deref(),
            Some("1.27.3")
        );
    }

    #[test]
    fn template_for_new_line_falls_back_to_the_newest_overall() {
        let available = owned(&["22.23.4", "26.12.1", "24.22.1"]);
        assert_eq!(
            template_for(&available, "27.0.0").as_deref(),
            Some("26.12.1")
        );
        assert_eq!(template_for(&[], "27.0.0"), None);
    }

    #[test]
    fn template_for_orders_numerically_not_lexically() {
        let available = owned(&["1.9.0", "1.10.0"]);
        assert_eq!(
            template_for(&available, "1.11.0").as_deref(),
            Some("1.10.0")
        );
        assert_eq!(template_for(&available, "2.0.0").as_deref(), Some("1.10.0"));
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
