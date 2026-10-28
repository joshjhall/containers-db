//! `node` adapter — reads nodejs.org's release index.
//!
//! One call per scan: `GET {base}/dist/index.json`, a JSON array of every
//! release with its `version` (`v26.12.1`), `date` (`YYYY-MM-DD`), `lts`
//! (`false`, or the line's codename once it is LTS), and `files` (the
//! artifacts published for it). Its dates match the catalog's `released`
//! field exactly, so they serve as the scanner's high-water mark as-is.
//!
//! **Artifact gate.** A release is emitted only when `files` lists every
//! artifact the catalog's install methods download: `linux-x64` and
//! `linux-arm64` (the glibc tarballs) and `src` (the alpine source build).
//! A release missing any of them is held back whole rather than emitted
//! with a partial matrix, and its absence is logged — the same "read the
//! downloadable artifact, not the announcement" rule check-versions follows.
//! Every release from 18.0.0 on has shipped all three.
//!
//! **Default version.** `default_version` is the newest emitted release on
//! the highest major whose `lts` names a codename: the newest LTS line, not
//! the newest release (Current lines are short-lived). It reaches the index
//! only alongside a scan that adds versions — a scan that finds nothing new
//! writes nothing — which is how a line's LTS promotion arrives anyway: as
//! a new release whose `lts` changed.
//!
//! Only `releases_last_90d` and `last_release_at` are computed; there is no
//! repo or advisory call, so `last_commit_at` and `open_advisories` stay
//! unset.

use async_trait::async_trait;
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::Deserialize;

use crate::activity::ActivitySignals;
use crate::adapter::{Adapter, AdapterResult, ToolIndex, UpstreamVersion};
use crate::adapters::rust::is_strict_semver;
use crate::adapters::{
    DEFAULT_REQUEST_TIMEOUT, TlsRoots, http_client, request_error, status_error,
};
use crate::generate::version_key;

const DEFAULT_BASE: &str = "https://nodejs.org";

/// The `files` entries every emitted release must list: the amd64 and
/// arm64 glibc tarballs and the source tarball alpine builds from.
pub const REQUIRED_ARTIFACTS: [&str; 3] = ["linux-x64", "linux-arm64", "src"];

pub struct NodeAdapter {
    client: reqwest::Client,
    base: String,
    /// Fixed "now" for the trailing-90-day window, or `None` to sample the
    /// wall clock. Only `with_fixed_now` sets it (see `RustAdapter`).
    now_override: Option<DateTime<Utc>>,
    /// The request timeout `client` was built with, for the constructor test
    /// (see `RustAdapter`).
    #[cfg(test)]
    pub(crate) timeout: std::time::Duration,
    /// The roots `client` trusts, likewise kept for the constructor tests.
    #[cfg(test)]
    pub(crate) roots: TlsRoots,
}

impl NodeAdapter {
    pub fn new() -> anyhow::Result<Self> {
        Self::build(
            DEFAULT_BASE,
            None,
            DEFAULT_REQUEST_TIMEOUT,
            TlsRoots::System,
        )
    }

    /// Construct an adapter pointed at a custom nodejs.org base — used by
    /// `wiremock`-based integration tests. Trusts no TLS roots, so it needs
    /// no CA bundle and cannot reach an HTTPS base (`TlsRoots::None`); this
    /// and the other test constructors below share that.
    pub fn with_base(base: impl Into<String>) -> anyhow::Result<Self> {
        Self::build(base, None, DEFAULT_REQUEST_TIMEOUT, TlsRoots::None)
    }

    /// Like `with_base`, but pins "now" so tests can place a release at
    /// exactly `now - 90 days`.
    pub fn with_fixed_now(base: impl Into<String>, now: DateTime<Utc>) -> anyhow::Result<Self> {
        Self::build(base, Some(now), DEFAULT_REQUEST_TIMEOUT, TlsRoots::None)
    }

    /// Like `with_base`, but with a custom request timeout. Test-only (see
    /// `RustAdapter::with_timeout`).
    pub fn with_timeout(
        base: impl Into<String>,
        timeout: std::time::Duration,
    ) -> anyhow::Result<Self> {
        Self::build(base, None, timeout, TlsRoots::None)
    }

