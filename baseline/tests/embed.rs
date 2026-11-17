//! The embedded table is exactly the `templates/` tree on disk, and holds every
//! file `manifest.tsv` renders from.

use std::path::Path;

/// Every regular file under `dir`, `/`-separated and relative to `root`. An
/// independent walk (no shared code with `build.rs`), so a build-script bug
/// that drops or invents an entry shows up as a mismatch. A symlink fails the
/// test here too, so a committed link is caught by `cargo test`, not only by
/// the build script's refusal.
fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        let kind = entry.file_type().unwrap();
        assert!(
            !kind.is_symlink(),
            "symlink in templates: {}",
            path.display()
        );
        if kind.is_dir() {
            walk(root, &path, out);
        } else {
            let rel = path.strip_prefix(root).unwrap().to_string_lossy();
            out.push(rel.replace('\\', "/"));
        }
    }
}

fn embedded_paths() -> Vec<&'static str> {
    repo_baseline::FILES.iter().map(|(p, _)| *p).collect()
}

#[test]
fn files_match_the_tree_on_disk() {
    let root = Path::new(repo_baseline::TEMPLATES_DIR);
    let mut disk = Vec::new();
    walk(root, root, &mut disk);
    disk.sort();
    assert_eq!(
        embedded_paths(),
        disk,
        "embedded set differs from templates/ on disk"
    );
}

#[test]
fn bytes_match_disk() {
    let root = Path::new(repo_baseline::TEMPLATES_DIR);
    for (path, bytes) in repo_baseline::FILES {
        let disk = std::fs::read(root.join(path)).unwrap_or_else(|e| panic!("{path}: {e}"));
        assert_eq!(*bytes, disk.as_slice(), "{path} differs from disk");
    }
}

#[test]
fn table_is_strictly_sorted() {
    let paths = embedded_paths();
    assert!(
        paths.windows(2).all(|w| w[0] < w[1]),
        "FILES not strictly sorted: {paths:?}"
    );
}

/// Every non-`link` manifest row's `source` is embedded, so a consumer that
/// renders from [`repo_baseline::FILES`] never misses a template.
#[test]
fn every_manifest_source_is_embedded() {
    let manifest =
        std::str::from_utf8(repo_baseline::get("manifest.tsv").expect("manifest.tsv embedded"))
            .expect("manifest.tsv is UTF-8");
    let mut rows = 0;
    for line in manifest
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
    {
        let cols: Vec<&str> = line.split('\t').collect();
        assert!(cols.len() >= 4, "malformed manifest row: {line:?}");
        if cols.get(4) == Some(&"link") {
            continue;
        }
        rows += 1;
        assert!(
            repo_baseline::get(cols[3]).is_some(),
            "manifest source not embedded: {}",
            cols[3]
        );
    }
    assert!(rows > 0, "manifest.tsv has no template rows");
}

/// `TEMPLATES_DIR` is a tree `render.sh` can run from: the script is there and
/// executable, next to the manifest it reads.
#[cfg(unix)]
#[test]
fn templates_dir_holds_a_runnable_render_sh() {
    use std::os::unix::fs::PermissionsExt as _;
    let root = Path::new(repo_baseline::TEMPLATES_DIR);
    assert!(root.join("manifest.tsv").is_file(), "manifest.tsv missing");
    let mode = std::fs::metadata(root.join("render.sh"))
        .expect("render.sh present in TEMPLATES_DIR")
        .permissions()
        .mode();
    assert_eq!(mode & 0o111, 0o111, "render.sh not executable: {mode:o}");
}

/// The crate stays dependency-free (issue #631): consumers pin it by git rev
/// and must not inherit a dependency tree with it.
#[test]
fn crate_has_no_dependencies() {
    let manifest = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
        .expect("read baseline/Cargo.toml");
    for line in manifest.lines().map(str::trim) {
        assert!(
            !(line.starts_with('[') && line.contains("dependencies")),
            "repo-baseline must have no dependencies, found {line}"
        );
    }
}

#[test]
fn get_finds_entries_and_rejects_unknown() {
    let manifest = repo_baseline::get("manifest.tsv").expect("manifest.tsv embedded");
    assert!(manifest.starts_with(b"# repo-baseline scaffold manifest"));
    assert!(repo_baseline::get("render.sh").is_some());
    assert!(repo_baseline::get("core/scripts/run-head.sh.tmpl").is_some());
    assert_eq!(repo_baseline::get("no/such/file"), None);
    assert_eq!(repo_baseline::get(""), None);
    assert_eq!(
        repo_baseline::get("core"),
        None,
        "a directory is not an entry"
    );
}
