//! Adapter registry — maps tool id → [`crate::adapter::Adapter`]
//! implementation. Tools without a registered adapter are silently
//! skipped (added to the catalog so the schema validates, but no
//! upstream lookup happens until someone writes the adapter).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;

use crate::adapter::Adapter;

pub mod go;
pub mod node;
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
///
/// The `.connect_timeout(..)` wiring is deliberately not pinned by a test:
/// only [`connect_timeout_for`] is. Observing it needs a TCP connect that
/// stalls rather than fails, and nothing hermetic produces one portably — a
/// non-routable address fails fast or hangs depending on the host's routing,
/// and a full listen backlog stalls a connect only on some kernels. A loopback
/// refusal fails immediately either way, so it cannot tell the call apart.
///
/// TLS trust comes from the OS store: reqwest 0.13's `rustls` feature verifies
/// through `rustls-platform-verifier`, which on Linux loads the system roots
/// (or `SSL_CERT_FILE`/`SSL_CERT_DIR`) here, in `build()`, and fails when it
/// finds none. reqwest 0.13 has no bundled-roots option, so that failure is
/// returned with a pointer at the fix rather than panicking the whole run
/// (issue #507). The crypto provider is reqwest's default, `aws-lc-rs`; see
/// `docs/scanner.md` for both requirements.
pub(crate) fn http_client(timeout: Duration) -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(timeout)
        .connect_timeout(connect_timeout_for(timeout))
        .build()
        .context(
            "building the HTTP client failed (TLS roots are loaded from the OS trust \
             store at build time: install ca-certificates or set SSL_CERT_FILE/SSL_CERT_DIR)",
        )
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
/// Only a bounded prefix of the body is read ([`read_error_body_prefix`]) and
/// it is rendered through [`render_error_body`], so an oversized or hostile
/// upstream body can neither split the log line nor be buffered whole
/// (issues #475, #477).
pub(crate) async fn status_error(url: &str, mut response: reqwest::Response) -> anyhow::Error {
    let status = response.status();
    let body = match read_error_body_prefix(&mut response).await {
        Ok((prefix, ended)) => render_error_body(&prefix, ended),
        Err(e) if e.is_timeout() => format!("<body unreadable: timed out: {e}>"),
        Err(e) => format!("<body unreadable: {e}>"),
    };
    anyhow::anyhow!("GET {url} returned {status}: {body}")
}

/// Longest upstream error body embedded in a [`status_error`] message, in
/// body bytes after content decoding (gzip), so it bounds memory too. The cap
/// applies before escaping, so a body of escaped characters can render
/// several times longer — still bounded.
const MAX_ERROR_BODY_BYTES: usize = 512;

/// Bytes read past the cap: room for the UTF-8 sequence straddling it, plus
/// one byte so a body that ends exactly at the cap is told apart from one
/// that continues.
const ERROR_BODY_SLACK: usize = 4;

/// Read at most [`MAX_ERROR_BODY_BYTES`] + [`ERROR_BODY_SLACK`] bytes of the
/// body, returning them and whether the body ended within that. Once over the
/// cap it stops pulling chunks, so a huge or never-ending body costs one
/// bounded buffer rather than everything sent before the client timeout.
async fn read_error_body_prefix(
    response: &mut reqwest::Response,
) -> Result<(Vec<u8>, bool), reqwest::Error> {
    let limit = MAX_ERROR_BODY_BYTES + ERROR_BODY_SLACK;
    let mut prefix = Vec::new();
    while prefix.len() < limit {
        match response.chunk().await? {
            Some(chunk) => {
                let take = chunk.len().min(limit - prefix.len());
                prefix.extend_from_slice(&chunk[..take]);
            }
            None => return Ok((prefix, true)),
        }
    }
    Ok((prefix, false))
}

