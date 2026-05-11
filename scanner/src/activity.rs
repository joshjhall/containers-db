//! Recompute the `activity.score` tier and `activity.scan_cadence_days`
//! from raw upstream signals.
//!
//! Issue #406 is the first place this logic is concretized; #400
//! intentionally left the signal-to-tier mapping out of the schema so
//! it could iterate. The rule table below is first-match-wins from
//! [`Score::Abandoned`] down to [`Score::VeryActive`]; tune by editing
//! [`compute_tier`].
//!
//! | Tier         | Trigger                                                | cadence_days |
//! | ------------ | ------------------------------------------------------ | ------------ |
//! | abandoned    | source repo archived / disabled / EOL                   | 180          |
//! | dormant      | last_release > 5y ago AND last_commit > 2y ago          | 90           |
//! | stale        | last_release > 18 months ago                            | 90           |
//! | slow         | last_release > 9 months ago                             | 60           |
//! | active       | releases_last_90d ≥ 1 AND last_release ≤ 90d ago        | 7            |
//! | very-active  | releases_last_90d ≥ 3 AND last_release ≤ 30d ago        | 1            |
//! | maintained   | fallback (signal-missing or quiet-but-not-stale)        | 30           |

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// One of the seven tiers declared in `schema/tool.schema.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Score {
    VeryActive,
    Active,
    Maintained,
    Slow,
    Stale,
    Dormant,
    Abandoned,
}

/// Raw upstream signals an [`crate::adapter::Adapter`] hands back. Field
/// names match the JSON Schema for `activity.signals`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ActivitySignals {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub releases_last_90d: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commits_last_90d: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_maintainers: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_advisories: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_release_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_commit_at: Option<DateTime<Utc>>,
}

