//! Adapter registry — maps tool id → [`crate::adapter::Adapter`]
//! implementation. Tools without a registered adapter are silently
//! skipped (added to the catalog so the schema validates, but no
//! upstream lookup happens until someone writes the adapter).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::adapter::Adapter;

pub mod go;
pub mod rust;

const USER_AGENT: &str = "containers-db-scanner/0.1";

/// Whole-request timeout for every adapter's HTTP client. `scanner due`
/// scans tools one at a time, so without it a single stalled upstream would
/// hang every tool after it instead of failing just that one (issue #467).
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// TCP/TLS connect timeout, capped at the request timeout so a short
/// test-only request timeout also bounds the connect.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The HTTP client every adapter builds on, so each one gets the user agent
/// and both timeouts without repeating them.
pub(crate) fn http_client(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(timeout)
        .connect_timeout(connect_timeout_for(timeout))
        .build()
        .expect("reqwest client build")
}

fn connect_timeout_for(request_timeout: Duration) -> Duration {
    CONNECT_TIMEOUT.min(request_timeout)
}

/// Map a failed request step to the `GET {url} {what}: …` error every adapter
/// reports, where `what` names the step (`failed` for `send()`, `body read
/// failed`, `JSON decode failed`). The client timeout spans the whole request,
/// so it can fire in any of those steps; reqwest's own message does not say a
/// timeout fired, so one is named explicitly — a stalled upstream should read
/// as such in the run log whichever step it stalled in.
pub(crate) fn request_error(url: &str, what: &str, e: reqwest::Error) -> anyhow::Error {
    if e.is_timeout() {
        anyhow::anyhow!("GET {url} {what}: timed out: {e}")
    } else {
        anyhow::anyhow!("GET {url} {what}: {e}")
    }
}

/// Map a non-2xx response to `GET {url} returned {status}: {body}`. The body
/// read can itself fail — an upstream that sends a 503's headers and then
/// stalls trips the client timeout mid-read — so a failed read is reported as
/// `<body unreadable: …>` (tagged `timed out:` like [`request_error`]) rather
/// than as an empty body that hides why the message is blank (issue #473).
/// A readable body is passed through [`sanitize_error_body`] so an oversized
/// or hostile upstream body cannot bloat or split the log line (issue #475).
pub(crate) async fn status_error(url: &str, response: reqwest::Response) -> anyhow::Error {
    let status = response.status();
    let body = match response.text().await {
        Ok(body) => sanitize_error_body(&body),
        Err(e) if e.is_timeout() => format!("<body unreadable: timed out: {e}>"),
        Err(e) => format!("<body unreadable: {e}>"),
    };
    anyhow::anyhow!("GET {url} returned {status}: {body}")
}

/// Longest upstream error body, in source bytes, embedded in a
/// [`status_error`] message.
const MAX_ERROR_BODY_BYTES: usize = 512;

/// Cap `body` at [`MAX_ERROR_BODY_BYTES`] (cut on a char boundary, with a
/// `… (N bytes truncated)` marker) and escape its control characters so it
/// stays on one log line. Only control characters are escaped — quotes and
/// backslashes pass through, so a short JSON error body reads as sent.
fn sanitize_error_body(body: &str) -> String {
    let cut = body.floor_char_boundary(MAX_ERROR_BODY_BYTES);
    let mut out = String::with_capacity(cut);
    for c in body[..cut].chars() {
        if c.is_control() {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    if cut < body.len() {
        out.push_str(&format!(" … ({} bytes truncated)", body.len() - cut));
    }
    out
}

/// Built-in registry shipped with the scanner. `register_all` callers
/// can extend this map for tests or future tools.
pub fn register_all() -> HashMap<String, Arc<dyn Adapter>> {
    let mut registry: HashMap<String, Arc<dyn Adapter>> = HashMap::new();
    registry.insert("go".to_string(), Arc::new(go::GoAdapter::new()));
    registry.insert("rust".to_string(), Arc::new(rust::RustAdapter::new()));
    registry
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `scanner due` silently skips any tool missing from this map, so an
    /// accidentally dropped registration would only show up as a tool that
    /// quietly stops getting new versions.
    #[test]
    fn registers_every_shipped_adapter() {
        let registry = register_all();
        let mut ids: Vec<&str> = registry.keys().map(String::as_str).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec!["go", "rust"]);
    }

    #[test]
    fn connect_timeout_is_capped_at_the_request_timeout() {
        assert_eq!(
            connect_timeout_for(DEFAULT_REQUEST_TIMEOUT),
            CONNECT_TIMEOUT
        );
        let short = Duration::from_millis(200);
        assert_eq!(connect_timeout_for(short), short);
    }

    #[test]
    fn short_error_body_passes_through_unchanged() {
        let body = r#"{"message":"API rate limit exceeded","path":"C:\\x"}"#;
        assert_eq!(sanitize_error_body(body), body);
        assert_eq!(sanitize_error_body(""), "");
    }

    #[test]
    fn control_characters_in_error_body_are_escaped() {
        assert_eq!(
            sanitize_error_body("a\nb\r\tc\x1bd\0"),
            r"a\nb\r\tc\u{1b}d\u{0}"
        );
    }

    #[test]
    fn error_body_at_the_cap_is_not_truncated() {
        let body = "x".repeat(MAX_ERROR_BODY_BYTES);
        assert_eq!(sanitize_error_body(&body), body);
    }

    #[test]
    fn error_body_over_the_cap_is_truncated_with_a_marker() {
        let body = "x".repeat(MAX_ERROR_BODY_BYTES + 88);
        let expected = format!(
            "{} … (88 bytes truncated)",
            "x".repeat(MAX_ERROR_BODY_BYTES)
        );
        assert_eq!(sanitize_error_body(&body), expected);
    }

    #[test]
    fn truncation_cuts_on_a_char_boundary() {
        // "é" is 2 bytes; it straddles the cap, so the cut lands before it.
        let body = format!("{}é{}", "x".repeat(MAX_ERROR_BODY_BYTES - 1), "y".repeat(9));
        let expected = format!(
            "{} … (11 bytes truncated)",
            "x".repeat(MAX_ERROR_BODY_BYTES - 1)
        );
        assert_eq!(sanitize_error_body(&body), expected);
    }

    #[test]
    fn escaping_applies_after_truncation() {
        // The cap bounds source bytes, so an escaped newline before the cut
        // does not push real bytes past it.
        let body = format!("\n{}", "x".repeat(MAX_ERROR_BODY_BYTES));
        let expected = format!(
            r"\n{} … (1 bytes truncated)",
            "x".repeat(MAX_ERROR_BODY_BYTES - 1)
        );
        assert_eq!(sanitize_error_body(&body), expected);
    }
}
