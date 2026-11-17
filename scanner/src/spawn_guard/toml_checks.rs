//! The `clippy.toml` and manifest checks behind [`super::the_ban_and_lint_levels_are_in_place`].

use std::path::Path;

/// TOML text with CRLF normalised, whole-line comments dropped and each
/// remaining line trimmed, so a commented-out entry does not count and
/// spacing or line endings do not matter.
pub(super) fn live_toml(text: &str) -> String {
    text.replace("\r\n", "\n")
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every `path = "..."` (or `'...'`) value in `clippy.toml`'s live
/// `disallowed-methods` table, read key by key outside TOML strings and
/// `#` comments, so entry field order, a `]` or `path =` inside a reason,
/// and a commented-out entry after a live one do not matter.
pub(super) fn banned_paths(clippy_toml: &str) -> Vec<String> {
    let Some((_, rest)) = clippy_toml.split_once("disallowed-methods") else {
        return Vec::new();
    };
    let b = rest.as_bytes();
    let (mut i, mut depth, mut paths) = (0, 0usize, Vec::new());
    // Index of the closing quote. Basic `"..."` strings take `\` escapes;
    // literal `'...'` strings have none.
    let string_end = |mut i: usize, q: u8| {
        while i < b.len() && b[i] != q {
            i += if q == b'"' && b[i] == b'\\' { 2 } else { 1 };
        }
        i
    };
    while i < b.len() {
        match b[i] {
            b'[' => depth += 1,
            b']' => {
                // A `]` before any `[` means a malformed table: no ban.
                let Some(d) = depth.checked_sub(1) else { break };
                depth = d;
                if depth == 0 {
                    break;
                }
            }
            q @ (b'"' | b'\'') => i = string_end(i + 1, q),
            b'#' => i = rest[i..].find('\n').map_or(b.len(), |n| i + n),
            // A whole `path` key: not the tail of `reason_path` or
            // `x-path`, and not the head of `paths`.
            b'p' if depth > 0
                && b[i..].starts_with(b"path")
                && !matches!(
                    b[i - 1],
                    b'_' | b'-' | b'\'' | b'"' | b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z'
                )
                && !matches!(
                    b.get(i + 4),
                    Some(b'_' | b'-' | b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z')
                ) =>
            {
                let after = rest[i + 4..].trim_start();
                if let Some(value) = after.strip_prefix('=').map(str::trim_start)
                    && let Some(q) = value.chars().next().filter(|c| matches!(c, '"' | '\''))
                {
                    let body = &value[1..];
                    let end = body.find(q).unwrap_or(body.len());
                    paths.push(body[..end].to_string());
                }
            }
            _ => {}
        }
        i += 1;
    }
    paths
}

/// Whether a crate manifest inherits the workspace lints with no
/// crate-level `[lints.*]` override.
pub(super) fn inherits_workspace_lints(manifest: &str) -> bool {
    let live = live_toml(manifest);
    live.contains("[lints]\nworkspace = true") && !live.contains("[lints.")
}

/// The body of the root manifest's `[workspace.lints.<tool>]` table, or
/// `None` when the table is missing.
pub(super) fn workspace_lints(root: &str, tool: &str) -> Option<String> {
    let live = live_toml(root);
    let (_, rest) = live.split_once(&format!("[workspace.lints.{tool}]"))?;
    Some(rest.split("\n[").next().unwrap_or("").trim().to_string())
}

/// Every clippy config or cargo config under the crate directories that
/// would shadow the workspace `clippy.toml` (clippy reads the first one
/// it finds walking up from each crate, without merging) or could set
/// `CLIPPY_CONF_DIR`.
pub(super) fn shadowing_configs(workspace: &Path, crates: &[&str]) -> Vec<String> {
    let mut found = Vec::new();
    let mut stack: Vec<_> = crates.iter().map(|c| workspace.join(c)).collect();
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if path.is_dir() {
                if name == ".cargo" {
                    found.push(path.strip_prefix(workspace).unwrap().display().to_string());
                } else if name != "target" && name != ".git" {
                    stack.push(path);
                }
            } else if name == "clippy.toml" || name == ".clippy.toml" {
                found.push(path.strip_prefix(workspace).unwrap().display().to_string());
            }
        }
    }
    for root_cargo in [".cargo/config", ".cargo/config.toml"] {
        let path = workspace.join(root_cargo);
        if path.exists()
            && std::fs::read_to_string(&path)
                .unwrap()
                .contains("CLIPPY_CONF_DIR")
        {
            found.push(root_cargo.to_string());
        }
    }
    found.sort();
    found
}

