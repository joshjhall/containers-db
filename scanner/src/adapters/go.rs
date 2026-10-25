//! `go` adapter — reads go.dev's download feed for stable releases and the
//! release-history page for their dates.
//!
//! Two calls per scan:
//!   1. `GET {base}/dl/?mode=json&include=all` — every version + its
//!      `stable` flag
//!   2. `GET {base}/doc/devel/release` — `(released YYYY-MM-DD)` per version
//!
//! The download feed carries no dates, and `golang/go` publishes no GitHub
//! releases, so the history page is the only upstream source whose dates
//! match the catalog's `released` field (the tarball `Last-Modified` header
//! is UTC and lands a day late for evening-Pacific releases). The scanner
//! uses `released` as its high-water mark, so a stable version the history
//! page does not date yet is skipped rather than guessed — the next scan
//! picks it up.
//!
//! Only `releases_last_90d` and `last_release_at` are computed; there is no
//! repo or advisory call, so `last_commit_at` and `open_advisories` stay
//! unset.

use std::collections::HashMap;

use async_trait::async_trait;
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::Deserialize;

use crate::activity::ActivitySignals;
use crate::adapter::{Adapter, AdapterResult, ToolIndex, UpstreamVersion};
use crate::adapters::rust::is_strict_semver;

const DEFAULT_BASE: &str = "https://go.dev";
const USER_AGENT: &str = "containers-db-scanner/0.1";

pub struct GoAdapter {
    client: reqwest::Client,
    base: String,
    /// Fixed "now" for the trailing-90-day window, or `None` to sample the
    /// wall clock. Only `with_fixed_now` sets it (see `RustAdapter`).
    now_override: Option<DateTime<Utc>>,
}

impl Default for GoAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl GoAdapter {
    pub fn new() -> Self {
        Self::with_base(DEFAULT_BASE)
    }

    /// Construct an adapter pointed at a custom go.dev base — used by
    /// `wiremock`-based integration tests.
    pub fn with_base(base: impl Into<String>) -> Self {
        Self::build(base, None)
    }

    /// Like `with_base`, but pins "now" so tests can place a release at
    /// exactly `now - 90 days`.
    pub fn with_fixed_now(base: impl Into<String>, now: DateTime<Utc>) -> Self {
        Self::build(base, Some(now))
    }

    fn build(base: impl Into<String>, now_override: Option<DateTime<Utc>>) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .build()
            .expect("reqwest client build");
        Self {
            client,
            base: base.into(),
            now_override,
        }
    }
}

/// `go1.27.3` ⇒ `1.27.3`. Anything without the prefix is returned as-is.
pub fn strip_go_prefix(version: &str) -> &str {
    version.strip_prefix("go").unwrap_or(version)
}

/// Map each `id="go<ver>"` anchor on the release-history page to the first
/// `(released YYYY-MM-DD)` that follows it before the next anchor. Anchors
/// with no date in their section (e.g. `go1.27.minor`) are omitted. Keys
/// are bare versions (`1.27.3`).
pub fn parse_release_dates(html: &str) -> HashMap<String, NaiveDate> {
    const ANCHOR: &str = "id=\"go";
    const RELEASED: &str = "(released ";

    let mut dates = HashMap::new();
    let mut rest = html;
    while let Some(start) = rest.find(ANCHOR) {
        rest = &rest[start + ANCHOR.len()..];
        let Some(end) = rest.find('"') else { break };
        let version = &rest[..end];
        let section_end = rest.find(ANCHOR).unwrap_or(rest.len());
        let section = &rest[end..section_end];
        if let Some(date) = section
            .find(RELEASED)
            .and_then(|i| section[i + RELEASED.len()..].get(..10))
            .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
        {
            dates.insert(version.to_string(), date);
        }
    }
    dates
}

#[derive(Debug, Deserialize)]
struct GoRelease {
    version: String,
    #[serde(default)]
    stable: bool,
}

