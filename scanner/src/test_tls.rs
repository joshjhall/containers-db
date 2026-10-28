//! A throwaway PKI and a loopback HTTPS server, for the tests that prove a
//! client's trust decision (issue #512).
//!
//! The PKI is generated per test, so no private key is committed:
//! `tests/fixtures/test-root.pem` had its key discarded and cannot serve.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A CA and a leaf for `localhost` signed by it, all PEM-encoded.
pub(crate) struct TestPki {
    pub(crate) ca_pem: String,
    pub(crate) leaf_pem: String,
    pub(crate) key_pem: String,
}

impl TestPki {
    pub(crate) fn generate() -> Self {
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.distinguished_name.push(
            rcgen::DnType::CommonName,
            "containers-db scanner throwaway CA",
        );
        let ca_key = KeyPair::generate().unwrap();
        let ca_pem = ca_params.self_signed(&ca_key).unwrap().pem();
        let issuer = Issuer::new(ca_params, ca_key);

        let leaf_key = KeyPair::generate().unwrap();
        let leaf = CertificateParams::new(vec!["localhost".to_owned()])
            .unwrap()
            .signed_by(&leaf_key, &issuer)
            .unwrap();
        Self {
            ca_pem,
            leaf_pem: leaf.pem(),
            key_pem: leaf_key.serialize_pem(),
        }
    }

    /// Write the PEMs into `dir` as `ca.pem`, `leaf.pem` and `key.pem`.
    pub(crate) fn write_to(&self, dir: &Path) {
        for (name, pem) in [
            ("ca.pem", &self.ca_pem),
            ("leaf.pem", &self.leaf_pem),
            ("key.pem", &self.key_pem),
        ] {
            std::fs::write(dir.join(name), pem).unwrap();
        }
    }

    /// Read back what [`TestPki::write_to`] wrote.
    pub(crate) fn read_from(dir: &Path) -> Self {
        let read = |name: &str| std::fs::read_to_string(dir.join(name)).unwrap();
        Self {
            ca_pem: read("ca.pem"),
            leaf_pem: read("leaf.pem"),
            key_pem: read("key.pem"),
        }
    }

    /// A client that trusts only this CA: the control that shows the server
    /// is reachable, so a rejection elsewhere is down to trust. It skips any
    /// `HTTPS_PROXY` so the request reaches loopback, and is bounded like the
    /// production client so a stalled server fails rather than hangs.
    pub(crate) fn trusting_client(&self) -> reqwest::Client {
        let ca = reqwest::Certificate::from_pem(self.ca_pem.as_bytes()).unwrap();
        reqwest::Client::builder()
            .tls_certs_only([ca])
            .no_proxy()
            .timeout(crate::adapters::DEFAULT_REQUEST_TIMEOUT)
            .build()
            .unwrap()
    }

    /// Serve `HTTP/1.1 200` with an empty body over TLS on a loopback port,
    /// presenting the leaf, until the runtime shuts down. Returns
    /// `https://localhost:{port}/`.
    pub(crate) async fn serve(&self) -> String {
        let chain = vec![CertificateDer::from_pem_slice(self.leaf_pem.as_bytes()).unwrap()];
        let key = PrivateKeyDer::from_pem_slice(self.key_pem.as_bytes()).unwrap();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));

        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                // A failed accept (ECONNABORTED, EMFILE) loses one
                // connection, not the server.
                let tcp = match listener.accept().await {
                    Ok((tcp, _)) => tcp,
                    Err(e) => {
                        eprintln!("test_tls: accept failed: {e}");
                        continue;
                    }
                };
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    // A client that rejects the certificate aborts the
                    // handshake; that is the expected outcome, not an error.
                    let Ok(mut tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let mut buf = [0u8; 4096];
                    let _ = tls.read(&mut buf).await;
                    let _ = tls
                        .write_all(
                            b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                        )
                        .await;
                    let _ = tls.shutdown().await;
                });
            }
        });
        format!("https://localhost:{port}/")
    }
}

/// GET `url` and require a 200, so a rejection elsewhere is down to trust
/// and not to the server.
pub(crate) async fn assert_reachable(client: &reqwest::Client, url: &str) {
    let resp = client.get(url).send().await.unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
}

/// GET `url` and require that it fails because the server's certificate was
/// not trusted. A refused connection or a timeout does not count: the chain
/// must hold a `rustls::Error::InvalidCertificate`.
pub(crate) async fn assert_cert_rejected(client: &reqwest::Client, url: &str) {
    let err = client.get(url).send().await.unwrap_err();
    let rejected =
        find_rustls_error(&err).is_some_and(|e| matches!(e, rustls::Error::InvalidCertificate(_)));
    assert!(rejected, "expected a certificate rejection: {err:?}");
}

/// The first `rustls::Error` in `err`'s source chain. `io::Error::source`
/// skips the error it wraps, so look inside each one with `get_ref`.
pub(crate) fn find_rustls_error<'a>(
    err: &'a (dyn std::error::Error + 'static),
) -> Option<&'a rustls::Error> {
    let mut next = Some(err);
    while let Some(e) = next {
        if let Some(found) = e.downcast_ref::<rustls::Error>() {
            return Some(found);
        }
        if let Some(inner) = e
            .downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::get_ref)
            && let Some(found) = find_rustls_error(inner)
        {
            return Some(found);
        }
        next = e.source();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::find_rustls_error;

    #[derive(Debug)]
    struct Wrapper(std::io::Error);

    impl std::fmt::Display for Wrapper {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("wrapper")
        }
    }

    impl std::error::Error for Wrapper {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    /// The `io::Error` layer is the one `source()` alone walks past, so a
    /// rustls cause under it must still be found.
    #[test]
    fn finds_rustls_error_inside_io_error() {
        let err = Wrapper(std::io::Error::other(rustls::Error::General("x".into())));
        assert!(matches!(
            find_rustls_error(&err),
            Some(rustls::Error::General(_))
        ));
    }

    #[test]
    fn no_rustls_error_is_none() {
        let err = Wrapper(std::io::Error::other("refused"));
        assert!(find_rustls_error(&err).is_none());
    }
}
