//! Error-path tests for `NodeAdapter`: decode failures, non-2xx responses,
//! and the request timeout (issue #467). Mirrors `go_adapter_errors_test.rs`;
//! the body-cap mechanics shared through `adapters::status_error` are
//! unit-tested in `adapters/mod.rs`, so only the cap's wiring through
//! node's URL is repeated here. No live network — `wiremock` or a raw
//! loopback socket stands in for nodejs.org.

mod common;

use chrono::{DateTime, TimeZone, Utc};
use common::{serve_chunked_body, serve_partial_body, serve_stalled_body};
use scanner::adapter::{Adapter, ToolIndex};
use scanner::adapters::node::NodeAdapter;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 25, 0, 0, 0).unwrap()
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

async fn serve_status(status: u16, body: impl Into<Vec<u8>>) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/dist/index.json"))
        .respond_with(ResponseTemplate::new(status).set_body_bytes(body.into()))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn non_json_index_is_a_decode_error() {
    let server = serve_status(200, "<html>maintenance</html>").await;

    let err = NodeAdapter::with_fixed_now(server.uri(), now())
        .unwrap()
        .fetch(&node_index())
        .await
        .unwrap_err()
        .to_string();

    assert!(
        err.starts_with("nodejs.org release index JSON decode failed: "),
        "{err}"
    );
}

/// An entry missing a field the adapter needs fails the scan loudly rather
/// than being skipped: a silently-shrunk feed would read as "no new
/// releases".
#[tokio::test]
async fn entry_without_a_date_is_a_decode_error() {
    let server = serve_status(
        200,
        json!([{ "version": "v26.12.2", "files": ["linux-x64"] }]).to_string(),
    )
    .await;

    let err = NodeAdapter::with_fixed_now(server.uri(), now())
        .unwrap()
        .fetch(&node_index())
        .await
        .unwrap_err()
        .to_string();

    assert!(
        err.starts_with("nodejs.org release index JSON decode failed: "),
        "{err}"
    );
    assert!(err.contains("date"), "{err}");
}

#[tokio::test]
async fn index_error_names_the_url() {
    let server = serve_status(503, "down").await;

    let err = NodeAdapter::with_fixed_now(server.uri(), now())
        .unwrap()
        .fetch(&node_index())
        .await
        .unwrap_err()
        .to_string();

    let expected = format!(
        "GET {}/dist/index.json returned 503 Service Unavailable: down",
        server.uri()
    );
    assert_eq!(err, expected);
}

/// An oversized body with an embedded newline is capped and escaped, so the
/// error stays one bounded log line (issue #475).
#[tokio::test]
async fn oversized_error_body_is_capped_and_escaped() {
    let server = serve_status(503, format!("line1\nline2{}", "z".repeat(600))).await;

    let err = NodeAdapter::with_fixed_now(server.uri(), now())
        .unwrap()
        .fetch(&node_index())
        .await
        .unwrap_err()
        .to_string();

    // 611 body bytes: the first 512 are kept.
    let expected = format!(
        "GET {}/dist/index.json returned 503 Service Unavailable: \
         line1\\nline2{} … (truncated: body over 512 bytes)",
        server.uri(),
        "z".repeat(501)
    );
    assert_eq!(err, expected);
}

/// The prefix is accumulated across many small chunks, kept in order, and
/// still cut at exactly the cap — without reading the rest (issue #477).
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
    let err = NodeAdapter::with_timeout(&uri, std::time::Duration::from_secs(10))
        .unwrap()
        .fetch(&node_index())
        .await
        .unwrap_err()
        .to_string();

    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "took {elapsed:?}"
    );
    let expected = format!(
        "GET {uri}/dist/index.json returned 503 Service Unavailable: \
         {kept} … (truncated: body over 512 bytes)"
    );
    assert_eq!(err, expected);
}

/// A stalled nodejs.org must fail this tool's scan, not hang `scanner due`
/// (issue #467).
#[tokio::test]
async fn stalled_upstream_times_out_with_an_error_naming_the_url() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/dist/index.json"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!([]))
                .set_delay(std::time::Duration::from_secs(5)),
        )
        .mount(&server)
        .await;

    let started = std::time::Instant::now();
    let err = NodeAdapter::with_timeout(server.uri(), std::time::Duration::from_millis(200))
        .unwrap()
        .fetch(&node_index())
        .await
        .unwrap_err()
        .to_string();

    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "took {elapsed:?}"
    );
    let expected = format!("GET {}/dist/index.json failed: timed out:", server.uri());
    assert!(err.starts_with(&expected), "{err}");
}

