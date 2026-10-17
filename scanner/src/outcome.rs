//! What a single tool scan or backfill produced.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use crate::pr;

#[derive(Debug)]
pub enum Outcome {
    /// Adapter ran successfully but no new versions were found. The
    /// activity block is *not* written back in this case — the next
    /// scheduled run will reattempt.
    NoChange,
    /// `--dry-run` printed the plan and exited without writing anything.
    DryRun { new_versions: Vec<String> },
    /// `--stage` validated the new files and recorded them for
    /// `scanner deliver`; the tree was restored.
    Staged { new_versions: Vec<String> },
    /// One PR was opened with new version files + a refreshed
    /// activity block.
    PrOpened {
        url: String,
        new_versions: Vec<String>,
    },
    /// An open PR was updated in place to cover the new versions.
    PrUpdated {
        url: String,
        new_versions: Vec<String>,
    },
    /// An open PR already covered every new version; nothing was opened.
    AlreadyCovered { pr: u64, new_versions: Vec<String> },
    /// Tool has no registered adapter — the orchestrator skipped it.
    NoAdapter,
}

impl Outcome {
    pub(crate) fn delivered(delivery: pr::Delivery, new_versions: Vec<String>) -> Self {
        match delivery {
            pr::Delivery::Opened(url) => Outcome::PrOpened { url, new_versions },
            pr::Delivery::Updated(url) => Outcome::PrUpdated { url, new_versions },
            pr::Delivery::Skipped(pr) => Outcome::AlreadyCovered { pr, new_versions },
        }
    }
}

/// The markdown bullet a run writes to the job's step summary for this
/// outcome, or `None` when there is no PR to point at. `AlreadyCovered`
/// is the line that surfaces a stalled scan PR (#401).
pub(crate) fn summary_line(tool_id: &str, outcome: &Outcome) -> Option<String> {
    match outcome {
        Outcome::PrOpened { url, .. } => Some(format!("- `{tool_id}`: opened {url}")),
        Outcome::PrUpdated { url, .. } => Some(format!("- `{tool_id}`: updated {url}")),
        Outcome::AlreadyCovered { pr, .. } => {
            Some(format!("- `{tool_id}`: already covered by #{pr}"))
        }
        Outcome::NoChange
        | Outcome::DryRun { .. }
        | Outcome::Staged { .. }
        | Outcome::NoAdapter => None,
    }
}

fn append_step_summary(path: &Path, line: &str) -> std::io::Result<()> {
    let mut file = OpenOptions::new().append(true).create(true).open(path)?;
    // One write, so the line can't interleave with other appenders.
    file.write_all(format!("{line}\n").as_bytes())
}

/// Append this outcome's [`summary_line`] to `$GITHUB_STEP_SUMMARY`.
/// A no-op outside Actions; a write failure only warns, because the
/// summary is a convenience and must never fail the scan.
pub(crate) fn report_step_summary(tool_id: &str, outcome: &Outcome) {
    let path = std::env::var_os("GITHUB_STEP_SUMMARY");
    if let Err(e) = report_to(path.as_deref().map(Path::new), tool_id, outcome) {
        eprintln!("scanner: {tool_id}: could not write step summary: {e}");
    }
}

/// [`report_step_summary`] with the summary path passed in, so tests
/// need not mutate the environment.
fn report_to(path: Option<&Path>, tool_id: &str, outcome: &Outcome) -> std::io::Result<()> {
    match (path, summary_line(tool_id, outcome)) {
        (Some(path), Some(line)) => append_step_summary(path, &line),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivered_maps_each_delivery_to_its_outcome() {
        let v = || vec!["1.0.0".to_string()];
        assert!(matches!(
            Outcome::delivered(pr::Delivery::Opened("u1".into()), v()),
            Outcome::PrOpened { url, new_versions } if url == "u1" && new_versions == v()
        ));
        assert!(matches!(
            Outcome::delivered(pr::Delivery::Updated("u2".into()), v()),
            Outcome::PrUpdated { url, new_versions } if url == "u2" && new_versions == v()
        ));
        assert!(matches!(
            Outcome::delivered(pr::Delivery::Skipped(398), v()),
            Outcome::AlreadyCovered { pr: 398, new_versions } if new_versions == v()
        ));
    }

    #[test]
    fn summary_line_names_the_pr_for_each_delivery() {
        let v = || vec!["1.0.0".to_string()];
        let opened = Outcome::PrOpened {
            url: "https://example.test/pull/7".into(),
            new_versions: v(),
        };
        let updated = Outcome::PrUpdated {
            url: "https://example.test/pull/8".into(),
            new_versions: v(),
        };
        let covered = Outcome::AlreadyCovered {
            pr: 398,
            new_versions: v(),
        };
        assert_eq!(
            summary_line("jq", &opened).as_deref(),
            Some("- `jq`: opened https://example.test/pull/7")
        );
        assert_eq!(
            summary_line("jq", &updated).as_deref(),
            Some("- `jq`: updated https://example.test/pull/8")
        );
        assert_eq!(
            summary_line("jq", &covered).as_deref(),
            Some("- `jq`: already covered by #398")
        );
    }

    #[test]
    fn summary_line_is_none_without_a_pr() {
        let dry = Outcome::DryRun {
            new_versions: vec!["1.0.0".into()],
        };
        assert_eq!(summary_line("jq", &Outcome::NoChange), None);
        assert_eq!(summary_line("jq", &dry), None);
        let staged = Outcome::Staged {
            new_versions: vec!["1.0.0".into()],
        };
        assert_eq!(summary_line("jq", &staged), None);
        assert_eq!(summary_line("jq", &Outcome::NoAdapter), None);
    }

    #[test]
    fn append_step_summary_creates_then_appends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("summary.md");
        append_step_summary(&path, "- first").unwrap();
        append_step_summary(&path, "- second").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "- first\n- second\n"
        );
    }

    #[test]
    fn append_step_summary_surfaces_io_errors() {
        let dir = tempfile::tempdir().unwrap();
        // A directory can't be opened for append.
        assert!(append_step_summary(dir.path(), "- x").is_err());
    }

    #[test]
    fn report_to_writes_only_with_a_path_and_a_pr() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("summary.md");
        let covered = Outcome::AlreadyCovered {
            pr: 398,
            new_versions: vec!["1.0.0".into()],
        };
        report_to(None, "jq", &covered).unwrap();
        report_to(Some(&path), "jq", &Outcome::NoChange).unwrap();
        assert!(!path.exists());
        report_to(Some(&path), "jq", &covered).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "- `jq`: already covered by #398\n"
        );
        assert!(report_to(Some(dir.path()), "jq", &covered).is_err());
    }
}
