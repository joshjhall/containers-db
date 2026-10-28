//! The hermetic TLS-roots tests (issues #507, #510, #512). On Linux each one
//! runs its assertions in a child of the test binary, through
//! [`run_with_env`](crate::tests::run_with_env), whose environment holds only
//! the roots the test means to give it.

use clap::Parser;

use crate::adapters;
use crate::tests::{in_child, run_with_env};
use crate::{Cli, run};

/// The production constructors build against the OS store, and the
/// registry holds every shipped adapter. On Linux this runs in a child
/// whose only root is a committed fixture, so it needs no host CA bundle
/// (issue #510) and is the "builds with roots" contrast for
/// [`missing_tls_roots_are_an_error_not_a_panic`].
///
/// Elsewhere it runs in-process, by decision (issue #512): on macOS and
/// Windows the platform verifier loads no roots in `build()`, so nothing
/// in the environment can change whether it builds, and an env-cleared
/// child would only add a way to fail (Windows needs `SystemRoot` and the
/// like to start a process at all).
#[test]
fn system_roots_client_builds() {
    if cfg!(target_os = "linux") {
        let fixture =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/test-root.pem");
        run_with_env(
            "test_tls::roots::system_roots_child",
            tempfile::tempdir().unwrap().path(),
            &[("SSL_CERT_FILE", &fixture)],
        );
    } else {
        assert_system_roots_clients_build();
    }
}

#[test]
#[ignore = "run by system_roots_client_builds"]
fn system_roots_child() {
    if in_child() {
        assert_system_roots_clients_build();
    }
}

fn assert_system_roots_clients_build() {
    use adapters::{DEFAULT_REQUEST_TIMEOUT, TlsRoots};

    adapters::http_client(DEFAULT_REQUEST_TIMEOUT, TlsRoots::System).unwrap();

    // `scanner due` silently skips any tool missing from this map, so an
    // accidentally dropped registration would only show up as a tool
    // that quietly stops getting new versions.
    let registry = adapters::register_all().unwrap();
    let mut ids: Vec<&str> = registry.keys().map(String::as_str).collect();
    ids.sort_unstable();
    assert_eq!(ids, vec!["go", "node", "rust"]);

    // Without this, `new()` could drop `DEFAULT_REQUEST_TIMEOUT` (the #467
    // hang) or the OS store with every other test green.
    let go = adapters::go::GoAdapter::new().unwrap();
    let node = adapters::node::NodeAdapter::new().unwrap();
    let rust = adapters::rust::RustAdapter::new().unwrap();
    for (name, timeout, roots) in [
        ("go", go.timeout, go.roots),
        ("node", node.timeout, node.roots),
        ("rust", rust.timeout, rust.roots),
    ] {
        assert_eq!(timeout, DEFAULT_REQUEST_TIMEOUT, "{name}");
        assert_eq!(roots, TlsRoots::System, "{name}");
    }
}

/// With no TLS roots to load, building the HTTP clients fails, and that
/// must reach the caller as an error rather than a panic (issue #507).
/// `SSL_CERT_FILE` naming an empty file stands in for a host without
/// `ca-certificates`: it replaces the system store with zero roots.
///
/// Linux only, by decision (issue #510): on macOS and Windows the
/// platform verifier defers to the OS at handshake time and loads no roots
/// in `build()`, so there is no build-time failure to provoke, and a
/// synthetic `reqwest::Error` cannot be constructed outside reqwest to
/// test the mapping alone. The roots-present half,
/// [`system_roots_client_builds`], runs everywhere.
#[test]
#[cfg(target_os = "linux")]
fn missing_tls_roots_are_an_error_not_a_panic() {
    let dir = tempfile::tempdir().unwrap();
    let empty = dir.path().join("empty.pem");
    std::fs::write(&empty, "").unwrap();
    run_with_env(
        "test_tls::roots::no_roots_client_build_fails_cleanly",
        dir.path(),
        &[("SSL_CERT_FILE", &empty)],
    );
}