    fn build(
        base: impl Into<String>,
        now_override: Option<DateTime<Utc>>,
        timeout: std::time::Duration,
        roots: TlsRoots,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            client: http_client(timeout, roots)?,
            base: base.into(),
            now_override,
            #[cfg(test)]
            timeout,
            #[cfg(test)]
            roots,
        })
    }
}

/// One entry of `dist/index.json`. Fields the adapter doesn't read (`npm`,
/// `v8`, `security`, …) are ignored.
#[derive(Debug, Deserialize)]
struct NodeRelease {
    version: String,
    date: NaiveDate,
    #[serde(default)]
    files: Vec<String>,
    /// `false` for a non-LTS release, the line's codename once it is LTS.
    /// Kept loose so an unexpected shape reads as "not LTS" rather than
    /// failing the whole scan.
    #[serde(default)]
    lts: serde_json::Value,
}

impl NodeRelease {
    fn is_lts(&self) -> bool {
        self.lts
            .as_str()
            .is_some_and(|codename| !codename.is_empty())
    }
}

/// `v26.12.1` ⇒ `26.12.1`. Anything without the prefix is returned as-is.
pub fn strip_v_prefix(version: &str) -> &str {
    version.strip_prefix('v').unwrap_or(version)
}

/// The required artifacts `files` doesn't list, in [`REQUIRED_ARTIFACTS`]
/// order. Empty means the release can be emitted.
pub fn missing_artifacts(files: &[String]) -> Vec<&'static str> {
    REQUIRED_ARTIFACTS
        .into_iter()
        .filter(|a| !files.iter().any(|f| f == a))
        .collect()
}

