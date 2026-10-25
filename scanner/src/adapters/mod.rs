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
pub(crate) async fn status_error(url: &str, response: reqwest::Response) -> anyhow::Error {
    let status = response.status();
    let body = match response.text().await {
        Ok(body) => body,
        Err(e) if e.is_timeout() => format!("<body unreadable: timed out: {e}>"),
        Err(e) => format!("<body unreadable: {e}>"),
    };
    anyhow::anyhow!("GET {url} returned {status}: {body}")
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
}
