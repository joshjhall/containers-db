//! `rust-lang/rust` adapter — queries the GitHub REST API for stable
//! releases and repo metadata.
//!
//! Three calls per scan:
//!   1. `GET /repos/{owner}/{repo}` — `archived`, `disabled`, `pushed_at`
//!   2. `GET /repos/{owner}/{repo}/releases?per_page=100` — versions +
//!      `last_release_at` + `releases_last_90d`
//!   3. `GET /repos/{owner}/{repo}/security-advisories?state=published&per_page=100`
//!      — `open_advisories`
//!
//! Auth via `GITHUB_TOKEN` is optional (unauthenticated requests work at
//! lower rate limits). `commits_last_90d` and `active_maintainers` are
//! deliberately not computed — they need extra paginated calls and
//! aren't load-bearing in the tier mapping.

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;

use crate::activity::ActivitySignals;
use crate::adapter::{Adapter, AdapterResult, ToolIndex, UpstreamVersion};

const DEFAULT_API_BASE: &str = "https://api.github.com";
const USER_AGENT: &str = "containers-db-scanner/0.1";

pub struct RustAdapter {
    client: reqwest::Client,
    api_base: String,
    /// Fixed "now" for the trailing-90-day window, or `None` to sample the
    /// wall clock. Production always leaves this `None` (see `new`); only
    /// `with_fixed_now` sets it, so tests can pin the window edge to the
    /// second instead of straddling it with a clock-drift margin.
    now_override: Option<DateTime<Utc>>,
}

impl Default for RustAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl RustAdapter {
    pub fn new() -> Self {
        Self::with_api_base(DEFAULT_API_BASE)
    }

    /// Construct an adapter pointed at a custom API base — used by
    /// `wiremock`-based integration tests.
    pub fn with_api_base(api_base: impl Into<String>) -> Self {
        Self::build(api_base, None)
    }

    /// Like `with_api_base`, but pins the adapter's notion of "now" to a fixed
    /// instant. Test-only: lets a test place a release at *exactly*
    /// `now - 90 days` and assert the window cutoff to the second, rather than
    /// inferring the `>=` comparison from a ±5-minute margin (issue #56).
    pub fn with_fixed_now(api_base: impl Into<String>, now: DateTime<Utc>) -> Self {
        Self::build(api_base, Some(now))
    }

    fn build(api_base: impl Into<String>, now_override: Option<DateTime<Utc>>) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .build()
            .expect("reqwest client build");
        Self {
            client,
            api_base: api_base.into(),
            now_override,
        }
    }
}

/// Extract `(owner, repo)` from a github.com URL. Supports
/// `https://github.com/<o>/<r>`, the `.git`-suffixed variant, and
/// `git@github.com:<o>/<r>`. Returns `None` for non-GitHub URLs.
pub fn parse_github_url(url: &str) -> Option<(&str, &str)> {
    let rest = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
        .or_else(|| url.strip_prefix("git@github.com:"))?;
    let rest = rest.trim_end_matches('/').trim_end_matches(".git");
    let (owner, repo) = rest.split_once('/')?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    // Trailing path segments (issues, blob/main/foo) are not allowed.
    if repo.contains('/') {
        return None;
    }
    Some((owner, repo))
}

/// `1.95.0` ⇒ true. `1.95.0-beta.1`, `v1.95.0`, `nightly` ⇒ false.
/// Conservative — the scanner only opens PRs for tag names that
/// look like pure semver, so anything ambiguous gets skipped.
pub fn is_strict_semver(tag: &str) -> bool {
    let parts: Vec<&str> = tag.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

#[derive(Debug, Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    published_at: Option<DateTime<Utc>>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
}

#[derive(Debug, Deserialize)]
struct GhRepo {
    #[serde(default)]
    archived: bool,
    #[serde(default)]
    disabled: bool,
    #[serde(default)]
    pushed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize)]
struct GhAdvisory {
    #[serde(default)]
    state: String,
}

#[async_trait]
impl Adapter for RustAdapter {
    async fn fetch(&self, index: &ToolIndex) -> anyhow::Result<AdapterResult> {
        let source = index
            .source_repo
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("tool `{}` has no source_repo", index.id))?;
        let (owner, repo) = parse_github_url(source).ok_or_else(|| {
            anyhow::anyhow!(
                "tool `{}` source_repo `{}` is not a GitHub URL the rust adapter understands",
                index.id,
                source,
            )
        })?;

