//! Two-phase scan (#416). A job with no secret runs `--stage <path>`: it
//! generates and validates each tool's changes and writes them, as a
//! [`Manifest`], to `<path>`. The job holding `SCANNER_TOKEN` runs
//! `scanner deliver --plan <path>`, which writes those files back and
//! pushes / opens PRs. ajv and npm never run where the token is.
//!
//! The manifest crosses a trust boundary: the staging job runs third-party
//! code, so a plan carries no free text and the token job writes none of
//! its bytes. A plan names which releases to add; the token job re-renders
//! every file from its own checkout ([`render_files`]) and refuses the plan
//! unless the staged files match. Branch, PR title, and body are likewise
//! rendered from the plan's typed fields.

mod deliver;
mod plan;

pub(crate) use deliver::{
    Schemas, deliver_checked, deliver_manifest, finish, render_files, start, write_and_gate,
};
#[cfg(test)]
pub(crate) use deliver::{deliver_manifest_with, deliver_plan};
pub use plan::{Activity, Failure, Kind, Manifest, Plan, StagedFile};
