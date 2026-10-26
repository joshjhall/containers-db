//! Error-path tests for `RustAdapter`: unusable `source_repo`, non-2xx
//! responses, and the request timeout (issue #467). Split from
//! `rust_adapter_test.rs`, which covers release selection and activity
//! signals. No live network — `wiremock` or a raw loopback socket stands in
//! for the GitHub REST API.

mod common;

use common::{serve_chunked_body, serve_partial_body, serve_stalled_body};
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
    assert_eq!(err.to_string(), "tool `rust` has no source_repo");
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
    assert_eq!(
        err.to_string(),
        "tool `rust` source_repo `https://gitlab.com/rust-lang/rust` is not a GitHub URL \
         the rust adapter understands"
    );
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

/// A 200 with a malformed body is a plain decode failure: the `timed out:`
/// tag is reserved for timeouts.
#[tokio::test]
async fn malformed_json_is_a_decode_error_not_a_timeout() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/rust-lang/rust"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{not json"))
        .mount(&server)
        .await;

    let err = RustAdapter::with_api_base(server.uri())
        .fetch(&rust_index(&server.uri()))
        .await
        .unwrap_err()
        .to_string();

    let expected = format!(
        "GET {}/repos/rust-lang/rust JSON decode failed:",
        server.uri()
    );
    assert!(err.starts_with(&expected), "{err}");
    assert!(!err.contains("timed out:"), "{err}");
}

/// A refused connection keeps the plain `GET {url} failed:` form, so the run
/// log does not blame a dead host on a slow one.
#[tokio::test]
async fn refused_connection_is_not_reported_as_a_timeout() {
    // Bind an ephemeral port and drop the listener so nothing is accepting
    // on it. Not a dropped MockServer: wiremock pools servers, so the port
    // would still answer (with a 404).
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let uri = format!("http://127.0.0.1:{port}");

    let err = RustAdapter::with_timeout(&uri, std::time::Duration::from_secs(5))
        .fetch(&rust_index(&uri))
        .await
        .unwrap_err()
        .to_string();

    let expected = format!("GET {uri}/repos/rust-lang/rust failed:");
    assert!(err.starts_with(&expected), "{err}");
    assert!(!err.contains("timed out:"), "{err}");
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

/// An oversized body with a line break and a bidi override is capped and
/// escaped, so the error stays one bounded log line (issue #475).
#[tokio::test]
async fn oversized_error_body_is_capped_and_escaped() {
    let server = MockServer::start().await;
    let body = format!("a\u{2028}b\u{202e}c{}", "z".repeat(600));
    Mock::given(method("GET"))
        .and(path("/repos/rust-lang/rust"))
        .respond_with(ResponseTemplate::new(503).set_body_string(body))
        .mount(&server)
        .await;

    let err = RustAdapter::with_api_base(server.uri())
        .fetch(&rust_index(&server.uri()))
        .await
        .unwrap_err()
        .to_string();

    // 609 body bytes (U+2028 and U+202E are 3 each): the first 512 are kept.
    let expected = format!(
        "GET {}/repos/rust-lang/rust returned 503 Service Unavailable: \
         a\\u{{2028}}b\\u{{202e}}c{} … (truncated: body over 512 bytes)",
        server.uri(),
        "z".repeat(503)
    );
    assert_eq!(err, expected);
}

/// A body far over the cap that then stalls is not read to the end: the
/// error returns as soon as the cap is reached, not at the client timeout
/// (issue #477).
#[tokio::test]
async fn oversized_error_body_is_not_read_to_the_end() {
    let mut head = b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 100000\r\n\r\n".to_vec();
    head.extend_from_slice(&[b'q'; 600]);
    let uri = serve_stalled_body(head);

    let started = std::time::Instant::now();
    let err = RustAdapter::with_timeout(&uri, std::time::Duration::from_secs(10))
        .fetch(&rust_index(&uri))
        .await
        .unwrap_err()
        .to_string();

    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "took {elapsed:?}"
    );
    let expected = format!(
        "GET {uri}/repos/rust-lang/rust returned 503 Service Unavailable: \
         {} … (truncated: body over 512 bytes)",
        "q".repeat(512)
    );
    assert_eq!(err, expected);
}

/// The prefix is accumulated across many small chunks, kept in order, and
/// still cut at exactly the cap (issue #477).
#[tokio::test]
async fn error_body_sent_in_small_chunks_is_accumulated_then_capped() {
    let body: Vec<u8> = (0..700).map(|i| b'a' + (i % 26) as u8).collect();
    let kept = String::from_utf8(body[..512].to_vec()).unwrap();
    let uri = serve_chunked_body(
        b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 100000\r\n\r\n",
        body,
        100,
    );

    let started = std::time::Instant::now();
    let err = RustAdapter::with_timeout(&uri, std::time::Duration::from_secs(10))
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
         {kept} … (truncated: body over 512 bytes)"
    );
    assert_eq!(err, expected);
}

/// A body of exactly the cap that ends there is shown whole: the slack byte
/// is what tells it apart from one that continues.
#[tokio::test]
async fn error_body_of_exactly_the_cap_is_not_marked_truncated() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/rust-lang/rust"))
        .respond_with(ResponseTemplate::new(503).set_body_string("e".repeat(512)))
        .mount(&server)
        .await;

    let err = RustAdapter::with_api_base(server.uri())
        .fetch(&rust_index(&server.uri()))
        .await
        .unwrap_err()
        .to_string();

    let expected = format!(
        "GET {}/repos/rust-lang/rust returned 503 Service Unavailable: {}",
        server.uri(),
        "e".repeat(512)
    );
    assert_eq!(err, expected);
}

/// Invalid UTF-8 in the body is replaced, as `text()` did, not rejected.
#[tokio::test]
async fn invalid_utf8_error_body_is_replaced() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/rust-lang/rust"))
        .respond_with(ResponseTemplate::new(503).set_body_bytes(b"bad\xffbyte".to_vec()))
        .mount(&server)
        .await;

    let err = RustAdapter::with_api_base(server.uri())
        .fetch(&rust_index(&server.uri()))
        .await
        .unwrap_err()
        .to_string();

    let expected = format!(
        "GET {}/repos/rust-lang/rust returned 503 Service Unavailable: bad\u{FFFD}byte",
        server.uri()
    );
    assert_eq!(err, expected);
}

/// A non-2xx on a later request (after `/repos` succeeds) names that
/// request's own URL, not an earlier step's.
#[tokio::test]
async fn non_2xx_on_releases_names_the_releases_url() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/rust-lang/rust"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "archived": false,
            "disabled": false,
            "pushed_at": "2026-10-01T00:00:00Z"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/rust-lang/rust/releases"))
        .respond_with(ResponseTemplate::new(503).set_body_string("down"))
        .mount(&server)
        .await;

    let err = RustAdapter::with_api_base(server.uri())
        .fetch(&rust_index(&server.uri()))
        .await
        .unwrap_err()
        .to_string();

    let expected = format!(
        "GET {}/repos/rust-lang/rust/releases?per_page=100 returned 503 Service Unavailable: down",
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
