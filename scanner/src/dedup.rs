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
    /// Head commit when listed — the lease a regen must still find (#526).
    pub head_oid: String,
    pub body: String,
    /// Head branch lives in a fork. Never trusted: anyone can open one.
    pub cross_repo: bool,
    /// Every commit on the PR is GitHub-verified, so it can merge under
    /// `main`'s required-signatures ruleset. Not just the head: merge and
    /// rebase merges carry every commit, so a signed fixup on top of an
    /// unsigned commit still cannot land. `false` when unknown: such a
    /// covering PR is regenerated rather than skipped (#529), and a
    /// redundant regen costs one signed commit where a wrong skip strands
    /// the release.
    pub commits_verified: bool,
}

/// `--json` fields [`parse_open_prs`] expects from `gh pr list`.
pub const GH_PR_FIELDS: &str = "number,headRefName,headRefOid,body,isCrossRepository";

/// Parse `gh pr list --json` output into [`OpenPr`]s.
///
/// Errors on malformed output rather than dropping rows: a dropped row
/// would fail open into a duplicate PR. A missing `isCrossRepository`
/// reads as cross-repo, so [`decide`] ignores the row; a missing
/// `commitsVerified` reads as unverified.
pub fn parse_open_prs(json: &str) -> Result<Vec<OpenPr>, String> {
    let rows: Vec<serde_json::Value> =
        serde_json::from_str(json).map_err(|e| format!("parsing `gh pr list` output: {e}"))?;
    rows.iter()
        .map(|r| {
            let missing = |k: &str| format!("`gh pr list` row missing `{k}`: {r}");
            let text = |k: &str| {
                r.get(k)
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
                    .ok_or_else(|| missing(k))
            };
            Ok(OpenPr {
                number: r
                    .get("number")
                    .and_then(serde_json::Value::as_u64)
                    .ok_or_else(|| missing("number"))?,
                head: text("headRefName")?,
                head_oid: text("headRefOid")?,
                body: text("body").unwrap_or_default(),
                cross_repo: r
                    .get("isCrossRepository")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(true),
                // `gh pr list` has no verification field; [`crate::pr`]
                // looks it up for the one candidate PR. Absent reads as
                // unverified so it fails toward a regen, never a skip.
                commits_verified: r
                    .get("commitsVerified")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
            })
        })
        .collect()
}

/// Refuse a listing that filled `limit`: it may be truncated, and a
/// missing row would fail open into a duplicate PR.
pub fn check_not_truncated(len: usize, limit: usize) -> Result<(), String> {
    if len >= limit {
        return Err(format!(
            "`gh pr list` hit its {limit}-PR limit; refusing to dedup on a truncated list"
        ));
    }
    Ok(())
}

/// What to do with a set of newly detected versions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// An open PR already covers every new version — open nothing.
    Skip { pr: u64, covered: Vec<String> },
    /// An open PR exists but cannot stand as-is — regenerate onto its
    /// branch and update it in place rather than opening a second PR.
    UpdateExisting { pr: OpenPr, reason: RegenReason },
    /// No open PR for this tool — open one as before.
    OpenNew,
}

/// Why an open PR is regenerated rather than skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegenReason {
    /// The PR misses some of the new versions.
    MissingVersions,
    /// The PR covers every version, but not every commit on it is
    /// verified, so it can never merge under required signatures (#529).
    /// Like any regen, this replaces the branch: commits pushed to it by
    /// hand are discarded, which is intended — unsigned, they could never
    /// merge anyway.
    UnsignedCommits,
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
/// `scan/rust-dev/…`. Fork PRs are ignored — their branch name and body are
/// attacker-controlled, and trusting them would let anyone suppress a scan.
/// When several open PRs match, the oldest (lowest number) is the one kept.
///
/// `new_versions` must be non-empty — an empty list would be vacuously
/// covered by any open PR. [`crate::pr::deliver`] enforces this.
///
/// A covering PR is skipped only when every commit on it is verified; one
/// with an unsigned commit can never merge, so it is regenerated through
/// the signed path.
pub fn decide(prefix: &str, open_prs: &[OpenPr], new_versions: &[String]) -> Decision {
    let Some(pr) = candidate(prefix, open_prs) else {
        return Decision::OpenNew;
    };
    let covered = parse_pr_versions(&pr.body);
    let reason = if !new_versions.iter().all(|v| covered.contains(v)) {
        RegenReason::MissingVersions
    } else if !pr.commits_verified {
        RegenReason::UnsignedCommits
    } else {
        return Decision::Skip {
            pr: pr.number,
            covered: new_versions.to_vec(),
        };
    };
    Decision::UpdateExisting {
        pr: pr.clone(),
        reason,
    }
}