/// The `timed out:` tag must be reserved for timeouts: a refused connection
/// keeps the plain `GET {url} failed:` form.
#[tokio::test]
async fn refused_connection_is_not_reported_as_a_timeout() {
    // Not a dropped MockServer: wiremock pools servers, so the port would
    // still answer (with a 404).
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let uri = format!("http://127.0.0.1:{port}");

    let err = NodeAdapter::with_timeout(&uri, std::time::Duration::from_secs(5))
        .unwrap()
        .fetch(&node_index())
        .await
        .unwrap_err()
        .to_string();

    let expected = format!("GET {uri}/dist/index.json failed:");
    assert!(err.starts_with(&expected), "{err}");
    assert!(!err.contains("timed out:"), "{err}");
}

/// A 200 whose body is cut off by the connection closing (not a stall) is a
/// plain body-read failure.
#[tokio::test]
async fn truncated_body_is_a_body_read_error_not_a_timeout() {
    let uri = serve_partial_body(
        b"HTTP/1.1 200 OK\r\ncontent-length: 1000\r\n\r\n[{\"v",
        std::time::Duration::ZERO,
    );

    let err = NodeAdapter::with_timeout(&uri, std::time::Duration::from_secs(5))
        .unwrap()
        .fetch(&node_index())
        .await
        .unwrap_err()
        .to_string();

    let expected = format!("GET {uri}/dist/index.json body read failed:");
    assert!(err.starts_with(&expected), "{err}");
    assert!(!err.contains("timed out:"), "{err}");
}

/// The client timeout spans the body read, so a stall after the headers is
/// reported as a timeout, not as a body-read error of unknown cause.
#[tokio::test]
async fn stall_mid_body_times_out_with_an_error_naming_the_url() {
    let uri = serve_stalled_body(b"HTTP/1.1 200 OK\r\ncontent-length: 1000\r\n\r\n[{\"v");

    let started = std::time::Instant::now();
    let err = NodeAdapter::with_timeout(&uri, std::time::Duration::from_millis(200))
        .unwrap()
        .fetch(&node_index())
        .await
        .unwrap_err()
        .to_string();

    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "took {elapsed:?}"
    );
    let expected = format!("GET {uri}/dist/index.json body read failed: timed out:");
    assert!(err.starts_with(&expected), "{err}");
}

/// A non-2xx whose error body stalls mid-read keeps the status and names the
/// timeout rather than reporting an empty body (issue #473).
#[tokio::test]
async fn non_2xx_with_stalled_body_reports_unreadable_body() {
    let uri = serve_stalled_body(
        b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 1000\r\n\r\nupstream is",
    );

    let err = NodeAdapter::with_timeout(&uri, std::time::Duration::from_millis(200))
        .unwrap()
        .fetch(&node_index())
        .await
        .unwrap_err()
        .to_string();

    let expected = format!(
        "GET {uri}/dist/index.json returned 503 Service Unavailable: \
         <body unreadable: timed out:"
    );
    assert!(err.starts_with(&expected), "{err}");
    assert!(err.ends_with('>'), "{err}");
}

/// A non-2xx whose error body is cut off by the connection closing is
/// unreadable without the `timed out:` tag.
#[tokio::test]
async fn non_2xx_with_truncated_body_is_unreadable_not_timed_out() {
    let uri = serve_partial_body(
        b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 1000\r\n\r\nupstream is",
        std::time::Duration::ZERO,
    );

    let err = NodeAdapter::with_timeout(&uri, std::time::Duration::from_secs(5))
        .unwrap()
        .fetch(&node_index())
        .await
        .unwrap_err()
        .to_string();

    let expected =
        format!("GET {uri}/dist/index.json returned 503 Service Unavailable: <body unreadable: ");
    assert!(err.starts_with(&expected), "{err}");
    assert!(!err.contains("timed out:"), "{err}");
    assert!(err.ends_with('>'), "{err}");
}
