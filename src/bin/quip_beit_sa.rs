//! BEIT cloud simulated-annealing miner (`quip-beit-sa`).
//!
//! Wraps `BEITUnconstrainedSampler` via a long-lived Python sidecar.
//! Every job is a billed AWS Marketplace call — see README cost guard.

#![deny(clippy::unwrap_used)]
#![deny(clippy::panic)]
#![warn(clippy::expect_used)]

use clap::Parser;
use quip_miner_beit::{BeitConfig, BeitSampler, BEIT_SA_IDENTITY, DEFAULT_MAX_BILLED_JOBS};
use quip_miner_core::{run, CommonArgs};
use std::process::ExitCode;

#[derive(Parser, Debug)]
#[command(version = concat!(env!("CARGO_PKG_VERSION"), " protocol 1"))]
struct Cli {
    #[command(flatten)]
    common: CommonArgs,

    /// Hard cap on billed BEIT jobs per process (default 10).
    ///
    /// Each sample job is a paid AWS Marketplace request. Raise only when you
    /// intend to spend. Env: `BEIT_MAX_BILLED_JOBS`.
    #[arg(long, env = "BEIT_MAX_BILLED_JOBS", default_value_t = DEFAULT_MAX_BILLED_JOBS)]
    max_billed_jobs: u64,

    /// Default BEIT sampler timeout in seconds when the job does not carry an
    /// anneal-time override (capped at BEIT's 12-minute max). Env: `BEIT_TIMEOUT_SECS`.
    #[arg(long, env = "BEIT_TIMEOUT_SECS", default_value_t = 10)]
    timeout_secs: u64,
}

fn main() -> ExitCode {
    // Logging to stderr only (print-stdout is denied in the library; the
    // harness owns capabilities JSON on stdout).
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .try_init();

    let cli = Cli::parse();
    let config = BeitConfig {
        max_billed_jobs: cli.max_billed_jobs.max(1),
        default_timeout_secs: cli.timeout_secs.max(1),
    };

    run(BEIT_SA_IDENTITY, &cli.common, move || {
        BeitSampler::open_live(config).map_err(quip_miner_core::OpenError)
    })
}
