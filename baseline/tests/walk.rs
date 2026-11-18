//! The build script's `templates/` walk, tested on scratch trees. `walk.rs` is
//! the same file `build.rs` compiles, so deleting either refusal assert there
//! fails a test here.

#[path = "../walk.rs"]
mod walk;

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// A unique directory under `std::env::temp_dir()`, removed on drop. This
/// stands in for `tempfile`, which `repo-baseline` can't depend on: the crate
/// has no dependencies at all. The name is kept short so a socket path inside
/// it stays under the `sun_path` length limit.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock is after the epoch")
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("rb-walk-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        Self(dir)
    }

    fn file(&self, rel: &str) {
        let path = self.0.join(rel);
        std::fs::create_dir_all(path.parent().expect("rel has a parent"))
            .expect("create parent dirs");
        std::fs::write(&path, rel).expect("write scratch file");
    }

    fn collect(&self) -> Vec<String> {
        let mut out = Vec::new();
        walk::collect_files(&self.0, &self.0, &mut out);
        out.sort();
        out
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn path_of(s: &Scratch, rel: &str) -> PathBuf {
    s.0.join(rel)
}

#[test]
fn clean_tree_is_sorted_and_slash_joined() {
    let s = Scratch::new("clean");
    s.file("b.txt");
    s.file("a/nested/c.txt");
    s.file("a/d.txt");
    assert_eq!(s.collect(), ["a/d.txt", "a/nested/c.txt", "b.txt"]);
}

#[test]
fn empty_directories_contribute_nothing() {
    let s = Scratch::new("empty");
    std::fs::create_dir_all(path_of(&s, "x/y")).unwrap();
    s.file("z.txt");
    assert_eq!(s.collect(), ["z.txt"]);
}

#[cfg(unix)]
#[test]
#[should_panic(expected = "refusing symlink in templates")]
fn symlink_to_file_is_refused() {
    let s = Scratch::new("lnf");
    s.file("real.txt");
    std::os::unix::fs::symlink(path_of(&s, "real.txt"), path_of(&s, "link.txt")).unwrap();
    s.collect();
}

/// A link to a directory is refused rather than followed. This entry also sits
/// one level down, so the refusal is exercised inside the recursion.
#[cfg(unix)]
#[test]
#[should_panic(expected = "refusing symlink in templates")]
fn nested_symlink_to_dir_is_refused() {
    let s = Scratch::new("lnd");
    s.file("sub/real.txt");
    s.file("other/x.txt");
    std::os::unix::fs::symlink(path_of(&s, "other"), path_of(&s, "sub/dirlink")).unwrap();
    s.collect();
}

/// A socket stands in for any non-regular entry (FIFO, device). std can't make
/// a FIFO without `unsafe` (forbidden), a spawned `mkfifo` (banned by
/// clippy.toml), or a dependency (this crate has none). A socket reaches the
/// same `is_file()` assert: it is neither a symlink nor a directory.
#[cfg(unix)]
#[test]
#[should_panic(expected = "refusing non-regular file in templates")]
fn socket_is_refused() {
    let s = Scratch::new("sock");
    s.file("ok.txt");
    let _listener = std::os::unix::net::UnixListener::bind(path_of(&s, "s.sock")).unwrap();
    s.collect();
}

#[test]
fn paths_are_relative_to_root_not_dir() {
    let s = Scratch::new("rel");
    s.file("top/inner/f.txt");
    let mut out = Vec::new();
    let root: &Path = &s.0;
    walk::collect_files(root, &root.join("top"), &mut out);
    assert_eq!(out, ["top/inner/f.txt"]);
}