/// The open PR [`decide`] acts on: the oldest non-fork PR whose head
/// branch starts with `prefix`. Exposed so the caller can look up the
/// commit signatures for this one PR only.
pub fn candidate<'a>(prefix: &str, open_prs: &'a [OpenPr]) -> Option<&'a OpenPr> {
    open_prs
        .iter()
        .filter(|pr| !pr.cross_repo && pr.head.starts_with(prefix))
        .min_by_key(|pr| pr.number)
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
            head_oid: "abc123".to_string(),
            body: body(versions),
            cross_repo: false,
            commits_verified: true,
        }
    }

    fn missing(pr: &OpenPr) -> Decision {
        Decision::UpdateExisting {
            pr: pr.clone(),
            reason: RegenReason::MissingVersions,
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

    /// Two independent guards cover `\r`: `str::lines` strips `\r\n` and
    /// the per-line `trim` strips a stray `\r`, so this fails only if both go.
    #[test]
    fn parse_accepts_crlf_line_endings() {
        let b = "## New versions\r\n\r\n- `1.0.0`\r\n- `1.0.1`\r\n\r\n## Activity\r\n- `2.0.0`\r\n";
        assert_eq!(parse_pr_versions(b), vs(&["1.0.0", "1.0.1"]));
    }

    #[test]
    fn parse_accepts_indented_bullets_and_ignores_malformed_ones() {
        let b = "## New versions\n\
                 \x20  - `1.0.0`\n\
                 * `2.0.0`\n\
                 - 3.0.0\n\
                 - `4.0.0` extra\n\
                 -`5.0.0`\n\
                 - `6.0.0\n";
        assert_eq!(parse_pr_versions(b), vs(&["1.0.0"]));
    }

    #[test]
    fn parse_unions_duplicate_sections_and_skips_between() {
        let b = "## New versions\n- `1.0.0`\n## Activity\n- `9.9.9`\n## New versions\n- `1.0.1`\n";
        assert_eq!(parse_pr_versions(b), vs(&["1.0.0", "1.0.1"]));
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

    /// #519: a covering PR with an unsigned commit can never merge, so it is
    /// regenerated through the signed path instead of skipped forever.
    #[test]
    fn regen_when_covering_pr_has_unsigned_commits() {
        let mut unsigned = pr(519, "scan/node/2026-10-28", &["24.22.2", "26.13.0"]);
        unsigned.commits_verified = false;
        assert_eq!(
            decide(
                "scan/node/",
                &[unsigned.clone()],
                &vs(&["24.22.2", "26.13.0"])
            ),
            Decision::UpdateExisting {
                pr: unsigned,
                reason: RegenReason::UnsignedCommits,
            }
        );
    }

    /// Missing versions outrank unsigned commits: the regen reason names
    /// what the PR lacks, not only that it cannot merge.
    #[test]
    fn missing_versions_reason_wins_over_unsigned_commits() {
        let mut unsigned = pr(398, "scan/rust/2026-10-16", &["1.99.1"]);
        unsigned.commits_verified = false;
        assert_eq!(
            decide("scan/rust/", &[unsigned.clone()], &vs(&["1.100.0"])),
            missing(&unsigned)
        );
    }

    #[test]
    fn update_when_open_pr_covers_some_versions() {
        let prs = [pr(398, "scan/rust/2026-10-16", &["1.99.1"])];
        assert_eq!(
            decide("scan/rust/", &prs, &vs(&["1.99.1", "1.100.0"])),
            missing(&prs[0])
        );
    }

    #[test]
    fn update_when_open_pr_covers_no_versions() {
        let prs = [pr(398, "scan/rust/2026-10-16", &["1.99.1"])];
        assert_eq!(
            decide("scan/rust/", &prs, &vs(&["1.100.0"])),
            missing(&prs[0])
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
            missing(&prs[1])
        );
    }

    #[test]
    fn fork_pr_is_never_trusted() {
        let mut fork = pr(30, "scan/rust/2026-10-16", &["1.99.1"]);
        fork.cross_repo = true;
        assert_eq!(
            decide("scan/rust/", &[fork], &vs(&["1.99.1"])),
            Decision::OpenNew
        );
    }

    #[test]
    fn older_fork_pr_does_not_shadow_the_real_one() {
        let mut fork = pr(5, "scan/rust/evil", &["1.99.1"]);
        fork.cross_repo = true;
        let real = pr(9, "scan/rust/2026-10-16", &[]);
        assert_eq!(
            decide("scan/rust/", &[fork, real.clone()], &vs(&["1.99.1"])),
            missing(&real)
        );
    }

    #[test]
    fn candidate_is_the_oldest_non_fork_prefix_match() {
        let mut fork = pr(5, "scan/rust/evil", &[]);
        fork.cross_repo = true;
        let prs = [
            fork,
            pr(402, "scan/rust/2026-10-17", &[]),
            pr(398, "scan/rust/2026-10-16", &[]),
            pr(300, "scan/rust-dev/2026-10-15", &[]),
        ];
        assert_eq!(candidate("scan/rust/", &prs).map(|p| p.number), Some(398));
        assert_eq!(candidate("scan/go/", &prs), None);
    }

    #[test]
    fn truncation_check_rejects_a_full_listing() {
        assert!(check_not_truncated(999, 1000).is_ok());
        assert!(check_not_truncated(1000, 1000).is_err());
    }

    #[test]
    fn parse_open_prs_reads_gh_json() {
        let json = r#"[{"number":398,"headRefName":"scan/rust/2026-10-16","headRefOid":"9f27","body":null,"isCrossRepository":false,"commitsVerified":true},
                      {"number":5,"headRefName":"x","headRefOid":"1","body":"b"}]"#;
        let prs = parse_open_prs(json).unwrap();
        assert_eq!(prs[0].number, 398);
        assert_eq!(prs[0].head_oid, "9f27");
        assert_eq!(prs[0].body, "");
        assert!(!prs[0].cross_repo);
        assert!(prs[0].commits_verified);
        assert!(
            !prs[1].commits_verified,
            "a missing commitsVerified must read as unverified, not error"
        );
        assert!(
            prs[1].cross_repo,
            "missing isCrossRepository must fail closed"
        );
    }

    #[test]
    fn parse_open_prs_rejects_malformed_rows() {
        assert!(parse_open_prs(r#"[{"number":1,"headRefOid":"a"}]"#).is_err());
        assert!(parse_open_prs("not json").is_err());
    }
}