#[async_trait]
impl Adapter for NodeAdapter {
    async fn fetch(&self, index: &ToolIndex) -> anyhow::Result<AdapterResult> {
        let releases: Vec<NodeRelease> = serde_json::from_str(&self.get("/dist/index.json").await?)
            .map_err(|e| anyhow::anyhow!("nodejs.org release index JSON decode failed: {e}"))?;

        // Releases are dated to the day, so the window is too (see `GoAdapter`).
        let now = self.now_override.unwrap_or_else(Utc::now);
        let ninety_days_ago = (now - Duration::days(90)).date_naive();

        let mut upstream_versions = Vec::new();
        let mut lts_versions: Vec<&str> = Vec::new();
        let mut held: Vec<(&str, Vec<&'static str>)> = Vec::new();
        for r in &releases {
            let version = strip_v_prefix(&r.version);
            if !is_strict_semver(version) {
                continue;
            }
            let missing = missing_artifacts(&r.files);
            if !missing.is_empty() {
                held.push((version, missing));
                continue;
            }
            if r.is_lts() {
                lts_versions.push(version);
            }
            upstream_versions.push(UpstreamVersion {
                version: version.to_string(),
                released_at: r
                    .date
                    .and_hms_opt(0, 0, 0)
                    .expect("midnight is a valid time")
                    .and_utc(),
                channel: "stable".to_string(),
            });
        }
        if let Some(line) = held_back_report(&held, &index.available_versions()) {
            eprintln!("scanner: node: holding back releases missing a required artifact: {line}");
        }

        let releases_last_90d = u32::try_from(
            upstream_versions
                .iter()
                .filter(|v| v.released_at.date_naive() >= ninety_days_ago)
                .count(),
        )
        .unwrap_or(u32::MAX);
        let last_release_at = upstream_versions.iter().map(|v| v.released_at).max();

        Ok(AdapterResult {
            upstream_versions,
            signals: ActivitySignals {
                releases_last_90d: Some(releases_last_90d),
                last_release_at,
                ..ActivitySignals::default()
            },
            source_archived: false,
            default_version: newest_lts(&lts_versions),
        })
    }
}

/// The held-back releases worth a log line: those at or above the catalog's
/// oldest version. Older ones (node 0.x lacks an arm64 tarball, for one)
/// could only arrive by an explicit backfill, so listing them on every scan
/// would bury the one entry that matters. `None` when nothing is left.
fn held_back_report(held: &[(&str, Vec<&str>)], cataloged: &[String]) -> Option<String> {
    let floor = cataloged
        .iter()
        .map(|v| version_key(Some(v)))
        .min()
        .unwrap_or_default();
    let lines: Vec<String> = held
        .iter()
        .filter(|(v, _)| version_key(Some(v)) >= floor)
        .map(|(v, missing)| format!("{v} (no {})", missing.join(", ")))
        .collect();
    (!lines.is_empty()).then(|| lines.join("; "))
}

/// The newest version on the highest major among `lts_versions`. Comparing
/// whole version keys gives both at once: the highest major wins, and
/// within it the newest patch.
fn newest_lts(lts_versions: &[&str]) -> Option<String> {
    lts_versions
        .iter()
        .max_by_key(|v| version_key(Some(v)))
        .map(|v| v.to_string())
}

impl NodeAdapter {
    async fn get(&self, path: &str) -> anyhow::Result<String> {
        let url = format!("{}{}", self.base, path);
        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| request_error(&url, "failed", e))?;
        if !response.status().is_success() {
            return Err(status_error(&url, response).await);
        }
        response
            .text()
            .await
            .map_err(|e| request_error(&url, "body read failed", e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every timeout test goes through `with_timeout`, so without this a
    /// constructor could drop `DEFAULT_REQUEST_TIMEOUT` — and bring
    /// back the #467 hang — with every test green. `new()` is checked in
    /// `crate::test_tls::roots::system_roots_client_builds`, which has roots
    /// to give it.
    #[test]
    fn test_constructors_use_the_default_timeout_and_no_roots() {
        let now = Utc::now();
        for (name, adapter) in [
            ("with_base", NodeAdapter::with_base("http://x").unwrap()),
            (
                "with_fixed_now",
                NodeAdapter::with_fixed_now("http://x", now).unwrap(),
            ),
        ] {
            assert_eq!(adapter.timeout, DEFAULT_REQUEST_TIMEOUT, "{name}");
            assert_eq!(adapter.roots, TlsRoots::None, "{name}");
        }
    }

    #[test]
    fn strip_v_prefix_handles_optional_prefix() {
        assert_eq!(strip_v_prefix("v26.12.1"), "26.12.1");
        assert_eq!(strip_v_prefix("26.12.1"), "26.12.1");
    }

    fn files(fs: &[&str]) -> Vec<String> {
        fs.iter().map(|f| f.to_string()).collect()
    }

    #[test]
    fn missing_artifacts_names_each_absent_one_in_order() {
        assert!(
            missing_artifacts(&files(&["src", "linux-arm64", "linux-x64", "win-x64-zip"]))
                .is_empty()
        );
        assert_eq!(
            missing_artifacts(&files(&["linux-x64", "src"])),
            vec!["linux-arm64"]
        );
        assert_eq!(
            missing_artifacts(&files(&["linux-x64-musl", "linux-arm64"])),
            vec!["linux-x64", "src"],
            "linux-x64-musl must not satisfy linux-x64"
        );
        assert_eq!(missing_artifacts(&[]), REQUIRED_ARTIFACTS.to_vec());
    }

    #[test]
    fn held_back_report_omits_releases_below_the_catalog_floor() {
        let cataloged = vec!["18.0.0".to_string(), "26.12.1".to_string()];
        let held = [
            ("0.12.18", vec!["linux-arm64"]),
            ("17.9.1", vec!["src"]),
            ("18.0.0", vec!["src"]),
            ("26.12.2", vec!["linux-x64", "linux-arm64"]),
        ];
        assert_eq!(
            held_back_report(&held, &cataloged).as_deref(),
            Some("18.0.0 (no src); 26.12.2 (no linux-x64, linux-arm64)")
        );
        assert_eq!(held_back_report(&held[..2], &cataloged), None);
        assert_eq!(held_back_report(&[], &cataloged), None);
        // An empty catalog has no floor: everything held back is reported.
        assert_eq!(
            held_back_report(&held[..1], &[]).as_deref(),
            Some("0.12.18 (no linux-arm64)")
        );
    }

    #[test]
    fn newest_lts_prefers_the_highest_major_then_the_newest_patch() {
        assert_eq!(
            newest_lts(&["22.23.4", "24.9.0", "24.22.1", "20.20.2"]).as_deref(),
            Some("24.22.1")
        );
        assert_eq!(newest_lts(&[]), None);
    }
}
