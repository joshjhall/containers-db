//! Duplicate-PR guard for scan and backfill PRs.
//!
//! The scanner computes "new" versions against `main`, so versions sitting
//! in an open, unmerged scan PR look new again on every hourly run. Before
//! opening a PR, the orchestrator lists open PRs and asks [`decide`] what to
//! do. Everything here is pure so the decision is testable without `gh`.
//!
//! The versions an open PR covers are recovered from its body — the
//! `## New versions` bullet list that [`crate::pr::pr_body`] writes — not
//! from its title: a backfill title names only a range (`from to last`).

/// An open PR as listed by `gh pr list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenPr {
    pub number: u64,
    pub head: String,
    pub body: String,
}

/// What to do with a set of newly detected versions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// An open PR already covers every new version — open nothing.
    Skip { pr: u64, covered: Vec<String> },
    /// An open PR exists but misses some versions — regenerate onto its
    /// branch and update it in place rather than opening a second PR.
    UpdateExisting(OpenPr),
    /// No open PR for this tool — open one as before.
    OpenNew,
}

/// Versions listed under `## New versions` in a scanner PR body.
///
/// Reads `` - `<v>` `` bullets until the next `## ` heading. A body with no
/// such section yields an empty list.
pub fn parse_pr_versions(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_section = false;
    for line in body.lines() {
        let line = line.trim();
        if line.starts_with("## ") {
            in_section = line == "## New versions";
            continue;
        }
        if !in_section {
            continue;
        }
        if let Some(v) = line
            .strip_prefix("- `")
            .and_then(|rest| rest.strip_suffix('`'))
        {
            out.push(v.to_string());
        }
    }
    out
}

/// Decide between skip / update-in-place / open-new.
///
/// `prefix` scopes the match to one tool and PR kind (`scan/<tool>/` or
/// `backfill/<tool>/`); the trailing slash keeps `scan/rust/` from matching
/// `scan/rust-dev/…`. When several open PRs match, the oldest (lowest
/// number) is the one kept.
pub fn decide(prefix: &str, open_prs: &[OpenPr], new_versions: &[String]) -> Decision {
    let Some(pr) = open_prs
        .iter()
        .filter(|pr| pr.head.starts_with(prefix))
        .min_by_key(|pr| pr.number)
    else {
        return Decision::OpenNew;
    };
    let covered = parse_pr_versions(&pr.body);
    if new_versions.iter().all(|v| covered.contains(v)) {
        Decision::Skip {
            pr: pr.number,
            covered: new_versions.to_vec(),
        }
    } else {
        Decision::UpdateExisting(pr.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activity::{ActivitySignals, Score};

    fn body(versions: &[&str]) -> String {
        let versions: Vec<String> = versions.iter().map(|s| s.to_string()).collect();
        crate::pr::pr_body(
            "rust",
            &versions,
            None,
            Score::Active,
            &ActivitySignals::default(),
            7,
        )
    }

    fn pr(number: u64, head: &str, versions: &[&str]) -> OpenPr {
        OpenPr {
            number,
            head: head.to_string(),
            body: body(versions),
        }
    }

    fn vs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_round_trips_pr_body() {
        assert_eq!(
            parse_pr_versions(&body(&["1.99.0", "1.99.1"])),
            vs(&["1.99.0", "1.99.1"])
        );
    }

    #[test]
    fn parse_stops_at_next_heading() {
        let b = "## New versions\n\n- `1.0.0`\n\n## Activity (recomputed)\n\n- `score`: `active`\n";
        assert_eq!(parse_pr_versions(b), vs(&["1.0.0"]));
    }

    #[test]
    fn parse_without_section_is_empty() {
        assert!(parse_pr_versions("## Summary\n\n- `1.0.0`\n").is_empty());
        assert!(parse_pr_versions("").is_empty());
    }

    #[test]
    fn skip_when_open_pr_covers_every_version() {
        let prs = [pr(398, "scan/rust/2026-10-16", &["1.99.1"])];
        assert_eq!(
            decide("scan/rust/", &prs, &vs(&["1.99.1"])),
            Decision::Skip {
                pr: 398,
                covered: vs(&["1.99.1"])
            }
        );
    }

    #[test]
    fn update_when_open_pr_covers_some_versions() {
        let prs = [pr(398, "scan/rust/2026-10-16", &["1.99.1"])];
        assert_eq!(
            decide("scan/rust/", &prs, &vs(&["1.99.1", "1.100.0"])),
            Decision::UpdateExisting(prs[0].clone())
        );
    }

    #[test]
    fn update_when_open_pr_covers_no_versions() {
        let prs = [pr(398, "scan/rust/2026-10-16", &["1.99.1"])];
        assert_eq!(
            decide("scan/rust/", &prs, &vs(&["1.100.0"])),
            Decision::UpdateExisting(prs[0].clone())
        );
    }

    #[test]
    fn open_new_without_matching_pr() {
        assert_eq!(
            decide("scan/rust/", &[], &vs(&["1.99.1"])),
            Decision::OpenNew
        );
        let others = [
            pr(10, "scan/node/2026-10-16", &["1.99.1"]),
            pr(11, "backfill/rust/1.82.0", &["1.99.1"]),
            pr(12, "feature/issue-400", &["1.99.1"]),
        ];
        assert_eq!(
            decide("scan/rust/", &others, &vs(&["1.99.1"])),
            Decision::OpenNew
        );
    }

    #[test]
    fn prefix_does_not_match_a_longer_tool_id() {
        let prs = [pr(20, "scan/rust-dev/2026-10-16", &["1.99.1"])];
        assert_eq!(
            decide("scan/rust/", &prs, &vs(&["1.99.1"])),
            Decision::OpenNew
        );
    }

    #[test]
    fn oldest_matching_pr_is_kept() {
        let prs = [
            pr(402, "scan/rust/2026-10-17", &["1.99.1"]),
            pr(398, "scan/rust/2026-10-16", &[]),
        ];
        assert_eq!(
            decide("scan/rust/", &prs, &vs(&["1.99.1"])),
            Decision::UpdateExisting(prs[1].clone())
        );
    }
}
