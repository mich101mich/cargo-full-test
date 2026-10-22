#![deny(
    missing_debug_implementations,
    trivial_casts,
    trivial_numeric_casts,
    unsafe_code,
    unstable_features,
    unused_import_braces,
    unused_qualifications
)]

//! `cargo full-test` - Parallel full test, lint, coverage, MSRV, and minimal-versions runner.

mod args;
mod config;
mod pipeline;
mod runner;
mod workspace;

use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

use crate::config::Config;

/// CLI arguments for `cargo full-test`.
#[derive(Debug, Parser)]
#[command(
    name = "cargo-full-test",
    bin_name = "cargo full-test",
    version,
    about = "Run the full test, lint, coverage, MSRV, and minimal-versions verification suite in parallel"
)]
pub struct CliArgs {
    /// Enable frozen mode for `error_message_tests` (`ERR_SPAN_CHECK=frozen`).
    #[arg(long)]
    pub frozen: bool,

    /// Optional path to `Cargo.toml`.
    #[arg(long, value_name = "PATH")]
    pub manifest_path: Option<PathBuf>,
}

impl CliArgs {
    /// Parses CLI arguments, stripping a leading `"full-test"` subcommand argument when invoked via `cargo full-test`.
    pub fn parse_from_env() -> Self {
        let mut args: Vec<OsString> = std::env::args_os().collect();
        if args.get(1).and_then(|s| s.to_str()) == Some("full-test") {
            args.remove(1);
        }
        Self::parse_from(args)
    }
}

fn main() -> Result<()> {
    let args = CliArgs::parse_from_env();
    let config = Config::load(args.manifest_path.as_deref(), args.frozen)?;
    pipeline::run_pipeline(&config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_parses_direct_and_cargo_subcommand_forms() {
        let direct = CliArgs::try_parse_from(["cargo-full-test", "--frozen"]).unwrap();
        assert!(direct.frozen);

        // Simulate stripping "full-test" when invoked via `cargo full-test --frozen`
        let mut raw = vec![
            OsString::from("cargo-full-test"),
            OsString::from("full-test"),
            OsString::from("--frozen"),
        ];
        if raw.get(1).and_then(|s| s.to_str()) == Some("full-test") {
            raw.remove(1);
        }
        let sub = CliArgs::try_parse_from(raw).unwrap();
        assert!(sub.frozen);
    }
}