/// Each manifest check rejects the overrides it exists to catch, so a
/// regression in its parsing shows up here, not only against the repo.
#[test]
fn manifest_checks_reject_overrides() {
    let ok = "[package]\nname = \"x\"\n\n[lints]\nworkspace = true\n";
    assert!(inherits_workspace_lints(ok));
    assert!(inherits_workspace_lints(&ok.replace('\n', "\r\n")));
    for bad in [
        "[package]\nname = \"x\"\n",
        "[lints]\n# workspace = true\n",
        "[lints]\nworkspace = false\n",
        "[lints]\nworkspace = true\n[lints.clippy]\ndisallowed_methods = \"allow\"\n",
        "[lints.clippy]\ndisallowed_methods = \"allow\"\n",
    ] {
        assert!(!inherits_workspace_lints(bad), "accepted {bad:?}");
    }

    let root = |clippy: &str| {
        format!(
            "[workspace.lints.rust]\nunsafe_code = \"forbid\"\n\n[workspace.lints.clippy]\n{clippy}\n[profile.release]\nlto = true\n"
        )
    };
    assert_eq!(
        workspace_lints(&root("all = \"warn\""), "clippy").as_deref(),
        Some("all = \"warn\"")
    );
    assert_eq!(
        workspace_lints(&root("all = \"warn\"\n# style = \"allow\""), "clippy").as_deref(),
        Some("all = \"warn\"")
    );
    for bad in [
        "all = \"warn\"\ndisallowed_methods = \"allow\"",
        "all = \"warn\"\nstyle = \"allow\"",
        "all = \"allow\"",
    ] {
        assert_ne!(
            workspace_lints(&root(bad), "clippy").as_deref(),
            Some("all = \"warn\""),
            "accepted {bad:?}"
        );
    }
    assert_eq!(
        workspace_lints("[workspace]\nmembers = []\n", "clippy"),
        None
    );

    // The rust table is pinned too: `warnings = "allow"` there silences
    // every lint, the ban included.
    let with_rust = |rust: &str| {
        format!("[workspace.lints.rust]\n{rust}\n\n[workspace.lints.clippy]\nall = \"warn\"\n")
    };
    assert_eq!(
        workspace_lints(&with_rust("unsafe_code = \"forbid\""), "rust").as_deref(),
        Some("unsafe_code = \"forbid\"")
    );
    assert_ne!(
        workspace_lints(
            &with_rust("unsafe_code = \"forbid\"\nwarnings = \"allow\""),
            "rust"
        )
        .as_deref(),
        Some("unsafe_code = \"forbid\"")
    );
}

/// A clippy config or cargo config under a crate is reported, wherever
/// it sits, and build output is not walked.
#[test]
fn shadowing_configs_are_found() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    for d in ["a/src", "b/target/debug", "c"] {
        std::fs::create_dir_all(ws.join(d)).unwrap();
    }
    assert_eq!(
        shadowing_configs(ws, &["a", "b", "c"]),
        Vec::<String>::new()
    );
    std::fs::write(ws.join("a/clippy.toml"), "").unwrap();
    std::fs::write(ws.join("a/src/.clippy.toml"), "").unwrap();
    std::fs::write(ws.join("b/target/debug/clippy.toml"), "").unwrap();
    std::fs::create_dir_all(ws.join("c/.cargo")).unwrap();
    std::fs::create_dir_all(ws.join(".cargo")).unwrap();
    std::fs::write(
        ws.join(".cargo/config.toml"),
        "[env]\nCLIPPY_CONF_DIR = \"x\"\n",
    )
    .unwrap();
    assert_eq!(
        shadowing_configs(ws, &["a", "b", "c"]),
        vec![
            ".cargo/config.toml",
            "a/clippy.toml",
            "a/src/.clippy.toml",
            "c/.cargo"
        ]
    );
}

/// The ban is found whatever the entry's field order, and text that only
/// mentions the path inside a reason string does not count as a ban.
#[test]
fn banned_paths_reads_keys_not_text() {
    let cmd = "std::process::Command::new".to_string();
    for live in [
        "disallowed-methods = [\n{ path = \"std::process::Command::new\", reason = \"x\" },\n]",
        "disallowed-methods = [\n{ reason = \"see #[expect] ] here\", path = \"std::process::Command::new\" },\n]",
        "disallowed-methods=[{path=\"a::b\"},{path=\"std::process::Command::new\"}]",
        "disallowed-methods = [\n{ path = 'std::process::Command::new' },\n]",
        "disallowed-methods = [ # see #588\n{ reason = 'a ] b', path = \"std::process::Command::new\" },\n]",
    ] {
        assert!(
            banned_paths(live).contains(&cmd),
            "missed the ban in {live:?}"
        );
    }
    for dead in [
        "disallowed-methods = [\n{ path = \"a::b\", reason = \"not path = \\\"std::process::Command::new\\\"\" },\n]",
        "disallowed-methods = []\nother = [{ path = \"std::process::Command::new\" }]",
        "disallowed-types = [{ path = \"std::process::Command::new\" }]",
        "disallowed-methods = [{ reason_path = \"std::process::Command::new\" }]",
        "disallowed-methods = [{ x-path = \"std::process::Command::new\" }]",
        "disallowed-methods = [{ paths = \"std::process::Command::new\" }]",
        "disallowed-methods = ]\n[{ path = \"std::process::Command::new\" }]",
        "disallowed-methods = [\n{ path = \"a::b\" }, # { path = \"std::process::Command::new\" },\n]",
        "disallowed-methods = [\n{ path = \"a::b\", reason = 'path = \"std::process::Command::new\"' },\n]",
        "disallowed-methods = [\n{ path = \"a::b\", reason = 'a ] b' },\n]\nx = [{ path = \"std::process::Command::new\" }]",
    ] {
        assert!(!banned_paths(dead).contains(&cmd), "false ban in {dead:?}");
    }
}
