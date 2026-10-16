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
    /// Head commit when listed — the explicit `--force-with-lease` value.
    pub head_oid: String,
    pub body: String,
    /// Head branch lives in a fork. Never trusted: anyone can open one.
    pub cross_repo: bool,
}

/// `--json` fields [`parse_open_prs`] expects from `gh pr list`.
pub const GH_PR_FIELDS: &str = "number,headRefName,headRefOid,body,isCrossRepository";

/// Parse `gh pr list --json` output into [`OpenPr`]s.
///
/// Errors on malformed output rather than dropping rows: a dropped row
/// would fail open into a duplicate PR. A missing `isCrossRepository`
/// reads as cross-repo, so [`decide`] ignores the row.
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

/// `git push` lease pinned to the head seen at listing time, so a push
/// that raced in since then rejects ours instead of being overwritten.
pub fn lease_arg(pr: &OpenPr) -> String {
    format!("--force-with-lease={}:{}", pr.head, pr.head_oid)
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
/// `scan/rust-dev/…`. Fork PRs are ignored — their branch name and body are
/// attacker-controlled, and trusting them would let anyone suppress a scan.
/// When several open PRs match, the oldest (lowest number) is the one kept.
pub fn decide(prefix: &str, open_prs: &[OpenPr], new_versions: &[String]) -> Decision {
    let Some(pr) = open_prs
        .iter()
        .filter(|pr| !pr.cross_repo && pr.head.starts_with(prefix))
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
            head_oid: "abc123".to_string(),
            body: body(versions),
            cross_repo: false,
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
            Decision::UpdateExisting(real)
        );
    }

    #[test]
    fn truncation_check_rejects_a_full_listing() {
        assert!(check_not_truncated(999, 1000).is_ok());
        assert!(check_not_truncated(1000, 1000).is_err());
    }

    #[test]
    fn lease_pins_branch_to_listed_head() {
        let p = pr(398, "scan/rust/2026-10-16", &[]);
        assert_eq!(
            lease_arg(&p),
            "--force-with-lease=scan/rust/2026-10-16:abc123"
        );
    }

    #[test]
    fn parse_open_prs_reads_gh_json() {
        let json = r#"[{"number":398,"headRefName":"scan/rust/2026-10-16","headRefOid":"9f27","body":null,"isCrossRepository":false},
                      {"number":5,"headRefName":"x","headRefOid":"1","body":"b"}]"#;
        let prs = parse_open_prs(json).unwrap();
        assert_eq!(prs[0].number, 398);
        assert_eq!(prs[0].head_oid, "9f27");
        assert_eq!(prs[0].body, "");
        assert!(!prs[0].cross_repo);
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
