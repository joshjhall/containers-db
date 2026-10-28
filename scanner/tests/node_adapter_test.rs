//! End-to-end test of `NodeAdapter` against a stubbed nodejs.org. No live
//! network — `wiremock` serves `dist/index.json` so the test is hermetic and
//! fast.

use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};
use scanner::adapter::{Adapter, AdapterResult, ToolIndex};
use scanner::adapters::node::NodeAdapter;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Fixed "now": releases are dated to the day, so pinning it is what lets
/// the 90-day edge be asserted exactly (see `go_adapter_test.rs`).
fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 25, 0, 0, 0).unwrap()
}

fn days_ago(n: i64) -> NaiveDate {
    (now() - Duration::days(n)).date_naive()
}

fn node_index() -> ToolIndex {
    ToolIndex {
        id: "node".to_string(),
        source_repo: Some("https://github.com/nodejs/node".to_string()),
        raw: json!({
            "id": "node",
            "source_repo": "https://github.com/nodejs/node",
            "available": [{ "version": "26.12.1" }]
        }),
        path: std::path::PathBuf::from("/tmp/index.json"),
    }
}

/// Every artifact a real release lists that the gate looks at, plus noise
/// that must not satisfy it.
const ALL_FILES: [&str; 6] = [
    "linux-arm64",
    "linux-x64",
    "linux-x64-musl",
    "osx-arm64-tar",
    "src",
    "win-x64-zip",
];

/// One `dist/index.json` entry with every required artifact.
fn release(version: &str, date: NaiveDate, lts: Value) -> Value {
    release_with(version, date, lts, &ALL_FILES)
}

fn release_with(version: &str, date: NaiveDate, lts: Value, files: &[&str]) -> Value {
    json!({
        "version": version,
        "date": date.to_string(),
        "files": files,
        "npm": "11.0.0",
        "v8": "14.0.0.0",
        "security": false,
        "lts": lts,
    })
}

async fn fetch(feed: Value) -> AdapterResult {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/dist/index.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(feed))
        .mount(&server)
        .await;
    NodeAdapter::with_fixed_now(server.uri(), now())
        .fetch(&node_index())
        .await
        .unwrap()
}

fn versions(result: &AdapterResult) -> Vec<&str> {
    let mut v: Vec<&str> = result
        .upstream_versions
        .iter()
        .map(|u| u.version.as_str())
        .collect();
    v.sort_unstable();
    v
}

#[tokio::test]
async fn returns_releases_with_prefix_stripped_and_index_dates() {
    let result = fetch(json!([
        release("v26.12.2", days_ago(3), json!(false)),
        release("v24.22.2", days_ago(3), json!("Krypton")),
    ]))
    .await;

    assert_eq!(versions(&result), vec!["24.22.2", "26.12.2"]);
    for u in &result.upstream_versions {
        assert_eq!(u.released_at.date_naive(), days_ago(3), "{}", u.version);
        assert_eq!(
            u.released_at.time(),
            chrono::NaiveTime::MIN,
            "{}",
            u.version
        );
        // LTS-ness drives `default_version` only; every release is stable.
        assert_eq!(u.channel, "stable", "{}", u.version);
    }
    assert!(!result.source_archived);
}

#[tokio::test]
async fn non_semver_versions_are_excluded() {
    let result = fetch(json!([
        release("v26.12.2", days_ago(3), json!(false)),
        release("v27.0.0-rc.1", days_ago(2), json!(false)),
        release("v26.12", days_ago(2), json!(false)),
        release("nightly", days_ago(1), json!(false)),
    ]))
    .await;

    assert_eq!(versions(&result), vec!["26.12.2"]);
}

/// Each required artifact gates the release on its own: dropping any one
/// of them holds the whole release back.
#[tokio::test]
async fn release_missing_any_required_artifact_is_held_back() {
    for missing in ["linux-x64", "linux-arm64", "src"] {
        let files: Vec<&str> = ALL_FILES.into_iter().filter(|f| *f != missing).collect();
        let result = fetch(json!([
            release("v26.12.1", days_ago(10), json!(false)),
            release_with("v26.12.2", days_ago(3), json!(false), &files),
        ]))
        .await;

        assert_eq!(versions(&result), vec!["26.12.1"], "missing {missing}");
    }
}