#[async_trait]
impl Adapter for GoAdapter {
    async fn fetch(&self, _index: &ToolIndex) -> anyhow::Result<AdapterResult> {
        let releases: Vec<GoRelease> =
            serde_json::from_str(&self.get("/dl/?mode=json&include=all").await?)
                .map_err(|e| anyhow::anyhow!("go.dev download feed JSON decode failed: {e}"))?;
        let dates = parse_release_dates(&self.get("/doc/devel/release").await?);

        // Releases are dated to the day, so the window is too: comparing a
        // midnight `released_at` against a wall-clock instant would drop the
        // day-90 release whenever a scan runs after 00:00 UTC.
        let now = self.now_override.unwrap_or_else(Utc::now);
        let ninety_days_ago = (now - Duration::days(90)).date_naive();

        let upstream_versions: Vec<UpstreamVersion> = releases
            .iter()
            .filter(|r| r.stable)
            .map(|r| strip_go_prefix(&r.version))
            .filter(|v| is_strict_semver(v))
            .filter_map(|v| {
                let date = dates.get(v)?;
                Some(UpstreamVersion {
                    version: v.to_string(),
                    released_at: date.and_hms_opt(0, 0, 0)?.and_utc(),
                    channel: "stable".to_string(),
                })
            })
            .collect();

        let releases_last_90d = u32::try_from(
            upstream_versions
                .iter()
                .filter(|v| v.released_at.date_naive() >= ninety_days_ago)
                .count(),
        )
        .unwrap_or(u32::MAX);
        let last_release_at = upstream_versions.iter().map(|v| v.released_at).max();

        let signals = ActivitySignals {
            releases_last_90d: Some(releases_last_90d),
            last_release_at,
            ..ActivitySignals::default()
        };

        Ok(AdapterResult {
            upstream_versions,
            signals,
            source_archived: false,
        })
    }
}

impl GoAdapter {
    async fn get(&self, path: &str) -> anyhow::Result<String> {
        let url = format!("{}{}", self.base, path);
        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("GET {url} failed: {e}"))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("GET {url} returned {status}: {body}");
        }
        response
            .text()
            .await
            .map_err(|e| anyhow::anyhow!("GET {url} body read failed: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn strip_go_prefix_handles_optional_prefix() {
        assert_eq!(strip_go_prefix("go1.27.3"), "1.27.3");
        assert_eq!(strip_go_prefix("1.27.3"), "1.27.3");
    }

    #[test]
    fn parses_major_and_minor_release_dates() {
        let html = r#"
            <h2 id="go1.27.0">go1.27.0 (released 2026-08-19)</h2>
            <p>Go 1.27.0 is a major release of Go.</p>
            <h3 id="go1.27.minor">Minor revisions</h3>
            <p id="go1.27.1">
            go1.27.1
            (released 2026-09-01)
            includes security fixes</p>
        "#;
        let dates = parse_release_dates(html);
        assert_eq!(dates.get("1.27.0"), Some(&date(2026, 8, 19)));
        assert_eq!(dates.get("1.27.1"), Some(&date(2026, 9, 1)));
        // The section header carries no date of its own and must not borrow
        // the next anchor's.
        assert!(!dates.contains_key("1.27.minor"));
        assert_eq!(dates.len(), 2);
    }

    #[test]
    fn anchor_without_date_does_not_borrow_next_sections_date() {
        let html = r#"<p id="go1.28.0">go1.28.0 coming soon</p>
                      <p id="go1.27.4">go1.27.4 (released 2026-11-04)</p>"#;
        let dates = parse_release_dates(html);
        assert!(!dates.contains_key("1.28.0"));
        assert_eq!(dates.get("1.27.4"), Some(&date(2026, 11, 4)));
    }

    #[test]
    fn malformed_date_is_omitted() {
        let html = r#"<p id="go1.27.5">go1.27.5 (released soon)</p>"#;
        assert!(parse_release_dates(html).is_empty());
    }
}
