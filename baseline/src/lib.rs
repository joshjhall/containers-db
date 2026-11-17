//! The repo-baseline templates (`baseline/templates/`), embedded at build time.
//!
//! Consumers such as joshjhall/containers' `stibbons setup standards` depend on
//! this crate by `git = …, rev = <full commit sha>`: the rev is the snapshot pin,
//! and the templates compile into the consumer's binary, so it renders offline
//! with no containers-db checkout.

/// Every file under `templates/`, as `(templates-relative path, bytes)`.
///
/// Paths use `/` separators and the table is sorted by path. The set is exactly
/// the committed tree: `manifest.tsv`, `render.sh`, `README.md`, and the
/// per-slice template files.
pub static FILES: &[(&str, &[u8])] = include!(concat!(env!("OUT_DIR"), "/templates.rs"));

/// Absolute path of the `templates/` directory this crate was built from.
///
/// For tests in a consumer that need the files on disk, e.g. to run the
/// reference `render.sh` against the same snapshot that [`FILES`] embeds. It is
/// a build-host path: do not read it from a shipped binary.
pub const TEMPLATES_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/templates");

/// The bytes of `path` (templates-relative, `/`-separated), if embedded.
#[must_use]
pub fn get(path: &str) -> Option<&'static [u8]> {
    FILES
        .binary_search_by(|(p, _)| (*p).cmp(path))
        .ok()
        .map(|i| FILES[i].1)
}
