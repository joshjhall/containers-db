//! The read-only GitHub lookups behind [`super::deliver`]: list the open
//! PRs and check the dedup candidate's commit signatures. Split out of
//! `pr.rs` (#531); the seam a direct GitHub API client replaces (#532).

use std::path::Path;

use anyhow::{Context, Result};

use super::gh;
use crate::dedup::{self, OpenPr};

/// Upper bound on listed open PRs. Hitting it errors rather than risk
/// missing the tool's scan PR and opening a duplicate.
const PR_LIST_LIMIT: usize = 1000;

/// Open PRs in the repo, as [`OpenPr`]s.
pub(super) fn list_open_prs(repo_root: &Path) -> Result<Vec<OpenPr>> {
    let limit = PR_LIST_LIMIT.to_string();
    let out = gh(
        repo_root,
        &[
            "pr",
            "list",
            "--state",
            "open",
            "--limit",
            &limit,
            "--json",
            dedup::GH_PR_FIELDS,
        ],
    )?;
    let prs = dedup::parse_open_prs(&out).map_err(anyhow::Error::msg)?;
    dedup::check_not_truncated(prs.len(), PR_LIST_LIMIT).map_err(anyhow::Error::msg)?;
    Ok(prs)
}

/// Look up whether every commit on the PR [`dedup::decide`] will act on
/// is GitHub-verified, and record it on that row. Only the candidate needs
/// it, so this is one API call per tool rather than one per open PR.
///
/// A failed lookup leaves the row unverified: a redundant regen costs one
/// signed commit, a wrong skip strands the release (#529). `lookup` is
/// [`commits_verified`] in production, injected so the fail-closed path is
/// testable without `gh`.
pub(super) fn mark_candidate_verified(
    tool_id: &str,
    prefix: &str,
    open: &mut [OpenPr],
    lookup: impl FnOnce(&OpenPr) -> Result<bool>,
) {
    let Some(number) = dedup::candidate(prefix, open).map(|pr| pr.number) else {
        return;
    };
    let Some(pr) = open.iter_mut().find(|pr| pr.number == number) else {
        return;
    };
    pr.commits_verified = match lookup(pr) {
        Ok(v) => v,
        Err(e) => {
            eprintln!(
                "scanner: {tool_id}: could not check the commit signatures of PR #{number}; treating it as unsigned: {e:#}"
            );
            false
        }
    };
}

/// The most commits `pulls/{n}/commits` returns, however it is paginated.
/// A listing that reaches it may be truncated, so it is refused.
const PR_COMMITS_LIMIT: usize = 250;

/// Whether every commit on `pr` carries a GitHub-verified signature.
pub(super) fn commits_verified(repo_root: &Path, pr: &OpenPr) -> Result<bool> {
    let out = gh(
        repo_root,
        &[
            "api",
            "--paginate",
            &format!("repos/{{owner}}/{{repo}}/pulls/{}/commits", pr.number),
            "--jq",
            r#".[] | "\(.sha) \(.commit.verification.verified)""#,
        ],
    )?;
    parse_verified(&out, &pr.head_oid)
}

