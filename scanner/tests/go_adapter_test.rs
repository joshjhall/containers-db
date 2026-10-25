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
    go_index_with(&["1.27.2"])
}

fn go_index_with(available: &[&str]) -> ToolIndex {
    let available: Vec<_> = available.iter().map(|v| json!({ "version": v })).collect();
    ToolIndex {
        id: "go".to_string(),
        source_repo: Some("https://github.com/golang/go".to_string()),
        raw: json!({
            "id": "go",
            "source_repo": "https://github.com/golang/go",
            "available": available
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
async fn undated_same_day_sibling_holds_back_every_new_release() {
    // Go ships both supported branches on one day. If the history page has
    // dated 1.27.4 but not yet 1.26.11, emitting 1.27.4 alone would make its
    // date the high-water mark, and the scanner's strictly-newer cutoff would
    // reject 1.26.11 forever. Neither may be emitted until both are dated.
    let server = serve(
        json!([
            { "version": "go1.27.4", "stable": true },
            { "version": "go1.26.11", "stable": true },
            { "version": "go1.27.3", "stable": true }
        ]),
        history(&[("1.27.4", days_ago(1)), ("1.27.3", days_ago(6))]),
    )
    .await;
    let index = go_index_with(&["1.26.10", "1.27.3"]);

    let result = GoAdapter::with_fixed_now(server.uri(), now())
        .fetch(&index)
        .await
        .unwrap();

    assert_eq!(versions(&result), vec!["1.27.3"]);
    // The dated release is real activity even while it is held back.
    assert_eq!(result.signals.releases_last_90d, Some(2));
    assert_eq!(
        result.signals.last_release_at.map(|t| t.date_naive()),
        Some(days_ago(1))
    );
}

#[tokio::test]
async fn held_back_releases_land_together_once_both_are_dated() {
    let server = serve(
        json!([
            { "version": "go1.27.4", "stable": true },
            { "version": "go1.26.11", "stable": true },
            { "version": "go1.27.3", "stable": true }
        ]),
        history(&[
            ("1.27.4", days_ago(1)),
            ("1.26.11", days_ago(1)),
            ("1.27.3", days_ago(6)),
        ]),
    )
    .await;
    let index = go_index_with(&["1.26.10", "1.27.3"]);

    let result = GoAdapter::with_fixed_now(server.uri(), now())
        .fetch(&index)
        .await
        .unwrap();

    assert_eq!(versions(&result), vec!["1.26.11", "1.27.3", "1.27.4"]);
}

#[tokio::test]
async fn permanently_undated_historical_gap_does_not_hold_back() {
    // 1.26.4 is older than the newest cataloged 1.26.x, so it is a gap in
    // history the cutoff would never admit, not a sibling still arriving.
    // If the history page never dates it, it must not stall new releases
    // for good.
    let server = serve(
        json!([
            { "version": "go1.27.4", "stable": true },
            { "version": "go1.26.4", "stable": true },
            { "version": "go1.27.3", "stable": true }
        ]),
        history(&[("1.27.4", days_ago(1)), ("1.27.3", days_ago(6))]),
    )
    .await;
    let index = go_index_with(&["1.26.10", "1.27.3"]);

    let result = GoAdapter::with_fixed_now(server.uri(), now())
        .fetch(&index)
        .await
        .unwrap();

    assert_eq!(versions(&result), vec!["1.27.3", "1.27.4"]);
}

#[tokio::test]
async fn undated_new_minor_holds_back_its_patch_sibling() {
    // A new minor ships alongside a patch of the previous one. No 1.28.x is
    // cataloged yet, so an undated 1.28.0 is a sibling still arriving.
    let server = serve(
        json!([
            { "version": "go1.28.0", "stable": true },
            { "version": "go1.27.4", "stable": true },
            { "version": "go1.27.3", "stable": true }
        ]),
        history(&[("1.27.4", days_ago(1)), ("1.27.3", days_ago(6))]),
    )
    .await;
    let index = go_index_with(&["1.27.3"]);

    let result = GoAdapter::with_fixed_now(server.uri(), now())
        .fetch(&index)
        .await
        .unwrap();

    assert_eq!(versions(&result), vec!["1.27.3"]);
}

#[tokio::test]
async fn malformed_cataloged_version_is_ignored_not_a_panic() {
    // A hand-edited or legacy catalog entry that is not three-part semver
    // must not crash the branch map; it is skipped, and the real entries
    // still decide what is pending.
    let server = serve(
        json!([
            { "version": "go1.27.4", "stable": true },
            { "version": "go1.26.4", "stable": true },
            { "version": "go1.27.3", "stable": true }
        ]),
        history(&[("1.27.4", days_ago(1)), ("1.27.3", days_ago(6))]),
    )
    .await;
    let index = go_index_with(&["1", "1.27", "garbage", "1.26.10", "1.27.3"]);

    let result = GoAdapter::with_fixed_now(server.uri(), now())
        .fetch(&index)
        .await
        .unwrap();

    assert_eq!(versions(&result), vec!["1.27.3", "1.27.4"]);
}

#[tokio::test]
async fn undated_release_with_empty_catalog_holds_back() {
    // With nothing cataloged, no branch has a newest patch, so any undated
    // release counts as pending.
    let server = serve(
        json!([
            { "version": "go1.27.4", "stable": true },
            { "version": "go1.21.13", "stable": true }
        ]),
        history(&[("1.27.4", days_ago(1))]),
    )
    .await;

    let result = GoAdapter::with_fixed_now(server.uri(), now())
        .fetch(&go_index_with(&[]))
        .await
        .unwrap();

    assert_eq!(versions(&result), Vec::<&str>::new());
}

#[tokio::test]
async fn feed_entry_without_stable_flag_is_excluded() {
    let server = serve(
        json!([
            { "version": "go1.27.4" },
            { "version": "go1.27.3", "stable": true }
        ]),
        history(&[("1.27.4", days_ago(1)), ("1.27.3", days_ago(6))]),
    )
    .await;

    let result = GoAdapter::with_fixed_now(server.uri(), now())
        .fetch(&go_index())
        .await
        .unwrap();

    assert_eq!(versions(&result), vec!["1.27.3"]);
}

#[tokio::test]
async fn parses_an_excerpt_of_the_real_history_page() {
    // Trimmed from go.dev/doc/devel/release: an `<h2>` major release, a
    // `.minor` section header, a multi-line `<p>` minor release with links,
    // and an unrelated `go-survey` anchor that must not disturb parsing.
    let html = r#"
	<p id="go-survey-2026"><a href="/s/survey">Take the Go survey</a></p>
	<h2 id="go1.27.0">go1.27.0 (released 2026-08-19)</h2>
	<p>
	Go 1.27.0 is a major release of Go.
	Read the <a href="/doc/go1.27">Go 1.27 Release Notes</a> for more information.
	</p>
	<h3 id="go1.27.minor">Minor revisions</h3>
		<p id="go1.27.3">
		go1.27.3
		(released 2026-10-19)
			includes
				security
			fixes to the <code>net/http</code> package.
		See the <a href="https://github.com/golang/go/issues?q=milestone%3AGo1.27.3+label%3ACherryPickApproved">Go
		1.27.3 milestone</a> on our issue tracker for details.
		</p>
    "#;
    let server = serve(
        json!([
            { "version": "go1.27.3", "stable": true },
            { "version": "go1.27.0", "stable": true }
        ]),
        html.to_string(),
    )
    .await;

    let result = GoAdapter::with_fixed_now(server.uri(), now())
        .fetch(&go_index_with(&["1.27.0"]))
        .await
        .unwrap();

    let dated: Vec<(String, NaiveDate)> = {
        let mut d: Vec<_> = result
            .upstream_versions
            .iter()
            .map(|u| (u.version.clone(), u.released_at.date_naive()))
            .collect();
        d.sort();
        d
    };
    assert_eq!(
        dated,
        vec![
            (
                "1.27.0".to_string(),
                NaiveDate::from_ymd_opt(2026, 8, 19).unwrap()
            ),
            (
                "1.27.3".to_string(),
                NaiveDate::from_ymd_opt(2026, 10, 19).unwrap()
            ),
        ]
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
async fn ninety_day_window_is_by_date_whatever_the_scan_time() {
    // A scan late in the UTC day must count the same releases as one at
    // midnight: release dates carry no time, so a day-90 release dated
    // 00:00 must not fall before an afternoon cutoff instant.
    let server = serve(
        json!([
            { "version": "go1.27.1", "stable": true },
            { "version": "go1.27.0", "stable": true }
        ]),
        history(&[("1.27.1", days_ago(90)), ("1.27.0", days_ago(91))]),
    )
    .await;
    let late = now() + Duration::hours(23) + Duration::minutes(59);

    let result = GoAdapter::with_fixed_now(server.uri(), late)
        .fetch(&go_index())
        .await
        .unwrap();

    assert_eq!(result.signals.releases_last_90d, Some(1));
}

#[tokio::test]
async fn non_json_feed_is_a_decode_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/dl/"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>maintenance</html>"))
        .mount(&server)
        .await;

    let err = GoAdapter::with_fixed_now(server.uri(), now())
        .fetch(&go_index())
        .await
        .unwrap_err()
        .to_string();

    assert!(err.contains("download feed JSON decode failed"), "{err}");
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
