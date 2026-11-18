//! The `templates/` walk behind the embedded table. It lives outside `src/` so
//! it stays out of the library API. `build.rs` and `tests/walk.rs` both pull it
//! in with `#[path]`, so the test checks the refusals the build actually runs.
//! (`tests/embed.rs` keeps its own separate walk on purpose.)

use std::path::Path;

/// Recursively collect files under `dir`, as `/`-separated paths relative to
/// `root`. Directories are watched too, so an added or removed file reruns the
/// build script. A symlink is refused: the table must hold the committed bytes,
/// not whatever a link happens to point at on the build host. Any other
/// non-regular entry (FIFO, socket, device) is refused for the same reason.
pub fn collect_files(root: &Path, dir: &Path, out: &mut Vec<String>) {
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
