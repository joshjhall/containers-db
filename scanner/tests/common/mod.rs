//! Loopback fixtures shared by the adapter error-path tests. Each serves one
//! raw HTTP response from a background thread, for the cases `wiremock`
//! cannot produce: headers that arrive while the body never does.

use std::io::{Read, Write};
use std::time::Duration;

/// Serve one connection that sends `head` (a status line, headers promising
/// more body than follows, and a body fragment), then stalls past any test
/// timeout. `ResponseTemplate`'s delay holds the headers back too, so it
/// cannot reach the body-read step.
pub fn serve_stalled_body(head: impl AsRef<[u8]>) -> String {
    serve_partial_body(head, Duration::from_secs(5))
}

/// Serve one connection that sends `head`, holds the socket open for `hold`,
/// then closes it short of the promised body.
///
/// The whole request is read before replying: closing a socket with unread
/// bytes in its receive buffer sends RST rather than FIN, and an RST can
/// discard the response the client has not yet read — so a zero `hold` would
/// intermittently surface as a failed `send()` instead of a truncated body.
pub fn serve_partial_body(head: impl AsRef<[u8]>, hold: Duration) -> String {
    let head = head.as_ref().to_vec();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        drain_request(&mut conn);
        let _ = conn.write_all(&head);
        std::thread::sleep(hold);
    });
    format!("http://{addr}")
}

/// Read until the blank line that ends the request headers (the adapters
/// only send bodiless GETs) or the client stops sending.
fn drain_request(conn: &mut std::net::TcpStream) {
    let mut request = Vec::new();
    let mut buf = [0u8; 4096];
    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
        match conn.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => request.extend_from_slice(&buf[..n]),
        }
    }
}