/// Render a body prefix for the log line: decode lossily (as `text()` did —
/// invalid UTF-8 becomes U+FFFD), escape the characters that could break or
/// spoof the line ([`breaks_log_line`]), and pass everything else — quotes
/// and backslashes included — through, so a short JSON error body reads as
/// sent. A body over the cap is cut on a char boundary of the raw bytes and
/// marked; its full size is unknown, since the rest was never read.
fn render_error_body(prefix: &[u8], ended: bool) -> String {
    let truncated = !ended || prefix.len() > MAX_ERROR_BODY_BYTES;
    let kept = if truncated {
        utf8_floor(prefix, MAX_ERROR_BODY_BYTES)
    } else {
        prefix
    };
    let mut out = String::with_capacity(kept.len());
    for c in String::from_utf8_lossy(kept).chars() {
        if breaks_log_line(c) {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    if truncated {
        out.push_str(&format!(
            " … (truncated: body over {MAX_ERROR_BODY_BYTES} bytes)"
        ));
    }
    out
}

/// `bytes[..max]`, backed off so it does not end inside a UTF-8 sequence.
/// A continuation byte (`0b10xx_xxxx`) at `max` means the cut would split a
/// char; at most 3 can follow a lead byte, so the back-off is bounded and
/// invalid input still yields a non-empty prefix.
fn utf8_floor(bytes: &[u8], max: usize) -> &[u8] {
    if bytes.len() <= max {
        return bytes;
    }
    let mut cut = max;
    while cut > max.saturating_sub(3) && bytes[cut] & 0b1100_0000 == 0b1000_0000 {
        cut -= 1;
    }
    &bytes[..cut]
}

/// Characters that can break a log line or hide/reorder what follows it:
/// controls (Cc, incl. NEL), the line/paragraph separators (Zl, Zp) that many
/// viewers render as a break, and every format character (Cf — bidi
/// overrides, zero-width and invisible characters, tags). The Cf ranges are
/// the complete category as of Unicode 15.1; std exposes no category lookup.
fn breaks_log_line(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{2028}'
                | '\u{2029}'
                | '\u{00AD}'
                | '\u{0600}'..='\u{0605}'
                | '\u{061C}'
                | '\u{06DD}'
                | '\u{070F}'
                | '\u{0890}'..='\u{0891}'
                | '\u{08E2}'
                | '\u{180E}'
                | '\u{200B}'..='\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{206F}'
                | '\u{FEFF}'
                | '\u{FFF9}'..='\u{FFFB}'
                | '\u{110BD}'
                | '\u{110CD}'
                | '\u{13430}'..='\u{1343F}'
                | '\u{1BCA0}'..='\u{1BCA3}'
                | '\u{1D173}'..='\u{1D17A}'
                | '\u{E0001}'
                | '\u{E0020}'..='\u{E007F}'
        )
}

/// Built-in registry shipped with the scanner. `register_all` callers
/// can extend this map for tests or future tools. Fails when an adapter's
/// HTTP client cannot be built (see [`http_client`]).
pub fn register_all() -> anyhow::Result<HashMap<String, Arc<dyn Adapter>>> {
    let mut registry: HashMap<String, Arc<dyn Adapter>> = HashMap::new();
    registry.insert("go".to_string(), Arc::new(go::GoAdapter::new()?));
    registry.insert("node".to_string(), Arc::new(node::NodeAdapter::new()?));
    registry.insert("rust".to_string(), Arc::new(rust::RustAdapter::new()?));
    Ok(registry)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MARKER: &str = " … (truncated: body over 512 bytes)";

    /// Render a whole UTF-8 body the way `status_error` does once it has been
    /// read to the end.
    fn sanitize_error_body(body: &str) -> String {
        render_error_body(body.as_bytes(), true)
    }

    /// `scanner due` silently skips any tool missing from this map, so an
    /// accidentally dropped registration would only show up as a tool that
    /// quietly stops getting new versions.
    #[test]
    fn registers_every_shipped_adapter() {
        let registry = register_all().unwrap();
        let mut ids: Vec<&str> = registry.keys().map(String::as_str).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec!["go", "node", "rust"]);
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
    fn unicode_line_breaks_and_bidi_controls_are_escaped() {
        assert_eq!(
            sanitize_error_body("a\u{85}b\u{2028}c\u{2029}d\u{202e}e\u{200b}f\u{2066}g\u{feff}h"),
            r"a\u{85}b\u{2028}c\u{2029}d\u{202e}e\u{200b}f\u{2066}g\u{feff}h"
        );
        // Ordinary non-ASCII text is not touched.
        assert_eq!(sanitize_error_body("café — ü"), "café — ü");
    }

    /// Both ends of every escaped range, and the code point just outside each
    /// end, so any range or singleton that shrinks or widens by one fails here.
    /// Generated from Unicode 15.1 Cf/Zl/Zp; no neighbour is itself Cc/Cf/Zl/Zp.
    #[test]
    fn escape_ranges_are_pinned_at_both_ends() {
        let escaped = [
            '\u{00AD}',
            '\u{0600}',
            '\u{0605}',
            '\u{061C}',
            '\u{06DD}',
            '\u{070F}',
            '\u{0890}',
            '\u{0891}',
            '\u{08E2}',
            '\u{180E}',
            '\u{200B}',
            '\u{200F}',
            '\u{2028}',
            '\u{2029}',
            '\u{202A}',
            '\u{202E}',
            '\u{2060}',
            '\u{2064}',
            '\u{2066}',
            '\u{206F}',
            '\u{FEFF}',
            '\u{FFF9}',
            '\u{FFFB}',
            '\u{110BD}',
            '\u{110CD}',
            '\u{13430}',
            '\u{1343F}',
            '\u{1BCA0}',
            '\u{1BCA3}',
            '\u{1D173}',
            '\u{1D17A}',
            '\u{E0001}',
            '\u{E0020}',
            '\u{E007F}',
        ];
        for c in escaped {
            assert!(breaks_log_line(c), "U+{:04X} should be escaped", c as u32);
        }
        let kept = [
            '\u{00AC}',
            '\u{00AE}',
            '\u{05FF}',
            '\u{0606}',
            '\u{061B}',
            '\u{061D}',
            '\u{06DC}',
            '\u{06DE}',
            '\u{070E}',
            '\u{0710}',
            '\u{088F}',
            '\u{0892}',
            '\u{08E1}',
            '\u{08E3}',
            '\u{180D}',
            '\u{180F}',
            '\u{200A}',
            '\u{2010}',
            '\u{2027}',
            '\u{202F}',
            '\u{205F}',
            '\u{2065}',
            '\u{2070}',
            '\u{FEFE}',
            '\u{FF00}',
            '\u{FFF8}',
            '\u{FFFC}',
            '\u{110BC}',
            '\u{110BE}',
            '\u{110CC}',
            '\u{110CE}',
            '\u{1342F}',
            '\u{13440}',
            '\u{1BC9F}',
            '\u{1BCA4}',
            '\u{1D172}',
            '\u{1D17B}',
            '\u{E0000}',
            '\u{E0002}',
            '\u{E001F}',
            '\u{E0080}',
        ];
        for c in kept {
            assert!(
                !breaks_log_line(c),
                "U+{:04X} should pass through",
                c as u32
            );
        }
    }

    #[test]
    fn error_body_at_the_cap_is_not_truncated() {
        let body = "x".repeat(MAX_ERROR_BODY_BYTES);
        assert_eq!(sanitize_error_body(&body), body);
    }

    #[test]
    fn error_body_over_the_cap_is_truncated_with_a_marker() {
        let body = "x".repeat(MAX_ERROR_BODY_BYTES + 1);
        let expected = format!("{}{MARKER}", "x".repeat(MAX_ERROR_BODY_BYTES));
        assert_eq!(sanitize_error_body(&body), expected);
    }

    /// A read that stopped early is truncated even if the bytes held happen
    /// to fit — the body continued past them.
    #[test]
    fn unfinished_read_is_marked_truncated() {
        assert_eq!(
            render_error_body(b"partial", false),
            format!("partial{MARKER}")
        );
    }

    #[test]
    fn truncation_cuts_on_a_char_boundary() {
        // "é" is 2 bytes; it straddles the cap, so the cut lands before it.
        let body = format!("{}é{}", "x".repeat(MAX_ERROR_BODY_BYTES - 1), "y".repeat(9));
        let expected = format!("{}{MARKER}", "x".repeat(MAX_ERROR_BODY_BYTES - 1));
        assert_eq!(sanitize_error_body(&body), expected);
    }

    #[test]
    fn utf8_floor_backs_off_at_most_three_bytes() {
        // A 4-byte char straddling the cap at each offset: the cut lands on
        // its lead byte, never inside it.
        for before in 1..=3 {
            let mut bytes = vec![b'x'; 8 - before];
            bytes.extend_from_slice("😀".as_bytes());
            assert_eq!(
                utf8_floor(&bytes, 8),
                &bytes[..8 - before],
                "before={before}"
            );
        }
        // Invalid input (all continuation bytes) still yields a bounded cut.
        assert_eq!(utf8_floor(&[0x80; 10], 8).len(), 5);
        assert_eq!(utf8_floor(b"short", 8), b"short");
    }

    #[test]
    fn invalid_utf8_is_replaced_not_escaped() {
        // U+FFFD is So, not Cf/Cc, so it passes through like `text()` gave it.
        assert_eq!(render_error_body(b"ok\xff\xfe", true), "ok\u{FFFD}\u{FFFD}");
    }

    #[test]
    fn escaping_applies_after_truncation() {
        // The cap bounds unescaped bytes, so an escaped newline before the cut
        // does not push real bytes past it.
        let body = format!("\n{}", "x".repeat(MAX_ERROR_BODY_BYTES));
        let expected = format!(r"\n{}{MARKER}", "x".repeat(MAX_ERROR_BODY_BYTES - 1));
        assert_eq!(sanitize_error_body(&body), expected);
    }
}
