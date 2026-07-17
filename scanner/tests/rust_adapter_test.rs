//! End-to-end test of `RustAdapter` against a stubbed GitHub REST API.
//! No live network — `wiremock` serves recorded fixtures so the test is
//! hermetic and fast.

use chrono::{Duration, SecondsFormat, Timelike, Utc};
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

/// Exact-second inclusivity guard for the trailing-90-day cutoff (issue #56).
///
/// `counts_releases_at_90_day_window_boundary` above straddles the edge with a
/// ±5-minute margin, so it proves the cutoff lands *near* 90 days but leaves the
/// precise comparison operator inferred — a release published at exactly
/// `now - 90 days` to the second is never asserted, so an off-by-one that made
/// the window exclusive (`>` instead of `>=`) at the boundary would slip
/// through. That margin exists only because the adapter sampled `Utc::now()`
/// itself, a hair after the fixtures were built, making an exactly-90-day
/// timestamp hostage to sub-second drift.
///
/// This test removes that ambiguity by pinning the adapter's clock via
/// `with_fixed_now`, then publishing a release at *precisely* `now - 90 days`.
/// With `now` fixed there is no drift, so `published_at == now - 90 days` lands
/// exactly on the cutoff and the release counts iff the operator is inclusive
/// (`>=`). A `>` regression would drop it to `Some(0)` and fail here.
#[tokio::test]
async fn counts_release_published_exactly_at_90_day_cutoff() {
    let server = MockServer::start().await;

    // Pin "now" so the boundary can be hit to the second. Truncate to whole
    // seconds: the fixture timestamp round-trips through an RFC3339 string at
    // seconds precision, so the pinned clock must have no sub-second component
    // either, or `now - 90 days` would carry nanos the fixture lost and the
    // release would land a fraction *before* the cutoff. With both at whole
    // seconds, `published_at == now - 90 days` holds bit-for-bit.
    let now = Utc::now()
        .with_nanosecond(0)
        .expect("nanosecond 0 is always valid");
    let exactly_90d_ago = (now - Duration::days(90)).to_rfc3339_opts(SecondsFormat::Secs, true);

    Mock::given(method("GET"))
        .and(path("/repos/rust-lang/rust"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "archived": false,
            "disabled": false,
            "pushed_at": days_ago(5)
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/rust-lang/rust/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "tag_name": "1.95.0", "published_at": exactly_90d_ago }
        ])))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/rust-lang/rust/security-advisories"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;

    let adapter = RustAdapter::with_fixed_now(server.uri(), now);
    let result = adapter.fetch(&rust_index(&server.uri())).await.unwrap();

    assert_eq!(
        result.signals.releases_last_90d,
        Some(1),
        "release published exactly at now-90d must count — the cutoff is inclusive (>=)"
    );
}

/// Exact-second exclusion guard for the trailing-90-day cutoff (issue #56).
///
/// The symmetric partner to `counts_release_published_exactly_at_90_day_cutoff`
/// above: that test pins the *inclusive* flank (a release exactly at the cutoff
/// counts); this one pins the *exclusive* flank to the same second precision. A
/// release published one second older than the cutoff (`now - 90 days - 1s`)
/// must NOT count. The pre-existing `counts_releases_at_90_day_window_boundary`
/// exercises exclusion only with a ±5-minute margin against the sampled wall
/// clock, so a cutoff drifted by a few minutes (e.g. `>= ninety_days_ago -
/// Duration::seconds(200)`) would still pass that straddle — but fails here,
/// because the fixed clock lets the fixture sit one second past the edge with no
/// drift slack to absorb the error.
#[tokio::test]
async fn excludes_release_published_one_second_before_90_day_cutoff() {
    let server = MockServer::start().await;

    // Same whole-second-truncated pinned clock as the inclusive test, so the
    // fixture below lands exactly one second older than the cutoff.
    let now = Utc::now()
        .with_nanosecond(0)
        .expect("nanosecond 0 is always valid");
    let one_second_before_cutoff = (now - Duration::days(90) - Duration::seconds(1))
        .to_rfc3339_opts(SecondsFormat::Secs, true);

    Mock::given(method("GET"))
        .and(path("/repos/rust-lang/rust"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "archived": false,
            "disabled": false,
            "pushed_at": days_ago(5)
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/rust-lang/rust/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "tag_name": "1.95.0", "published_at": one_second_before_cutoff }
        ])))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/rust-lang/rust/security-advisories"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;

    let adapter = RustAdapter::with_fixed_now(server.uri(), now);
    let result = adapter.fetch(&rust_index(&server.uri())).await.unwrap();

    // Passes the semver filter, so it lands in upstream_versions — the only
    // thing keeping it out of the count is the window cutoff.
    assert_eq!(result.upstream_versions.len(), 1);
    assert_eq!(
        result.signals.releases_last_90d,
        Some(0),
        "release published one second before now-90d must not count — the cutoff excludes older releases"
    );
}

/// Zero-in-window guard for `releases_last_90d` (issue #56).
///
/// The signal's value space is 0, 1, and N; the tests above cover 1 (boundary)
/// and 2/N (the end-to-end test). Nothing pinned the empty case: when releases
/// exist but *none* fall inside the window, is the signal `Some(0)` or `None`?
/// `archived_repo_sets_source_archived_flag` serves an empty releases list but
/// never asserts this. The adapter maps the count through `Some(...)`
/// unconditionally, so the contract is `Some(0)` — a real "we looked, found
/// zero recent releases" signal, distinct from `None` ("not computed"). This
/// test locks that in: a regression that collapsed an empty window to `None`
/// (e.g. via a `NonZeroU32` or an `is_empty` short-circuit) would fail here.
#[tokio::test]
async fn zero_releases_in_window_is_some_zero_not_none() {
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

    // A stable semver release exists, so this is not the empty-list case — it
    // is well outside the 90-day window (~200 days back), so the window count
    // is genuinely zero while `upstream_versions` is non-empty.
    Mock::given(method("GET"))
        .and(path("/repos/rust-lang/rust/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "tag_name": "1.90.0", "published_at": days_ago(200) }
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

    // The release passed the semver filter but not the window filter.
    assert_eq!(result.upstream_versions.len(), 1);
    assert_eq!(
        result.signals.releases_last_90d,
        Some(0),
        "an empty window is Some(0) (looked, found none), not None (not computed)"
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
