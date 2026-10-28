//! A throwaway PKI and a loopback HTTPS server, for the tests that prove a
//! client's trust decision (issue #512).
//!
//! The PKI is generated per test, so no private key is committed:
//! `tests/fixtures/test-root.pem` had its key discarded and cannot serve.

use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A CA and a leaf for `127.0.0.1` signed by it, all PEM-encoded. The leaf
/// names the IP, not `localhost`, so no resolver can send a client to `::1`
/// while the server listens on IPv4 only.
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
        let leaf = CertificateParams::new(vec!["127.0.0.1".to_owned()])
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
    /// `https://127.0.0.1:{port}/`.
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
                // A connection that dies before it is accepted loses only
                // itself. Anything else (EMFILE) would fail again at once, so
                // stop rather than spin; the client then sees a refusal, and
                // the reason is on stderr.
                let tcp = match listener.accept().await {
                    Ok((tcp, _)) => tcp,
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::ConnectionAborted
                                | std::io::ErrorKind::ConnectionReset
                                | std::io::ErrorKind::Interrupted
                        ) =>
                    {
                        continue;
                    }
                    Err(e) => {
                        eprintln!("test_tls: accept failed, server stopping: {e}");
                        return;
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
        format!("https://127.0.0.1:{port}/")
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
/// must hold `InvalidCertificate(UnknownIssuer)`: no trust anchor for the CA,
/// which a name or validity problem would not produce. A root-less client
/// verifies with rustls's own WebPKI verifier on every OS (reqwest only uses
/// the platform verifier when it may consult the OS store), so the variant
/// is the same everywhere.
///
/// A client under test that honours the host's proxy sends the request to the
/// proxy instead (issue #514), so the failure names any proxy variable set.
pub(crate) async fn assert_cert_rejected(client: &reqwest::Client, url: &str) {
    let err = client.get(url).send().await.unwrap_err();
    assert!(
        find_rustls_error(&err).is_some_and(is_unknown_issuer),
        "{}",
        rejection_message(&err, |name| std::env::var_os(name))
    );
}

/// The failure message for [`assert_cert_rejected`]: the error, then the
/// [`proxy_hint`] for `var` when there is one.
fn rejection_message(err: &dyn std::fmt::Debug, var: impl Fn(&str) -> Option<OsString>) -> String {
    let hint = proxy_hint(var)
        .map(|hint| format!("\n{hint}"))
        .unwrap_or_default();
    format!("expected a certificate rejection: {err:?}{hint}")
}

/// The proxy variables reqwest reads for an `https` URL.
const PROXY_VARS: [&str; 4] = ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"];

/// A note naming each non-empty [`PROXY_VARS`] entry `var` reports, or `None`
/// if there are none. Names only: a proxy URL can carry credentials.
fn proxy_hint(var: impl Fn(&str) -> Option<OsString>) -> Option<String> {
    let set: Vec<&str> = PROXY_VARS
        .into_iter()
        .filter(|name| var(name).is_some_and(|v| !v.is_empty()))
        .collect();
    (!set.is_empty()).then(|| {
        format!(
            "proxy variable(s) set: {}; the request may have gone to the proxy, not the \
             test server. Unset them or add 127.0.0.1 to NO_PROXY",
            set.join(", ")
        )
    })
}

/// Whether `e` says the certificate chains to no trusted root, and nothing
/// else: a name or validity failure is a different variant.
fn is_unknown_issuer(e: &rustls::Error) -> bool {
    matches!(
        e,
        rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer)
    )
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

mod roots;

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::{find_rustls_error, is_unknown_issuer, proxy_hint, rejection_message};

    /// An environment lookup holding only `vars`.
    fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let vars: Vec<(String, String)> = vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |name| {
            vars.iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| OsString::from(v))
        }
    }

    #[test]
    fn no_proxy_vars_give_no_hint() {
        assert_eq!(proxy_hint(env(&[("NO_PROXY", "127.0.0.1")])), None);
    }

    /// Empty is how a proxy is unset for one command, so it does not count.
    #[test]
    fn empty_proxy_var_gives_no_hint() {
        assert_eq!(proxy_hint(env(&[("HTTPS_PROXY", "")])), None);
    }

    /// The hint names every variable set, and never prints a value, which
    /// can hold credentials.
    #[test]
    fn hint_names_each_proxy_var_but_not_its_value() {
        let hint = proxy_hint(env(&[
            ("HTTPS_PROXY", "http://user:secret@127.0.0.1:9"),
            ("all_proxy", "socks5://127.0.0.1:9"),
        ]))
        .unwrap();
        assert!(
            hint.starts_with("proxy variable(s) set: HTTPS_PROXY, all_proxy;"),
            "{hint}"
        );
        assert!(
            !hint.contains("secret") && !hint.contains("127.0.0.1:9"),
            "{hint}"
        );
    }

    /// Every variable reqwest reads for `https` is checked, in both cases.
    #[test]
    fn hint_covers_every_proxy_var() {
        let hint = proxy_hint(env(&[
            ("HTTPS_PROXY", "x"),
            ("https_proxy", "x"),
            ("ALL_PROXY", "x"),
            ("all_proxy", "x"),
        ]))
        .unwrap();
        assert!(
            hint.starts_with(
                "proxy variable(s) set: HTTPS_PROXY, https_proxy, ALL_PROXY, all_proxy;"
            ),
            "{hint}"
        );
    }

    /// The failure message carries the hint, so a proxied request reads as
    /// a proxy problem and not a trust one.
    #[test]
    fn rejection_message_appends_the_proxy_hint() {
        let msg = rejection_message(&"refused", env(&[("HTTPS_PROXY", "x")]));
        assert_eq!(
            msg,
            format!(
                "expected a certificate rejection: \"refused\"\n{}",
                proxy_hint(env(&[("HTTPS_PROXY", "x")])).unwrap()
            )
        );
    }

    #[test]
    fn rejection_message_without_a_proxy_is_just_the_error() {
        assert_eq!(
            rejection_message(&"refused", env(&[])),
            "expected a certificate rejection: \"refused\""
        );
    }

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
    fn finds_rustls_error_at_the_top() {
        let err = rustls::Error::General("x".into());
        assert!(matches!(
            find_rustls_error(&err),
            Some(rustls::Error::General(_))
        ));
    }

    /// Two plain `source()` hops, no `io::Error`: the ordinary walk.
    #[test]
    fn finds_rustls_error_down_the_source_chain() {
        #[derive(Debug)]
        struct Chain(Box<dyn std::error::Error + Send + Sync>);
        impl std::fmt::Display for Chain {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("chain")
            }
        }
        impl std::error::Error for Chain {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&*self.0)
            }
        }

        let err = Chain(Box::new(Chain(Box::new(rustls::Error::General(
            "x".into(),
        )))));
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

    /// The rejection tests count only a missing trust anchor, so the other
    /// ways a certificate fails must not satisfy them.
    #[test]
    fn only_unknown_issuer_counts_as_untrusted() {
        use rustls::CertificateError;

        assert!(is_unknown_issuer(&rustls::Error::InvalidCertificate(
            CertificateError::UnknownIssuer
        )));
        for other in [
            rustls::Error::InvalidCertificate(CertificateError::NotValidForName),
            rustls::Error::InvalidCertificate(CertificateError::Expired),
            rustls::Error::InvalidCertificate(CertificateError::BadSignature),
            rustls::Error::General("refused".into()),
        ] {
            assert!(!is_unknown_issuer(&other), "{other:?}");
        }
    }
}
