//! `scanner backfill`: add releases older than the catalog's oldest entry.

use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::activity::compute_tier;
use crate::adapter::{Adapter, ToolIndex, UpstreamVersion};
use crate::{Outcome, generate, pr, validate, which_ajv, write_file};

/// `scanner backfill`: add releases older than the catalog's oldest entry.
///
/// Mirrors [`crate::scan_one`]'s write → validate → PR path, but the selection is
/// [`select_backfill`] (explicit floor, below the oldest entry) and the
/// template is the *oldest* entry, whose support matrix and install methods
/// are the closest description of an older release. Errors propagate rather
/// than filing a tracking issue: a backfill is run by hand.
pub(crate) async fn backfill_one(
    repo_root: &Path,
    tool_id: &str,
    from: &str,
    adapter: Arc<dyn Adapter>,
    now: DateTime<Utc>,
    dry_run: bool,
) -> anyhow::Result<Outcome> {
    // Before any fetch or write: a malformed floor must fail, not select everything.
    let floor = parse_floor(from)?;
    let index_path = repo_root.join("tools").join(tool_id).join("index.json");
    let mut index = ToolIndex::from_path(&index_path)?;
    let result = adapter.fetch(&index).await?;
    let (score, cadence_days) = compute_tier(&result.signals, result.source_archived, now);

    let mut existing = index.available_versions();
    existing.sort_by_cached_key(|v| version_key(v));
    let template_version = existing.first().cloned().ok_or_else(|| {
        anyhow::anyhow!("tool `{tool_id}` has no existing versions to template from")
    })?;
    let template_path = repo_root
        .join("tools")
        .join(tool_id)
        .join("versions")
        .join(format!("{template_version}.json"));
    let template_bytes = std::fs::read(&template_path)
        .map_err(|e| anyhow::anyhow!("reading template {}: {e}", template_path.display()))?;
    let template: Value = serde_json::from_slice(&template_bytes)
        .map_err(|e| anyhow::anyhow!("parsing template {}: {e}", template_path.display()))?;

    let picked = select_backfill(&result.upstream_versions, &floor, &template_version);
    if picked.is_empty() {
        println!("scanner: {tool_id}: nothing to backfill from {from} below {template_version}");
        return Ok(Outcome::NoChange);
    }
    let versions: Vec<String> = picked.iter().map(|v| v.version.clone()).collect();

    if dry_run {
        println!(
            "scanner: {tool_id} (dry-run backfill): {} version(s)",
            versions.len()
        );
        for v in &versions {
            println!("  + {v}");
        }
        return Ok(Outcome::DryRun {
            new_versions: versions,
        });
    }

    for nv in &picked {
        let path = repo_root
            .join("tools")
            .join(tool_id)
            .join("versions")
            .join(format!("{}.json", nv.version));
        write_file(
            &path,
            &generate::render_version(&template, &template_version, nv, now),
        )?;
    }
    generate::update_index(
        &mut index,
        &versions,
        &result.signals,
        score,
        cadence_days,
        now,
    );
    write_file(&index.path, &index.raw)?;

    validate::validate_rules(repo_root)?;
    if which_ajv() {
        validate::validate_schemas(repo_root)?;
    }

    let last = versions.last().cloned().unwrap_or_default();
    let branch = format!("backfill/{tool_id}/{from}");
    let title = format!("feat({tool_id}): backfill versions {from} to {last}");
    let body = pr::pr_body(
        tool_id,
        &versions,
        index.source_repo.as_deref(),
        score,
        &result.signals,
        cadence_days,
    );
    let prefix = format!("backfill/{tool_id}/");
    let delivery = pr::deliver(
        repo_root, tool_id, &prefix, &branch, &versions, &title, &body,
    )?;
    Ok(Outcome::delivered(delivery, versions))
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
    use super::*;
    use chrono::TimeZone;

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
