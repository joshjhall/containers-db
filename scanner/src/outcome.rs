//! What a single tool scan or backfill produced.

use crate::pr;

#[derive(Debug)]
pub enum Outcome {
    /// Adapter ran successfully but no new versions were found. The
    /// activity block is *not* written back in this case — the next
    /// scheduled run will reattempt.
    NoChange,
    /// `--dry-run` printed the plan and exited without writing anything.
    DryRun { new_versions: Vec<String> },
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
}