/// Decide the (score, cadence) pair for a tool. `source_archived` flips
/// the tool to [`Score::Abandoned`] regardless of activity — it's how
/// archived/EOL upstreams get caught even if they had recent releases
/// just before being shut down.
pub fn compute_tier(
    signals: &ActivitySignals,
    source_archived: bool,
    now: DateTime<Utc>,
) -> (Score, u32) {
    if source_archived {
        return (Score::Abandoned, 180);
    }

    // No release timestamp at all → fall back to maintained. Adapters
    // for non-GitHub registry-only tools land here.
    let Some(last_release) = signals.last_release_at else {
        return (Score::Maintained, 30);
    };

    let release_age = now - last_release;
    // If last_commit_at is missing, treat it as equal to release_age so
    // dormant detection only fires when *both* signals say so.
    let commit_age = signals.last_commit_at.map_or(release_age, |c| now - c);
    let releases_90d = signals.releases_last_90d.unwrap_or(0);

    if release_age > Duration::days(365 * 5) && commit_age > Duration::days(365 * 2) {
        return (Score::Dormant, 90);
    }
    if release_age > Duration::days(365 + 180) {
        return (Score::Stale, 90);
    }
    if release_age > Duration::days(270) {
        return (Score::Slow, 60);
    }
    if release_age <= Duration::days(30) && releases_90d >= 3 {
        return (Score::VeryActive, 1);
    }
    if release_age <= Duration::days(90) && releases_90d >= 1 {
        return (Score::Active, 7);
    }
    (Score::Maintained, 30)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(year: i32, month: u32, day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, 0, 0, 0).unwrap()
    }

    #[test]
    fn archived_is_always_abandoned() {
        // Even with very recent activity, an archived source repo is
        // abandoned. Catches projects shut down mid-release-cycle.
        let signals = ActivitySignals {
            last_release_at: Some(at(2026, 5, 1)),
            releases_last_90d: Some(10),
            ..ActivitySignals::default()
        };
        let (score, cadence) = compute_tier(&signals, true, at(2026, 5, 10));
        assert_eq!(score, Score::Abandoned);
        assert_eq!(cadence, 180);
    }

    #[test]
    fn missing_last_release_falls_back_to_maintained() {
        let signals = ActivitySignals::default();
        let (score, cadence) = compute_tier(&signals, false, at(2026, 5, 10));
        assert_eq!(score, Score::Maintained);
        assert_eq!(cadence, 30);
    }

    #[test]
    fn very_active_requires_three_releases_within_thirty_days() {
        let signals = ActivitySignals {
            last_release_at: Some(at(2026, 4, 25)),
            releases_last_90d: Some(4),
            ..ActivitySignals::default()
        };
        let (score, cadence) = compute_tier(&signals, false, at(2026, 5, 1));
        assert_eq!(score, Score::VeryActive);
        assert_eq!(cadence, 1);
    }

    #[test]
    fn two_releases_in_thirty_days_is_active_not_very_active() {
        let signals = ActivitySignals {
            last_release_at: Some(at(2026, 4, 25)),
            releases_last_90d: Some(2),
            ..ActivitySignals::default()
        };
        let (score, cadence) = compute_tier(&signals, false, at(2026, 5, 1));
        assert_eq!(score, Score::Active);
        assert_eq!(cadence, 7);
    }

    #[test]
    fn one_release_within_ninety_days_is_active() {
        let signals = ActivitySignals {
            last_release_at: Some(at(2026, 3, 1)),
            releases_last_90d: Some(1),
            ..ActivitySignals::default()
        };
        let (score, cadence) = compute_tier(&signals, false, at(2026, 5, 1));
        assert_eq!(score, Score::Active);
        assert_eq!(cadence, 7);
    }

    #[test]
    fn no_releases_within_ninety_days_is_maintained() {
        let signals = ActivitySignals {
            last_release_at: Some(at(2026, 3, 1)),
            releases_last_90d: Some(0),
            ..ActivitySignals::default()
        };
        let (score, cadence) = compute_tier(&signals, false, at(2026, 5, 1));
        assert_eq!(score, Score::Maintained);
        assert_eq!(cadence, 30);
    }

    #[test]
    fn nine_to_eighteen_months_is_slow() {
        let signals = ActivitySignals {
            last_release_at: Some(at(2025, 4, 1)),
            ..ActivitySignals::default()
        };
        let (score, cadence) = compute_tier(&signals, false, at(2026, 5, 1));
        assert_eq!(score, Score::Slow);
        assert_eq!(cadence, 60);
    }

    #[test]
    fn over_eighteen_months_is_stale() {
        let signals = ActivitySignals {
            last_release_at: Some(at(2024, 8, 1)),
            ..ActivitySignals::default()
        };
        let (score, cadence) = compute_tier(&signals, false, at(2026, 5, 1));
        assert_eq!(score, Score::Stale);
        assert_eq!(cadence, 90);
    }

    #[test]
    fn very_old_release_with_recent_commits_stays_stale_not_dormant() {
        // Dormant requires BOTH releases and commits to be ancient.
        // Active development without releases (LTS branches, dev forks)
        // should not be branded dormant.
        let signals = ActivitySignals {
            last_release_at: Some(at(2020, 1, 1)),
            last_commit_at: Some(at(2026, 4, 1)),
            ..ActivitySignals::default()
        };
        let (score, _) = compute_tier(&signals, false, at(2026, 5, 1));
        assert_eq!(score, Score::Stale);
    }

    #[test]
    fn five_years_no_release_and_two_years_no_commit_is_dormant() {
        let signals = ActivitySignals {
            last_release_at: Some(at(2020, 1, 1)),
            last_commit_at: Some(at(2023, 1, 1)),
            ..ActivitySignals::default()
        };
        let (score, cadence) = compute_tier(&signals, false, at(2026, 5, 1));
        assert_eq!(score, Score::Dormant);
        assert_eq!(cadence, 90);
    }

    #[test]
    fn last_commit_absent_falls_back_to_release_age_for_dormant_check() {
        // No commit signal → treat commit_age == release_age; the
        // dormant threshold (>2y) is then easily met by a 6-year-old
        // release.
        let signals = ActivitySignals {
            last_release_at: Some(at(2020, 1, 1)),
            ..ActivitySignals::default()
        };
        let (score, _) = compute_tier(&signals, false, at(2026, 5, 1));
        assert_eq!(score, Score::Dormant);
    }
}
