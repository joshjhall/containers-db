//! End-to-end test of `RustAdapter` against a stubbed GitHub REST API.
//! No live network — `wiremock` serves recorded fixtures so the test is
//! hermetic and fast.

use chrono::{Duration, SecondsFormat, Utc};
use scanner::adapter::{Adapter, ToolIndex};
use scanner::adapters::rust::RustAdapter;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// An RFC3339 timestamp `n` days before wall-clock now, matching the `...Z`
/// shape the adapter parses. Fixtures below must use this instead of absolute
/// literals: the adapter's `releases_last_90d` signal is computed against
/// `Utc::now()`, so hard-coded dates silently age out of the trailing-90-day
/// window and turn the test into a date bomb (issue #48). Keep the newest
/// stable release comfortably inside 90 days.
fn days_ago(n: i64) -> String {
    (Utc::now() - Duration::days(n)).to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn rust_index(server_uri: &str) -> ToolIndex {
    // server_uri is unused by ToolIndex itself — the adapter pulls
    // source_repo and the api_base override controls where requests go.
    let _ = server_uri;
    ToolIndex {
        id: "rust".to_string(),
        source_repo: Some("https://github.com/rust-lang/rust".to_string()),
        raw: json!({
            "id": "rust",
            "source_repo": "https://github.com/rust-lang/rust",
            "available": [
                { "version": "1.93.0" },
                { "version": "1.94.0" }
            ]
        }),
        path: std::path::PathBuf::from("/tmp/index.json"),
    }
}

#[tokio::test]
async fn returns_only_stable_semver_releases_with_signals_and_advisories() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/rust-lang/rust"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "archived": false,
            "disabled": false,
            "pushed_at": days_ago(5)
        })))
        .mount(&server)
        .await;

    // Dates are window-relative (see `days_ago`): the newest stable release sits
    // ~12 days back so it stays inside the adapter's trailing-90-day window
    // regardless of when CI runs. Do not reintroduce absolute date literals here.
    Mock::given(method("GET"))
        .and(path("/repos/rust-lang/rust/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "tag_name": "1.95.0", "published_at": days_ago(12) },
            { "tag_name": "1.94.1", "published_at": days_ago(55) },
            { "tag_name": "1.94.0", "published_at": days_ago(120) },
            { "tag_name": "1.95.0-beta.1", "published_at": days_ago(20), "prerelease": true },
            { "tag_name": "draft-internal", "published_at": null, "draft": true },
            { "tag_name": "nightly-2026-04-29", "published_at": days_ago(6) }
        ])))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/rust-lang/rust/security-advisories"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "state": "published" },
            { "state": "published" }
        ])))
        .mount(&server)
        .await;

    let adapter = RustAdapter::with_api_base(server.uri());
    let result = adapter.fetch(&rust_index(&server.uri())).await.unwrap();

    let versions: Vec<&str> = result
        .upstream_versions
        .iter()
        .map(|v| v.version.as_str())
        .collect();
    assert!(versions.contains(&"1.95.0"));
    assert!(versions.contains(&"1.94.1"));
    assert!(versions.contains(&"1.94.0"));
    assert!(
        !versions.contains(&"1.95.0-beta.1"),
        "prereleases must be filtered"
    );
    assert!(
        !versions.contains(&"draft-internal"),
        "drafts must be filtered"
    );
    assert!(
        !versions.contains(&"nightly-2026-04-29"),
        "non-semver tags must be filtered"
    );

    assert!(!result.source_archived);
    assert_eq!(result.signals.open_advisories, Some(2));
    assert!(result.signals.last_release_at.is_some());
    assert!(result.signals.last_commit_at.is_some());
    // Exactly 2 stable semver releases fall inside the trailing-90-day window:
    // 1.95.0 (~12d) and 1.94.1 (~55d). 1.94.0 (~120d) is stable semver but
    // outside the window, so a broken cutoff that failed to exclude it would
    // count 3 and fail here — which a `>= 1` check would have silently passed.
    assert_eq!(result.signals.releases_last_90d, Some(2));
}

