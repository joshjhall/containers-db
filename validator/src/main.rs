//! `validate-catalog` — semantic validator for the containers-db tool catalog.
//!
//! Loads the catalog, walks every version-shaped JSON file, and applies
//! the rules defined in `db_validator::rules`. Exits 0 on success, 1
//! on any rule violation, 2 on infrastructure failure (catalog walk
//! error, IO error).
//!
//! Modes:
//!   - default: full-catalog walk
//!   - `--only PATH`: validate a single file (used by the negative
//!     fixture loop in CI)

use std::path::PathBuf;
use std::process::ExitCode;

use db_validator::{catalog, rules};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let only = parse_only_flag(&args);

    let repo_root = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("validate-catalog: failed to read current directory: {e}");
            return ExitCode::from(2);
        }
    };

    let catalog = match catalog::load(&repo_root) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("validate-catalog: catalog load failed: {e}");
            return ExitCode::from(2);
        }
    };

    let only_path = only.map(|s| {
        // CI invokes `--only fixtures/_negative/foo.json` from the repo root.
        // Resolve relative paths against repo_root so the negative loop
        // can use either form.
        let p = PathBuf::from(s);
        if p.is_absolute() {
            p
        } else {
            repo_root.join(p)
        }
    });

    let diagnostics = match rules::check_all(&repo_root, &catalog, only_path.as_deref()) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("validate-catalog: rule check failed: {e}");
            return ExitCode::from(2);
        }
    };

    if diagnostics.is_empty() {
        println!(
            "catalog OK: validated against {} catalog entries",
            catalog.len()
        );
        return ExitCode::SUCCESS;
    }

    for d in &diagnostics {
        eprintln!("validate-catalog: {d}");
    }
    eprintln!("validate-catalog FAILED: {} violation(s)", diagnostics.len());
    ExitCode::from(1)
}

fn parse_only_flag(args: &[String]) -> Option<&str> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--only" {
            return iter.next().map(|s| s.as_str());
        }
        if let Some(rest) = arg.strip_prefix("--only=") {
            return Some(rest);
        }
    }
    None
}
