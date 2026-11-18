//! Generate `$OUT_DIR/templates.rs`: a `(relative path, bytes)` table of every
//! file under `templates/`, so a consumer that pins this crate by git rev
//! carries the templates inside its own binary and needs no checkout at run
//! time. Paths use `/` separators and are sorted, so the table is identical
//! across platforms and builds.

use std::fmt::Write as _;
use std::path::Path;

#[path = "walk.rs"]
mod walk;

fn main() {
    let manifest_dir =
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is always set by cargo");
    let root = Path::new(&manifest_dir).join("templates");
    println!("cargo:rerun-if-changed={}", root.display());

    let mut files = Vec::new();
    walk::collect_files(&root, &root, &mut files);
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
