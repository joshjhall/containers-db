//! End-to-end test of `RustAdapter` against a stubbed GitHub REST API.
//! No live network — `wiremock` serves recorded fixtures so the test is
//! hermetic and fast.

use scanner::adapter::{Adapter, ToolIndex};
use scanner::adapters::rust::RustAdapter;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

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
            "pushed_at": "2026-04-30T12:00:00Z"
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/rust-lang/rust/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "tag_name": "1.95.0", "published_at": "2026-04-15T00:00:00Z" },
            { "tag_name": "1.94.1", "published_at": "2026-03-01T00:00:00Z" },
            { "tag_name": "1.94.0", "published_at": "2026-02-01T00:00:00Z" },
            { "tag_name": "1.95.0-beta.1", "published_at": "2026-04-01T00:00:00Z", "prerelease": true },
            { "tag_name": "draft-internal", "published_at": null, "draft": true },
            { "tag_name": "nightly-2026-04-29", "published_at": "2026-04-29T00:00:00Z" }
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
    assert!(result.signals.releases_last_90d.unwrap() >= 1);
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