/// Parse `<sha> <verified>` lines, one per commit, across every page. Verified only when every line says `true`. An error, so the
/// caller fails toward a regen, when:
/// - the listing is empty, or a line is not `<sha> true|false`;
/// - it reaches [`PR_COMMITS_LIMIT`] — unsigned commits past the cap
///   would go unseen and read as a wrong skip;
/// - `head_oid` is not among its commits — the branch was rewritten or
///   replaced since the PRs were listed, so this answer describes commits
///   `decide` never saw. Membership, not position: the endpoint's order is
///   undocumented. A fast-forward since listing still contains `head_oid`
///   and passes; that is safe, because the answer then covers the newer
///   commits too, and a regen's lease on `head_oid` rejects a stale push.
fn parse_verified(out: &str, head_oid: &str) -> Result<bool> {
    let mut all = true;
    let mut has_head = false;
    let mut count = 0;
    for line in out.lines() {
        let (sha, verified) = line
            .trim()
            .split_once(' ')
            .with_context(|| format!("unexpected commit verification line {line:?}"))?;
        match verified {
            "true" => {}
            "false" => all = false,
            other => anyhow::bail!("unexpected commit verification value {other:?}"),
        }
        has_head |= sha == head_oid;
        count += 1;
    }
    anyhow::ensure!(count > 0, "no commits listed for the PR");
    anyhow::ensure!(
        count < PR_COMMITS_LIMIT,
        "PR lists {count} commits, at the {PR_COMMITS_LIMIT}-commit API cap; refusing to trust a possibly truncated listing"
    );
    anyhow::ensure!(
        has_head,
        "PR head {head_oid} is not among its listed commits; the branch moved since listing"
    );
    Ok(all)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pr::tests::open_pr_fixture;

    /// `<sha> <verified>` lines for commits `c0..`.
    fn commits(verified: &[bool]) -> String {
        verified
            .iter()
            .enumerate()
            .map(|(i, v)| format!("c{i} {v}\n"))
            .collect()
    }

    #[test]
    fn parse_verified_requires_every_commit_verified() {
        assert!(parse_verified(&commits(&[true]), "c0").unwrap());
        assert!(parse_verified(&commits(&[true, true, true]), "c2").unwrap());
        assert!(!parse_verified(&commits(&[false]), "c0").unwrap());
        // A signed fixup on top of an unsigned commit is still unmergeable.
        assert!(!parse_verified(&commits(&[false, true]), "c1").unwrap());
        assert!(!parse_verified(&commits(&[true, false, true]), "c2").unwrap());
        // The empty-listing guard is checked by its own message: the head
        // check would also reject an empty listing, masking its removal.
        let err = parse_verified("", "c0").unwrap_err();
        assert!(
            format!("{err:#}").contains("no commits listed"),
            "got {err:#}"
        );
        // A blank line is malformed, not an empty listing.
        for bad in [
            "\n",
            "c0 null\n",
            "c0 True\n",
            "true\n",
            "c0 true\nc1 yes\n",
        ] {
            assert!(
                parse_verified(bad, "c0").is_err(),
                "{bad:?} must be an error"
            );
        }
    }

    /// The commits endpoint stops at 250 however it is paginated: a full
    /// listing may hide unsigned commits past the cap, so it is refused.
    #[test]
    fn parse_verified_refuses_a_listing_at_the_api_cap() {
        let under = commits(&vec![true; PR_COMMITS_LIMIT - 1]);
        let head = format!("c{}", PR_COMMITS_LIMIT - 2);
        assert!(parse_verified(&under, &head).unwrap());
        let at = commits(&vec![true; PR_COMMITS_LIMIT]);
        let head = format!("c{}", PR_COMMITS_LIMIT - 1);
        let err = parse_verified(&at, &head).unwrap_err();
        assert!(format!("{err:#}").contains("commit API cap"), "got {err:#}");
    }

    /// A force-push between listing and lookup: the answer describes a
    /// head `decide` never saw, so it is refused rather than trusted.
    #[test]
    fn parse_verified_refuses_a_head_not_in_the_listing() {
        let err = parse_verified(&commits(&[true, true]), "gone").unwrap_err();
        assert!(format!("{err:#}").contains("branch moved"), "got {err:#}");
    }

    /// The endpoint's order is undocumented, so the head need not be last.
    #[test]
    fn parse_verified_accepts_the_head_anywhere_in_the_listing() {
        assert!(parse_verified(&commits(&[true, true, true]), "c0").unwrap());
        assert!(!parse_verified(&commits(&[true, false, true]), "c0").unwrap());
    }

    /// Two matching scan PRs plus a fork; only the oldest non-fork row is
    /// the candidate, so only it is looked up and only it is marked.
    fn listing() -> Vec<OpenPr> {
        let mut fork = open_pr_fixture(5, "scan/rust/evil");
        fork.cross_repo = true;
        vec![
            fork,
            open_pr_fixture(402, "scan/rust/2026-10-17"),
            open_pr_fixture(398, "scan/rust/2026-10-16"),
        ]
    }

    #[test]
    fn mark_candidate_verified_marks_only_the_candidate() {
        let mut open = listing();
        let mut asked = None;
        mark_candidate_verified("rust", "scan/rust/", &mut open, |pr| {
            asked = Some(pr.number);
            Ok(true)
        });
        assert_eq!(asked, Some(398));
        let marked: Vec<u64> = open
            .iter()
            .filter(|p| p.commits_verified)
            .map(|p| p.number)
            .collect();
        assert_eq!(marked, vec![398]);
    }

    /// #529: a lookup that errors must leave the candidate unverified, so
    /// `decide` regenerates it rather than skipping it.
    #[test]
    fn mark_candidate_verified_fails_closed_on_lookup_error() {
        let mut open = listing();
        for pr in &mut open {
            pr.commits_verified = true;
        }
        mark_candidate_verified("rust", "scan/rust/", &mut open, |_| {
            anyhow::bail!("gh api: 502")
        });
        let candidate = open.iter().find(|p| p.number == 398).unwrap();
        assert!(!candidate.commits_verified);
        assert!(
            open.iter()
                .filter(|p| p.number != 398)
                .all(|p| p.commits_verified),
            "non-candidate rows must be left untouched"
        );
    }

    #[test]
    fn mark_candidate_verified_skips_lookup_without_a_candidate() {
        // Start from `true` so the final assert can tell "left untouched"
        // apart from the fixture's `false` default.
        let mut open = vec![OpenPr {
            commits_verified: true,
            ..open_pr_fixture(10, "scan/node/2026-10-16")
        }];
        mark_candidate_verified("rust", "scan/rust/", &mut open, |_| {
            panic!("no candidate: lookup must not run")
        });
        assert!(
            open[0].commits_verified,
            "a non-candidate row is left untouched"
        );
    }
}
