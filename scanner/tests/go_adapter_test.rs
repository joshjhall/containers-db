//! End-to-end test of `GoAdapter` against a stubbed go.dev. No live
//! network — `wiremock` serves the download feed and release-history page
//! so the test is hermetic and fast.

use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};
use scanner::adapter::{Adapter, ToolIndex};
use scanner::adapters::go::GoAdapter;
use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Fixed "now" for every test. The adapter only dates releases to the day,
/// so pinning now (rather than sampling the wall clock, as
/// `rust_adapter_test.rs` does for second-precision timestamps) is what lets
/// the 90-day edge be asserted exactly without a date bomb.
fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 25, 0, 0, 0).unwrap()
}

fn days_ago(n: i64) -> NaiveDate {
    (now() - Duration::days(n)).date_naive()
}

fn go_index() -> ToolIndex {
    ToolIndex {
        id: "go".to_string(),
        source_repo: Some("https://github.com/golang/go".to_string()),
        raw: json!({
            "id": "go",
            "source_repo": "https://github.com/golang/go",
            "available": [ { "version": "1.27.2" } ]
        }),
        path: std::path::PathBuf::from("/tmp/index.json"),
    }
}

/// Render a release-history page with one `<p id="go…">` section per entry.
fn history(entries: &[(&str, NaiveDate)]) -> String {
    entries
        .iter()
        .map(|(v, d)| format!("<p id=\"go{v}\">\n go{v}\n (released {d})\n</p>\n"))
        .collect()
}

async fn serve(feed: serde_json::Value, history_html: String) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/dl/"))
        .and(query_param("mode", "json"))
        .and(query_param("include", "all"))
        .respond_with(ResponseTemplate::new(200).set_body_json(feed))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/doc/devel/release"))
        .respond_with(ResponseTemplate::new(200).set_body_string(history_html))
        .mount(&server)
        .await;
    server
}

fn versions(result: &scanner::adapter::AdapterResult) -> Vec<&str> {
    let mut v: Vec<&str> = result
        .upstream_versions
        .iter()
        .map(|u| u.version.as_str())
        .collect();
    v.sort_unstable();
    v
}

#[tokio::test]
async fn returns_stable_versions_with_prefix_stripped_and_history_dates() {
    let server = serve(
        json!([
            { "version": "go1.28rc1", "stable": false, "files": [] },
            { "version": "go1.27.3", "stable": true, "files": [] },
            { "version": "go1.27.2", "stable": true, "files": [] }
        ]),
        history(&[("1.27.3", days_ago(6)), ("1.27.2", days_ago(17))]),
    )
    .await;

    let result = GoAdapter::with_fixed_now(server.uri(), now())
        .fetch(&go_index())
        .await
        .unwrap();

    assert_eq!(versions(&result), vec!["1.27.2", "1.27.3"]);
    let newest = result
        .upstream_versions
        .iter()
        .find(|u| u.version == "1.27.3")
        .unwrap();
    assert_eq!(newest.released_at.date_naive(), days_ago(6));
    assert_eq!(newest.channel, "stable");
    assert!(!result.source_archived);
}

#[tokio::test]
async fn excludes_prereleases_unstable_and_non_semver_entries() {
    // Pre-releases are dated on the history page here on purpose: the
    // filter must come from the feed's `stable` flag and the version shape,
    // not from a missing date.
    let server = serve(
        json!([
            { "version": "go1.28rc1", "stable": false },
            { "version": "go1.28beta1", "stable": false },
            // Mislabelled pre-release: stable in the feed, but not semver.
            { "version": "go1.28rc2", "stable": true },
            // Pre-1.21 two-part name.
            { "version": "go1.20", "stable": true },
            { "version": "go1.27.4", "stable": false },
            { "version": "go1.27.3", "stable": true }
        ]),
        history(&[
            ("1.28rc1", days_ago(2)),
            ("1.28beta1", days_ago(20)),
            ("1.28rc2", days_ago(1)),
            ("1.20", days_ago(900)),
            ("1.27.4", days_ago(1)),
            ("1.27.3", days_ago(6)),
        ]),
    )
    .await;

    let result = GoAdapter::with_fixed_now(server.uri(), now())
        .fetch(&go_index())
        .await
        .unwrap();

    assert_eq!(versions(&result), vec!["1.27.3"]);
}

#[tokio::test]
async fn stable_version_missing_from_history_page_is_skipped() {
    let server = serve(
        json!([
            { "version": "go1.27.4", "stable": true },
            { "version": "go1.27.3", "stable": true }
        ]),
        history(&[("1.27.3", days_ago(6))]),
    )
    .await;

    let result = GoAdapter::with_fixed_now(server.uri(), now())
        .fetch(&go_index())
        .await
        .unwrap();

    assert_eq!(versions(&result), vec!["1.27.3"]);
    // An undated release must not leak into the activity signals either.
    assert_eq!(result.signals.releases_last_90d, Some(1));
    assert_eq!(
        result.signals.last_release_at.map(|t| t.date_naive()),
        Some(days_ago(6))
    );
}

#[tokio::test]
async fn ninety_day_window_includes_edge_and_excludes_day_before() {
    let server = serve(
        json!([
            { "version": "go1.27.3", "stable": true },
            { "version": "go1.27.2", "stable": true },
            { "version": "go1.27.1", "stable": true },
            { "version": "go1.27.0", "stable": true }
        ]),
        history(&[
            ("1.27.3", days_ago(6)),
            ("1.27.2", days_ago(40)),
            ("1.27.1", days_ago(90)),
            ("1.27.0", days_ago(91)),
        ]),
    )
    .await;

    let result = GoAdapter::with_fixed_now(server.uri(), now())
        .fetch(&go_index())
        .await
        .unwrap();

    assert_eq!(versions(&result).len(), 4);
    // 6, 40, and exactly 90 days count; 91 does not.
    assert_eq!(result.signals.releases_last_90d, Some(3));
    assert_eq!(
        result.signals.last_release_at.map(|t| t.date_naive()),
        Some(days_ago(6))
    );
    assert_eq!(result.signals.last_commit_at, None);
    assert_eq!(result.signals.open_advisories, None);
}

#[tokio::test]
async fn feed_error_names_the_url() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/dl/"))
        .respond_with(ResponseTemplate::new(503).set_body_string("down"))
        .mount(&server)
        .await;

    let err = GoAdapter::with_fixed_now(server.uri(), now())
        .fetch(&go_index())
        .await
        .unwrap_err()
        .to_string();

    assert!(err.contains("/dl/?mode=json&include=all"), "{err}");
    assert!(err.contains("503"), "{err}");
}

#[tokio::test]
async fn history_page_error_names_the_url() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/dl/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!([{ "version": "go1.27.3", "stable": true }])),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/doc/devel/release"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let err = GoAdapter::with_fixed_now(server.uri(), now())
        .fetch(&go_index())
        .await
        .unwrap_err()
        .to_string();

    assert!(err.contains("/doc/devel/release"), "{err}");
    assert!(err.contains("404"), "{err}");
}