#[test]
#[ignore = "run by missing_tls_roots_are_an_error_not_a_panic"]
fn no_roots_client_build_fails_cleanly() {
    if !in_child() {
        return;
    }
    let hint = "install ca-certificates or set SSL_CERT_FILE/SSL_CERT_DIR";

    use adapters::{DEFAULT_REQUEST_TIMEOUT, TlsRoots};

    // The hint is a fixed context string, so also require what sits under
    // it: reqwest's builder error, carrying the verifier's own cause as a
    // `rustls::Error` (issue #512: a type, where any source used to do).
    // Neither the variant nor the wording is pinned; a dependency bump
    // may change both. What makes the cause specific to missing roots is
    // the contrast: the same build succeeds in `system_roots_child`, whose
    // environment differs only by having a root.
    let err = adapters::http_client(DEFAULT_REQUEST_TIMEOUT, TlsRoots::System).unwrap_err();
    assert!(format!("{err}").contains(hint), "{err:#}");
    let Some(reqwest_err) = err.downcast_ref::<reqwest::Error>() else {
        panic!("no reqwest::Error under the context: {err:#}");
    };
    assert!(reqwest_err.is_builder(), "{err:#}");
    assert!(
        super::find_rustls_error(reqwest_err).is_some(),
        "no rustls::Error under the builder error: {err:#}"
    );

    // The wiremock constructors' client does not touch the OS store, so
    // the tests built on it run here too.
    adapters::http_client(DEFAULT_REQUEST_TIMEOUT, TlsRoots::None).unwrap();
    adapters::go::GoAdapter::with_base("http://x").unwrap();
    adapters::node::NodeAdapter::with_base("http://x").unwrap();
    adapters::rust::RustAdapter::with_api_base("http://x").unwrap();

    // `register_all` stops at the first adapter, so check each one's
    // constructor on its own.
    for (name, built) in [
        ("go", adapters::go::GoAdapter::new().map(drop)),
        ("node", adapters::node::NodeAdapter::new().map(drop)),
        ("rust", adapters::rust::RustAdapter::new().map(drop)),
    ] {
        let err = built.unwrap_err();
        assert!(format!("{err}").contains(hint), "{name}: {err:#}");
    }

    let Err(err) = adapters::register_all() else {
        panic!("register_all built clients with no TLS roots");
    };
    assert!(format!("{err}").contains(hint), "{err:#}");

    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    // Each scanning command builds the registry first, before any repo I/O.
    for argv in [
        &["scanner", "due", "--dry-run"][..],
        &["scanner", "tool", "rust", "--dry-run"],
        &[
            "scanner",
            "backfill",
            "rust",
            "--from",
            "1.0.0",
            "--dry-run",
        ],
    ] {
        let cli = Cli::try_parse_from(argv).unwrap();
        let err = rt.block_on(run(cli)).unwrap_err();
        assert!(format!("{err}").contains(hint), "{argv:?}: {err:#}");
    }

    // `deliver` makes no requests, so it gets past the registry and fails
    // on the missing plan instead.
    let dir = tempfile::tempdir().unwrap();
    let plan = dir.path().join("missing-plan.json");
    let deliver =
        Cli::try_parse_from(["scanner", "deliver", "--plan", plan.to_str().unwrap()]).unwrap();
    let err = format!("{:#}", rt.block_on(run(deliver)).unwrap_err());
    assert!(
        err.starts_with(&format!("reading plan {}", plan.display())),
        "{err}"
    );
    assert!(!err.contains(hint), "{err}");
}

/// Set in a [`none_roots_ignore_the_system_store`] child: the directory
/// holding the [`super::TestPki`] the parent wrote.
#[cfg(target_os = "linux")]
const PKI_DIR: &str = "SCANNER_TEST_TLS_PKI_DIR";

/// [`TlsRoots::None`](adapters::TlsRoots::None) must not fall back to the
/// OS store (issue #512). The child's only system root is a throwaway CA
/// whose leaf it serves, so a `System` client reaches the server and a
/// `None` client that trusted the store would too.
///
/// Linux only, like [`missing_tls_roots_are_an_error_not_a_panic`]:
/// `SSL_CERT_FILE` is how the platform verifier is given a root there,
/// and elsewhere it asks the OS, which a test cannot seed hermetically.
/// `adapters::tests::none_roots_reject_https` covers every OS for a
/// `None` that trusts anything at all.
#[test]
#[cfg(target_os = "linux")]
fn none_roots_ignore_the_system_store() {
    let dir = tempfile::tempdir().unwrap();
    super::TestPki::generate().write_to(dir.path());
    run_with_env(
        "test_tls::roots::none_roots_ignore_the_system_store_child",
        tempfile::tempdir().unwrap().path(),
        &[
            ("SSL_CERT_FILE", &dir.path().join("ca.pem")),
            (PKI_DIR, dir.path()),
        ],
    );
}

#[test]
#[cfg(target_os = "linux")]
#[ignore = "run by none_roots_ignore_the_system_store"]
fn none_roots_ignore_the_system_store_child() {
    if !in_child() {
        return;
    }
    use adapters::{DEFAULT_REQUEST_TIMEOUT, TlsRoots};

    let dir = std::env::var_os(PKI_DIR).unwrap();
    let pki = super::TestPki::read_from(std::path::Path::new(&dir));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let url = pki.serve().await;
        let system = adapters::http_client(DEFAULT_REQUEST_TIMEOUT, TlsRoots::System).unwrap();
        super::assert_reachable(&system, &url).await;
        let none = adapters::http_client(DEFAULT_REQUEST_TIMEOUT, TlsRoots::None).unwrap();
        super::assert_cert_rejected(&none, &url).await;
    });
}
