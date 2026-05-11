//! `scanner` — daily catalog scanner CLI for containers-db.
//!
//! Thin wrapper over [`scanner::run`]: parses CLI flags, propagates the
//! exit code. All orchestration logic lives in the library so it stays
//! testable.

use std::process::ExitCode;

use clap::Parser;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = scanner::Cli::parse();
    match scanner::run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("scanner: {e:#}");
            ExitCode::from(1)
        }
    }
}
