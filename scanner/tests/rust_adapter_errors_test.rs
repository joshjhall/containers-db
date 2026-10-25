//! Error-path tests for `RustAdapter`: unusable `source_repo`, non-2xx
//! responses, and the request timeout (issue #467). Split from
//! `rust_adapter_test.rs`, which covers release selection and activity
//! signals. No live network — `wiremock` or a raw loopback socket stands in
//! for the GitHub REST API.

mod common;

use common::{serve_partial_body, serve_stalled_body};
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

/// A stalled api.github.com must fail this tool's scan, not hang
/// `scanner due` (issue #467). The server holds the response far past the
/// test timeout; the elapsed bound proves the adapter gave up rather than
/// waiting it out.
#[tokio::test]
async fn stalled_upstream_times_out_with_an_error_naming_the_url() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/rust-lang/rust"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({}))
                .set_delay(std::time::Duration::from_secs(5)),
        )
        .mount(&server)
        .await;

    let started = std::time::Instant::now();
    let err = RustAdapter::with_timeout(server.uri(), std::time::Duration::from_millis(200))
        .fetch(&rust_index(&server.uri()))
        .await
        .unwrap_err()
        .to_string();

    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "took {elapsed:?}"
    );
    let expected = format!(
        "GET {}/repos/rust-lang/rust failed: timed out:",
        server.uri()
    );
    assert!(err.starts_with(&expected), "{err}");
}

/// The client timeout spans the body read, so a stall after the headers must
/// also fail fast and be reported as a timeout (in the JSON decode step that
/// reads the body), not as a malformed response.
#[tokio::test]
async fn stall_mid_body_times_out_with_an_error_naming_the_url() {
    let uri = serve_stalled_body(
        b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 1000\r\n\r\n{\"ar",
    );

    let started = std::time::Instant::now();
    let err = RustAdapter::with_timeout(&uri, std::time::Duration::from_millis(200))
        .fetch(&rust_index(&uri))
        .await
        .unwrap_err()
        .to_string();

    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "took {elapsed:?}"
    );
    let expected = format!("GET {uri}/repos/rust-lang/rust JSON decode failed: timed out:");
    assert!(err.starts_with(&expected), "{err}");
}

/// A non-2xx with a readable body reports the status and the body verbatim.
#[tokio::test]
async fn non_2xx_error_names_the_url_status_and_body() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/rust-lang/rust"))
        .respond_with(ResponseTemplate::new(503).set_body_string("down"))
        .mount(&server)
        .await;

    let err = RustAdapter::with_api_base(server.uri())
        .fetch(&rust_index(&server.uri()))
        .await
        .unwrap_err()
        .to_string();

    let expected = format!(
        "GET {}/repos/rust-lang/rust returned 503 Service Unavailable: down",
        server.uri()
    );
    assert_eq!(err, expected);
}

/// A non-2xx whose error body stalls mid-read must keep the status and say
/// the body could not be read — naming the timeout — rather than reporting
/// an empty body (issue #473).
#[tokio::test]
async fn non_2xx_with_stalled_body_reports_unreadable_body() {
    let uri = serve_stalled_body(
        b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 1000\r\n\r\nupstream is",
    );

    let started = std::time::Instant::now();
    let err = RustAdapter::with_timeout(&uri, std::time::Duration::from_millis(200))
        .fetch(&rust_index(&uri))
        .await
        .unwrap_err()
        .to_string();

    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "took {elapsed:?}"
    );
    let expected = format!(
        "GET {uri}/repos/rust-lang/rust returned 503 Service Unavailable: \
         <body unreadable: timed out:"
    );
    assert!(err.starts_with(&expected), "{err}");
    assert!(err.ends_with('>'), "{err}");
}

/// A non-2xx whose error body is cut off by the connection closing (not a
/// stall) still reports the body as unreadable — and without the `timed out:`
/// tag, which is reserved for timeouts.
#[tokio::test]
async fn non_2xx_with_truncated_body_is_unreadable_not_timed_out() {
    let uri = serve_partial_body(
        b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 1000\r\n\r\nupstream is",
        std::time::Duration::ZERO,
    );

    let err = RustAdapter::with_timeout(&uri, std::time::Duration::from_secs(5))
        .fetch(&rust_index(&uri))
        .await
        .unwrap_err()
        .to_string();

    let expected = format!(
        "GET {uri}/repos/rust-lang/rust returned 503 Service Unavailable: <body unreadable: "
    );
    assert!(err.starts_with(&expected), "{err}");
    assert!(!err.contains("timed out:"), "{err}");
    assert!(err.ends_with('>'), "{err}");
}
