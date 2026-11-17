//! Generate `$OUT_DIR/templates.rs`: a `(relative path, bytes)` table of every
//! file under `templates/`, so a consumer that pins this crate by git rev
//! carries the templates inside its own binary and needs no checkout at run
//! time. Paths use `/` separators and are sorted, so the table is identical
//! across platforms and builds.

use std::fmt::Write as _;
use std::path::Path;

fn main() {
    let manifest_dir =
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is always set by cargo");
    let root = Path::new(&manifest_dir).join("templates");
    println!("cargo:rerun-if-changed={}", root.display());

    let mut files = Vec::new();
    collect_files(&root, &root, &mut files);
    files.sort();

    let mut table = String::from("&[\n");
    for rel in &files {
        let abs = root.join(rel);
        println!("cargo:rerun-if-changed={}", abs.display());
        let _ = writeln!(
            table,
            "    ({rel:?}, include_bytes!({:?}) as &[u8]),",
            abs.to_string_lossy()
        );
    }
    table.push(']');

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR is always set by cargo");
    std::fs::write(Path::new(&out_dir).join("templates.rs"), table)
        .expect("failed to write templates.rs");
}

/// Recursively collect files under `dir`, as `/`-separated paths relative to
/// `root`. Directories are watched too, so an added or removed file reruns the
/// build script. A symlink is refused: the table must hold the committed bytes,
/// not whatever a link happens to point at on the build host.
fn collect_files(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let entries =
        std::fs::read_dir(dir).unwrap_or_else(|e| panic!("failed to read {}: {e}", dir.display()));
    for entry in entries {
        let entry = entry.expect("failed to read directory entry");
        let path = entry.path();
        let kind = entry.file_type().expect("failed to stat directory entry");
        assert!(
            !kind.is_symlink(),
            "refusing symlink in templates: {}",
            path.display()
        );
        if kind.is_dir() {
            println!("cargo:rerun-if-changed={}", path.display());
            collect_files(root, &path, out);
        } else {
            assert!(
                kind.is_file(),
                "refusing non-regular file in templates: {}",
                path.display()
            );
            let rel = path.strip_prefix(root).expect("entry is under root");
            let parts: Vec<String> = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            out.push(parts.join("/"));
        }
    }
}