#[tokio::test]
async fn musl_tarball_does_not_stand_in_for_the_glibc_one() {
    let result = fetch(json!([release_with(
        "v26.12.2",
        days_ago(3),
        json!(false),
        &["linux-x64-musl", "linux-arm64", "src"],
    )]))
    .await;

    assert!(result.upstream_versions.is_empty());
}

/// A held-back release is not a release yet: it counts toward neither
/// activity signal nor `default_version`.
#[tokio::test]
async fn held_back_release_counts_toward_no_signal() {
    let result = fetch(json!([
        release("v24.22.1", days_ago(10), json!("Krypton")),
        release_with(
            "v24.22.2",
            days_ago(3),
            json!("Krypton"),
            &["linux-x64", "src"]
        ),
    ]))
    .await;

    assert_eq!(result.signals.releases_last_90d, Some(1));
    assert_eq!(
        result.signals.last_release_at.map(|t| t.date_naive()),
        Some(days_ago(10))
    );
    assert_eq!(result.default_version.as_deref(), Some("24.22.1"));
}

#[tokio::test]
async fn default_version_is_the_newest_release_on_the_newest_lts_line() {
    let result = fetch(json!([
        // Current lines are newer but never the default.
        release("v26.12.1", days_ago(4), json!(false)),
        release("v25.9.0", days_ago(200), json!(false)),
        release("v24.22.1", days_ago(5), json!("Krypton")),
        release("v24.9.0", days_ago(300), json!("Krypton")),
        release("v22.23.4", days_ago(5), json!("Jod")),
        // Before its LTS promotion a line's releases carry `lts: false`.
        release("v24.8.0", days_ago(320), json!(false)),
    ]))
    .await;

    assert_eq!(result.default_version.as_deref(), Some("24.22.1"));
}

#[tokio::test]
async fn no_lts_release_leaves_default_version_unset() {
    let result = fetch(json!([
        release("v26.12.1", days_ago(4), json!(false)),
        // An empty or non-string `lts` is "not LTS", never a scan failure.
        release("v26.12.0", days_ago(11), json!("")),
        release("v26.11.1", days_ago(18), json!(null)),
        release("v26.11.0", days_ago(25), json!(true)),
    ]))
    .await;

    assert_eq!(versions(&result).len(), 4);
    assert_eq!(result.default_version, None);
}

#[tokio::test]
async fn missing_lts_field_reads_as_not_lts() {
    let mut entry = release("v26.12.1", days_ago(4), json!("Ignored"));
    entry.as_object_mut().unwrap().remove("lts");
    let result = fetch(json!([entry])).await;

    assert_eq!(versions(&result), vec!["26.12.1"]);
    assert_eq!(result.default_version, None);
}

#[tokio::test]
async fn ninety_day_window_includes_edge_and_excludes_day_before() {
    let result = fetch(json!([
        release("v26.12.1", days_ago(6), json!(false)),
        release("v26.12.0", days_ago(40), json!(false)),
        release("v26.11.1", days_ago(90), json!(false)),
        release("v26.11.0", days_ago(91), json!(false)),
    ]))
    .await;

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
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/dist/index.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            release("v26.11.1", days_ago(90), json!(false)),
            release("v26.11.0", days_ago(91), json!(false)),
        ])))
        .mount(&server)
        .await;
    let late = now() + Duration::hours(23) + Duration::minutes(59);

    let result = NodeAdapter::with_fixed_now(server.uri(), late)
        .fetch(&node_index())
        .await
        .unwrap();

    assert_eq!(result.signals.releases_last_90d, Some(1));
}

#[tokio::test]
async fn empty_index_yields_no_versions_and_zero_activity() {
    let result = fetch(json!([])).await;

    assert!(result.upstream_versions.is_empty());
    assert_eq!(result.signals.releases_last_90d, Some(0));
    assert_eq!(result.signals.last_release_at, None);
    assert_eq!(result.default_version, None);
}