        let token = std::env::var("GITHUB_TOKEN").ok();

        let repo_data: GhRepo = self
            .get(&format!("/repos/{owner}/{repo}"), token.as_deref())
            .await?;
        let releases: Vec<GhRelease> = self
            .get(
                &format!("/repos/{owner}/{repo}/releases?per_page=100"),
                token.as_deref(),
            )
            .await?;
        let advisories: Vec<GhAdvisory> = self
            .get(
                &format!("/repos/{owner}/{repo}/security-advisories?state=published&per_page=100"),
                token.as_deref(),
            )
            .await?;

        let now = self.now_override.unwrap_or_else(Utc::now);
        let ninety_days_ago = now - Duration::days(90);

        let stable: Vec<GhRelease> = releases
            .into_iter()
            .filter(|r| !r.draft && !r.prerelease)
            .filter(|r| is_strict_semver(strip_v(&r.tag_name)))
            .collect();

        let releases_last_90d = u32::try_from(
            stable
                .iter()
                .filter(|r| r.published_at.is_some_and(|p| p >= ninety_days_ago))
                .count(),
        )
        .unwrap_or(u32::MAX);

        let last_release_at = stable.iter().filter_map(|r| r.published_at).max();

        let upstream_versions: Vec<UpstreamVersion> = stable
            .into_iter()
            .filter_map(|r| {
                let released_at = r.published_at?;
                Some(UpstreamVersion {
                    version: strip_v(&r.tag_name).to_string(),
                    released_at,
                    channel: "stable".to_string(),
                })
            })
            .collect();

        let open_advisories =
            u32::try_from(advisories.iter().filter(|a| a.state == "published").count())
                .unwrap_or(u32::MAX);

        let signals = ActivitySignals {
            releases_last_90d: Some(releases_last_90d),
            last_release_at,
            last_commit_at: repo_data.pushed_at,
            open_advisories: Some(open_advisories),
            ..ActivitySignals::default()
        };

        Ok(AdapterResult {
            upstream_versions,
            signals,
            source_archived: repo_data.archived || repo_data.disabled,
        })
    }
}

impl RustAdapter {
    async fn get<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        token: Option<&str>,
    ) -> anyhow::Result<T> {
        let url = format!("{}{}", self.api_base, path);
        let mut req = self
            .client
            .get(&url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        let response = req
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("GET {url} failed: {e}"))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("GET {url} returned {status}: {body}");
        }
        response
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("GET {url} JSON decode failed: {e}"))
    }
}

fn strip_v(tag: &str) -> &str {
    tag.strip_prefix('v').unwrap_or(tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_https_github_url() {
        assert_eq!(
            parse_github_url("https://github.com/rust-lang/rust"),
            Some(("rust-lang", "rust"))
        );
    }

    #[test]
    fn parses_https_github_url_with_trailing_slash_and_git_suffix() {
        assert_eq!(
            parse_github_url("https://github.com/rust-lang/rust.git"),
            Some(("rust-lang", "rust"))
        );
        assert_eq!(
            parse_github_url("https://github.com/rust-lang/rust/"),
            Some(("rust-lang", "rust"))
        );
    }

    #[test]
    fn rejects_non_github_url() {
        assert!(parse_github_url("https://gitlab.com/foo/bar").is_none());
    }

    #[test]
    fn rejects_url_with_subpath() {
        // Don't silently accept arbitrary URLs that happen to start with
        // github.com — a stray issues link in the catalog would otherwise
        // produce nonsense API calls.
        assert!(parse_github_url("https://github.com/rust-lang/rust/issues").is_none());
    }

    #[test]
    fn strict_semver_accepts_plain_three_part() {
        assert!(is_strict_semver("1.95.0"));
        assert!(is_strict_semver("0.1.0"));
        assert!(is_strict_semver("10.20.30"));
    }

    #[test]
    fn strict_semver_rejects_pre_release_and_prefix() {
        assert!(!is_strict_semver("1.95.0-beta.1"));
        assert!(!is_strict_semver("1.95.0+build"));
        assert!(!is_strict_semver("nightly"));
        assert!(!is_strict_semver("1.95"));
        assert!(!is_strict_semver(""));
    }

    #[test]
    fn strip_v_handles_optional_prefix() {
        assert_eq!(strip_v("1.95.0"), "1.95.0");
        assert_eq!(strip_v("v1.95.0"), "1.95.0");
    }
}