/// An RFC3339 timestamp offset a small amount from the exact trailing-90-day
/// cutoff: `offset_minutes` minutes *inside* the boundary when positive (newer
/// than 90 days ago), *outside* when negative. Used to straddle the window edge
/// far more tightly than a whole-day margin while staying robust to clock skew.
fn near_90d_boundary(offset_minutes: i64) -> String {
    (Utc::now() - Duration::days(90) + Duration::minutes(offset_minutes))
        .to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Boundary guard for the trailing-90-day release window (issue #51).
///
/// The adapter counts a release when `published_at >= now - 90 days`
/// (`scanner/src/adapters/rust.rs`). The end-to-end test above only exercises
/// releases comfortably inside (12, 55 days) and comfortably outside (120
/// days) the window, so an off-by-one that shifted the cutoff to 89 or 91 days
/// instead of 90 would go uncaught.
///
/// This test straddles the exact 90-day edge with a ±5-minute margin: one
/// release sits 5 minutes *inside* the cutoff (must be counted) and one 5
/// minutes *outside* (must not). Pinning both flanks a hair from the exact
/// boundary catches a one-day off-by-one in *either* direction — a cutoff
/// widened to 91 days would wrongly pull the outside release in (count 2), and
/// one narrowed to 89 days would wrongly push the inside release out (count 0)
/// — whereas a 89/91-day straddle would only catch the widening direction.
///
/// The 5-minute margin (not exactly-90) is deliberate: the adapter samples
/// `Utc::now()` a moment after the fixtures are built, so an *exactly*-90-day
/// timestamp would land a hair outside the window and make the assertion
/// hostage to sub-second clock drift. Five minutes dwarfs that drift while
/// staying tight enough that any day-granularity boundary error still flips a
/// count.
#[tokio::test]
async fn counts_releases_at_90_day_window_boundary() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/rust-lang/rust"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "archived": false,
            "disabled": false,
            "pushed_at": days_ago(5)
        })))
        .mount(&server)
        .await;

    // 1.95.0 sits 5 minutes inside the 90-day cutoff (counted); 1.94.0 sits 5
    // minutes outside (not counted). Both are stable semver releases, so the
    // only thing separating them is the window cutoff.
    Mock::given(method("GET"))
        .and(path("/repos/rust-lang/rust/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "tag_name": "1.95.0", "published_at": near_90d_boundary(5) },
            { "tag_name": "1.94.0", "published_at": near_90d_boundary(-5) }
        ])))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/rust-lang/rust/security-advisories"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;

    let adapter = RustAdapter::with_api_base(server.uri());
    let result = adapter.fetch(&rust_index(&server.uri())).await.unwrap();

    // Both releases pass the draft/prerelease/semver filters, so both land in
    // upstream_versions — this isolates the assertion to the window cutoff
    // rather than the semver filter.
    assert_eq!(result.upstream_versions.len(), 2);
    assert_eq!(
        result.signals.releases_last_90d,
        Some(1),
        "release just inside 90d must count, just outside must not"
    );
}

#[tokio::test]
async fn archived_repo_sets_source_archived_flag() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/rust-lang/rust"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "archived": true,
            "disabled": false,
            "pushed_at": "2024-01-01T00:00:00Z"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/rust-lang/rust/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/rust-lang/rust/security-advisories"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;

    let adapter = RustAdapter::with_api_base(server.uri());
    let result = adapter.fetch(&rust_index(&server.uri())).await.unwrap();
    assert!(result.source_archived);
    assert!(result.upstream_versions.is_empty());
}

#[tokio::test]
async fn missing_source_repo_returns_error() {
    let server = MockServer::start().await;
    let adapter = RustAdapter::with_api_base(server.uri());
    let index = ToolIndex {
        id: "rust".to_string(),
        source_repo: None,
        raw: json!({ "id": "rust" }),
        path: std::path::PathBuf::from("/tmp/index.json"),
    };
    let err = adapter.fetch(&index).await.unwrap_err();
    assert!(err.to_string().contains("no source_repo"));
}

#[tokio::test]
async fn non_github_source_repo_returns_error() {
    let server = MockServer::start().await;
    let adapter = RustAdapter::with_api_base(server.uri());
    let index = ToolIndex {
        id: "rust".to_string(),
        source_repo: Some("https://gitlab.com/rust-lang/rust".to_string()),
        raw: json!({}),
        path: std::path::PathBuf::from("/tmp/index.json"),
    };
    let err = adapter.fetch(&index).await.unwrap_err();
    assert!(err.to_string().contains("not a GitHub URL"));
}
