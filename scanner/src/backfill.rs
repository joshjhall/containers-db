//! `scanner backfill`: add releases older than the catalog's oldest entry.

use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::Scanned;
use crate::activity::compute_tier;
use crate::adapter::{Adapter, ToolIndex, UpstreamVersion};
use crate::stage::{self, Activity, Kind, Plan};

/// `scanner backfill`: add releases older than the catalog's oldest entry.
///
/// Mirrors [`crate::scan_one`]: builds the [`Plan`] the caller's mode then
/// writes, validates, and delivers. The selection is [`select_backfill`]
/// (explicit floor, below the oldest entry) and the template is the
/// *oldest* entry, whose support matrix and install methods are the
/// closest description of an older release. Writes nothing itself.
pub(crate) async fn backfill_one(
    repo_root: &Path,
    tool_id: &str,
    from: &str,
    adapter: Arc<dyn Adapter>,
    now: DateTime<Utc>,
) -> anyhow::Result<Scanned> {
    // Before any fetch: a malformed floor must fail, not select everything.
    let floor = parse_floor(from)?;
    let index_path = repo_root.join("tools").join(tool_id).join("index.json");
    let index = ToolIndex::from_path(&index_path)?;
    let result = adapter.fetch(&index).await?;
    let (score, cadence_days) = compute_tier(&result.signals, result.source_archived, now);

    let mut existing = index.available_versions();
    existing.sort_by_cached_key(|v| version_key(v));
    let template_version = existing.first().cloned().ok_or_else(|| {
        anyhow::anyhow!("tool `{tool_id}` has no existing versions to template from")
    })?;
    // The template itself is read by render_files, which errors if it is
    // missing or not JSON.
    let picked = select_backfill(&result.upstream_versions, &floor, &template_version);
    if picked.is_empty() {
        println!("scanner: {tool_id}: nothing to backfill from {from} below {template_version}");
        return Ok(Scanned::NoChange);
    }
    let kind = Kind::Backfill {
        from: from.to_string(),
    };
    let activity = Activity {
        score,
        cadence_days,
        signals: result.signals,
    };
    let files = stage::render_files(repo_root, tool_id, &kind, &picked, &activity, now)?;
    Ok(Scanned::Plan(Plan {
        tool_id: tool_id.to_string(),
        kind,
        new_versions: picked.iter().map(|v| v.version.clone()).collect(),
        releases: picked,
        now,
        activity,
        files,
    }))
}

/// Upstream stable releases `v` with `lo <= v < oldest`, ascending; `lo` is the
/// [`parse_floor`]-validated `--from` key.
///
/// Bounded above by the oldest cataloged version, so a backfill never
/// touches the range the forward scan owns; versions already present are
/// excluded by that bound.
fn select_backfill(upstream: &[UpstreamVersion], lo: &[u64], oldest: &str) -> Vec<UpstreamVersion> {
    let hi = version_key(oldest);
    let mut out: Vec<UpstreamVersion> = upstream
        .iter()
        .filter(|v| v.channel == "stable")
        .filter(|v| {
            let k = version_key(&v.version);
            k.as_slice() >= lo && k < hi
        })
        .cloned()
        .collect();
    out.sort_by_cached_key(|v| version_key(&v.version));
    out.dedup_by(|a, b| a.version == b.version);
    out
}

/// Sort key for a dotted numeric version: `1.9.0` < `1.10.0`.
fn version_key(v: &str) -> Vec<u64> {
    v.split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

/// Parse a user-supplied `--from` floor; every dot segment must be ASCII digits
/// that fit a `u64`.
///
/// Unlike [`version_key`], nothing defaults to `0`: `v1.82.0` would otherwise
/// key as `[0, 82, 0]` and select every release below the oldest entry. The
/// digit check comes first because `u64::from_str` alone accepts a leading `+`.
fn parse_floor(from: &str) -> anyhow::Result<Vec<u64>> {
    from.split('.')
        .map(|p| {
            if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            p.parse::<u64>().ok()
        })
        .collect::<Option<_>>()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "invalid --from `{from}`: expected a dotted numeric version like 1.82.0"
            )
        })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};

    use async_trait::async_trait;
    use chrono::TimeZone;
    use tempfile::TempDir;

    use super::*;
    use crate::activity::ActivitySignals;
    use crate::adapter::AdapterResult;

    /// Returns fixed upstream versions; never touches the network.
    struct StubAdapter(Vec<UpstreamVersion>);

    #[async_trait]
    impl Adapter for StubAdapter {
        async fn fetch(&self, _index: &ToolIndex) -> anyhow::Result<AdapterResult> {
            Ok(AdapterResult {
                upstream_versions: self.0.clone(),
                signals: ActivitySignals::default(),
                source_archived: false,
            })
        }
    }

    /// Like [`StubAdapter`], but records whether `fetch` was ever called.
    struct RecordingAdapter {
        upstream: Vec<UpstreamVersion>,
        fetched: Arc<AtomicBool>,
    }

    #[async_trait]
    impl Adapter for RecordingAdapter {
        async fn fetch(&self, _index: &ToolIndex) -> anyhow::Result<AdapterResult> {
            self.fetched.store(true, Ordering::SeqCst);
            Ok(AdapterResult {
                upstream_versions: self.upstream.clone(),
                signals: ActivitySignals::default(),
                source_archived: false,
            })
        }
    }

    /// A repo root holding `tools/demo/index.json` listing `available`, plus
    /// one `versions/<v>.json` per `(v, contents)` in `templates`.
    fn fixture(available: &[&str], templates: &[(&str, &str)]) -> TempDir {
        let tmp = TempDir::new().unwrap();
        let tool = tmp.path().join("tools").join("demo");
        std::fs::create_dir_all(tool.join("versions")).unwrap();
        let available: Vec<serde_json::Value> = available
            .iter()
            .map(|v| serde_json::json!({ "version": v }))
            .collect();
        let index = serde_json::json!({
            "schemaVersion": 1,
            "id": "demo",
            "source_repo": "https://github.com/example/demo",
            "available": available,
        });
        std::fs::write(tool.join("index.json"), index.to_string()).unwrap();
        for (v, contents) in templates {
            std::fs::write(tool.join("versions").join(format!("{v}.json")), contents).unwrap();
        }
        tmp
    }

    /// Every file under `root` with its bytes, so a before/after comparison
    /// catches a write anywhere in the tree, not just a new file count.
    fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut out = BTreeMap::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    let bytes = std::fs::read(&path).unwrap();
                    out.insert(path, bytes);
                }
            }
        }
        out
    }

    async fn run(
        root: &Path,
        from: &str,
        upstream: Vec<UpstreamVersion>,
    ) -> anyhow::Result<Scanned> {
        let now = Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap();
        backfill_one(root, "demo", from, Arc::new(StubAdapter(upstream)), now).await
    }

    const TEMPLATE: &str = r#"{"schemaVersion": 1, "version": "1.93.0"}"#;

    /// The `1.93.0` template path, joined segment by segment as
    /// `backfill_one` builds it so the separators match on every platform.
    fn template_path(root: &Path) -> PathBuf {
        root.join("tools")
            .join("demo")
            .join("versions")
            .join("1.93.0.json")
    }

    #[tokio::test]
    async fn no_change_when_nothing_upstream_falls_below_the_oldest_entry() {
        let tmp = fixture(&["1.93.0"], &[("1.93.0", TEMPLATE)]);
        let before = snapshot(tmp.path());
        let upstream = vec![
            up("1.93.0", 2026, 1, 22, "stable"),
            up("1.94.0", 2026, 3, 5, "stable"),
        ];
        let scanned = run(tmp.path(), "1.82.0", upstream).await.unwrap();
        assert!(matches!(scanned, Scanned::NoChange), "got {scanned:?}");
        assert_eq!(snapshot(tmp.path()), before);
    }

    #[tokio::test]
    async fn plans_the_selection_and_writes_nothing() {
        // Newest listed first: the bound must be the oldest entry by version,
        // not by file order, or 1.94.0 would be selected.
        let tmp = fixture(
            &["1.95.0", "1.93.0"],
            &[("1.95.0", TEMPLATE), ("1.93.0", TEMPLATE)],
        );
        let before = snapshot(tmp.path());
        let upstream = vec![
            up("1.85.0", 2025, 2, 20, "stable"),
            up("1.81.0", 2024, 9, 5, "stable"),
            up("1.82.0", 2024, 10, 17, "stable"),
            // A release the source reports twice is backfilled once.
            up("1.85.0", 2025, 2, 20, "stable"),
            up("1.90.0-beta.1", 2025, 8, 1, "beta"),
            up("1.94.0", 2026, 3, 5, "stable"),
        ];
        let scanned = run(tmp.path(), "1.82.0", upstream).await.unwrap();
        let Scanned::Plan(plan) = scanned else {
            panic!("expected a plan, got {scanned:?}");
        };
        assert_eq!(plan.new_versions, vec!["1.82.0", "1.85.0"]);
        assert_eq!(plan.prefix(), "backfill/demo/");
        assert_eq!(plan.branch(), "backfill/demo/1.82.0");
        assert_eq!(
            plan.title(),
            "feat(demo): backfill versions 1.82.0 to 1.85.0"
        );
        let paths: Vec<&str> = plan.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "tools/demo/versions/1.82.0.json",
                "tools/demo/versions/1.85.0.json",
                "tools/demo/index.json",
            ]
        );
        // A tampered or mis-built plan is refused at delivery; this one isn't.
        plan.check().unwrap();
        assert_eq!(snapshot(tmp.path()), before);
    }

    #[tokio::test]
    async fn rejects_a_malformed_from_before_fetching_or_writing() {
        // Everything but `--from` is valid, so a parse_floor moved below the
        // fetch is reached and still returns this error; only `fetched`
        // catches the reorder.
        let tmp = fixture(&["1.93.0"], &[("1.93.0", TEMPLATE)]);
        let before = snapshot(tmp.path());
        let fetched = Arc::new(AtomicBool::new(false));
        let adapter = RecordingAdapter {
            upstream: vec![up("1.82.0", 2024, 10, 17, "stable")],
            fetched: Arc::clone(&fetched),
        };
        let now = Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap();
        let err = backfill_one(tmp.path(), "demo", "v1.82.0", Arc::new(adapter), now)
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            "invalid --from `v1.82.0`: expected a dotted numeric version like 1.82.0"
        );
        assert!(
            !fetched.load(Ordering::SeqCst),
            "fetch must not run before --from is validated"
        );
        assert_eq!(snapshot(tmp.path()), before);
    }

    #[tokio::test]
    async fn errors_when_the_index_has_no_versions_to_template_from() {
        let tmp = fixture(&[], &[]);
        let upstream = vec![up("1.82.0", 2024, 10, 17, "stable")];
        let err = run(tmp.path(), "1.82.0", upstream)
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(err, "tool `demo` has no existing versions to template from");
    }

    #[tokio::test]
    async fn errors_when_the_template_file_is_missing() {
        let tmp = fixture(&["1.93.0"], &[]);
        let path = template_path(tmp.path());
        let upstream = vec![up("1.82.0", 2024, 10, 17, "stable")];
        let err = run(tmp.path(), "1.82.0", upstream)
            .await
            .unwrap_err()
            .to_string();
        let prefix = format!("reading template {}: ", path.display());
        assert!(err.starts_with(&prefix), "got {err:?}");
    }

    #[tokio::test]
    async fn errors_when_the_template_is_not_json() {
        let tmp = fixture(&["1.93.0"], &[("1.93.0", "not json")]);
        let path = template_path(tmp.path());
        let upstream = vec![up("1.82.0", 2024, 10, 17, "stable")];
        let err = run(tmp.path(), "1.82.0", upstream)
            .await
            .unwrap_err()
            .to_string();
        let prefix = format!("parsing template {}: ", path.display());
        assert!(err.starts_with(&prefix), "got {err:?}");
    }

    fn up(version: &str, y: i32, m: u32, d: u32, channel: &str) -> UpstreamVersion {
        UpstreamVersion {
            version: version.into(),
            released_at: Utc.with_ymd_and_hms(y, m, d, 0, 0, 0).unwrap(),
            channel: channel.into(),
        }
    }

    #[test]
    fn select_backfill_takes_floor_up_to_oldest_exclusive_in_version_order() {
        let upstream = vec![
            up("1.93.0", 2026, 1, 22, "stable"),
            up("1.85.1", 2025, 3, 23, "stable"),
            up("1.81.0", 2024, 9, 5, "stable"),
            up("1.82.0", 2024, 10, 17, "stable"),
            up("1.92.0", 2025, 12, 11, "stable"),
            up("1.85.0", 2025, 2, 20, "stable"),
            up("1.90.0-beta.1", 2025, 8, 1, "beta"),
        ];
        let ids: Vec<String> =
            select_backfill(&upstream, &parse_floor("1.82.0").unwrap(), "1.93.0")
                .into_iter()
                .map(|v| v.version)
                .collect();
        assert_eq!(ids, vec!["1.82.0", "1.85.0", "1.85.1", "1.92.0"]);
    }

    #[test]
    fn select_backfill_orders_numerically_not_lexically() {
        let upstream = vec![
            up("1.10.0", 2016, 7, 7, "stable"),
            up("1.9.0", 2016, 5, 26, "stable"),
        ];
        let ids: Vec<String> = select_backfill(&upstream, &parse_floor("1.9.0").unwrap(), "1.11.0")
            .into_iter()
            .map(|v| v.version)
            .collect();
        assert_eq!(ids, vec!["1.9.0", "1.10.0"]);
    }

    #[test]
    fn select_backfill_dedups_duplicates_that_are_not_adjacent_upstream() {
        // The duplicates are apart on input, so dedup only works after the sort.
        let upstream = vec![
            up("1.85.0", 2025, 2, 20, "stable"),
            up("1.84.0", 2025, 1, 9, "stable"),
            up("1.85.0", 2025, 2, 20, "stable"),
        ];
        let ids: Vec<String> =
            select_backfill(&upstream, &parse_floor("1.80.0").unwrap(), "1.93.0")
                .into_iter()
                .map(|v| v.version)
                .collect();
        assert_eq!(ids, vec!["1.84.0", "1.85.0"]);
    }

    #[test]
    fn select_backfill_is_empty_when_floor_is_at_or_above_oldest() {
        let upstream = vec![
            up("1.84.0", 2025, 1, 9, "stable"),
            up("1.85.0", 2025, 2, 20, "stable"),
            up("1.86.0", 2025, 4, 3, "stable"),
        ];
        assert!(select_backfill(&upstream, &parse_floor("1.85.0").unwrap(), "1.85.0").is_empty());
        assert!(select_backfill(&upstream, &parse_floor("1.86.0").unwrap(), "1.85.0").is_empty());
    }

    #[test]
    fn select_backfill_is_empty_for_empty_upstream() {
        assert!(select_backfill(&[], &parse_floor("1.0.0").unwrap(), "1.93.0").is_empty());
    }

    #[test]
    fn parse_floor_rejects_a_non_numeric_segment_naming_the_value() {
        for bad in [
            "v1.82.0",
            "1.82.0-rc1",
            "",
            "1..2",
            "1.82.",
            "+1.82.0",
            "1.+82.0",
            "99999999999999999999.0.0",
        ] {
            let err = parse_floor(bad).expect_err(bad).to_string();
            assert_eq!(
                err,
                format!("invalid --from `{bad}`: expected a dotted numeric version like 1.82.0")
            );
        }
    }

    #[test]
    fn parse_floor_accepts_a_dotted_numeric_version() {
        assert_eq!(parse_floor("1.82.0").unwrap(), vec![1, 82, 0]);
    }
}
