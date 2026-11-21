use std::{sync::Arc, time::Instant};

use anyhow::{Context, Result};
use lcov::Report;

use crate::{
    config::{Config, TestType},
    instruction::{cmd, set},
    runner::{RunResult, Shared, TrackRunner},
    workspace::Workspace,
};

mod tool_setup;

/// Runs the full test pipeline (Setup -> Parallel Base / MSRV / Minimal-Versions -> Summary).
pub fn run_pipeline(config: &Config) -> Result<()> {
    let start_time = Instant::now();

    // Step 1: Declare instructions
    let base_setup = vec![
        set!(CARGO_BUILD_WARNINGS = "deny"),
        set!(RUSTDOCFLAGS = "-D warnings"),
        cmd!(cargo +stable "update"; "cargo update"),
        cmd!(cargo +stable "fetch"; "cargo fetch"),
        set!(CARGO_NET_OFFLINE = "true"),
    ];

    let base_instructions = [
        cmd!(cargo +stable "test" "--workspace"; "stable test"),
        cmd!(coverage +nightly "--workspace"; "nightly"; "nightly test coverage"),
        cmd!(cargo +nightly "doc" "--no-deps" "--workspace"; "doc"),
        cmd!(cargo +nightly "clippy" "--workspace" "--" "-D" "warnings"; "clippy"),
        cmd!(cargo +stable "fmt" "--check" "--all"; "check fmt"),
    ];

    let err_span_check_instructions = config.err_span_check_test.as_ref().map(|test_name| [
        set!(ERR_SPAN_CHECK = if config.args.frozen { "frozen" } else { "update" }),
        cmd!(coverage +stable "--workspace" test_name "--" "--ignored"; "err_stable"; "err_span_check stable"),
        set!(ERR_SPAN_CHECK_SPLIT_ON_CHANGE = "true"),
        cmd!(coverage +nightly "--workspace" test_name "--" "--ignored"; "err_nightly"; "err_span_check nightly"),
    ]);

    let msrv_setup = {
        let mut setup = vec![
            set!(RUSTFLAGS = "-D warnings"), // MSRV might not have CARGO_BUILD_WARNINGS yet
            cmd!(cargo +{config.msrv} "update"; "cargo update"),
        ];

        setup.extend(config.msrv_overrides.iter().map(
            |ovr| cmd!(cargo +{config.msrv} "update" "--package" {&ovr.package} "--precise" {&ovr.version}; "msrv override update"),
        ));

        setup.extend([
            cmd!(cargo +{config.msrv} "fetch"; "cargo fetch"),
            set!(CARGO_NET_OFFLINE = "true"),
        ]);

        setup
    };

    let msrv_instructions = [cmd!(cargo +{config.msrv} "test" "--workspace"; "msrv test")];

    let min_versions_setup = [
        set!(CARGO_BUILD_WARNINGS = "deny"),
        cmd!(cargo +nightly "-Z" "minimal-versions" "update"; "minimal-versions update"),
        cmd!(cargo +stable "fetch"; "cargo fetch"),
        set!(CARGO_NET_OFFLINE = "true"), // we can set this now that fetch is complete
    ];

    let min_versions_instructions = [
        cmd!(cargo +stable "test" "--workspace"; "stable test"),
        cmd!(cargo +nightly "test" "--workspace"; "nightly test"),
    ];

    // Step 2: Create workspaces and runners

    // Ensure directories exist
    let log_dir = config.base_dir.join("target").join("log");
    let cov_base_dir = config.base_dir.join("target").join("cov");
    fs_err::create_dir_all(&log_dir)?;
    fs_err::create_dir_all(&cov_base_dir)?;

    let shared = Arc::new(Shared {
        base_dir: config.base_dir.clone(),
        log_dir,
        cov_base_dir,
        multi_progress: indicatif::MultiProgress::new(),
    });

    let base_workspace = Workspace::new_base_dir(&config.base_dir);

    let tool_setup_runner = TrackRunner::new(
        "setup",
        "Setup",
        Arc::clone(&shared),
        base_workspace.clone(),
        [],
        [],
    )?;
    tool_setup::setup_tools(config, tool_setup_runner)?;

    let msrv_dir_name = format!("msrv_{}", config.msrv);

    // clean up any different MSRV workspaces
    if let Ok(entries) = fs_err::read_dir(config.base_dir.join("target")) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir()
                && let Some(filename) = path.file_name()
                && filename.to_string_lossy().starts_with("msrv_")
                && filename != msrv_dir_name.as_str()
            {
                let _ = fs_err::remove_dir_all(path);
            }
        }
    }

    let msrv_workspace = Workspace::new_isolated(&config.base_dir, &msrv_dir_name)?;
    let min_versions_workspace = Workspace::new_isolated(&config.base_dir, "min_versions")?;

    let (base_setup, err_span_check_setup) = if config.should_run(TestType::Base) {
        (base_setup, vec![]) // base runner runs first and runs the setup
    } else {
        (vec![], base_setup) // base setup is run by err_span_check_runner if needed
    };

    let mut base_runner = if config.should_run(TestType::Base) {
        Some(TrackRunner::new(
            "base",
            "Base",
            Arc::clone(&shared),
            base_workspace.clone(),
            base_setup,
            base_instructions,
        )?)
    } else {
        None
    };

    let mut msrv_runner = if config.should_run(TestType::Msrv) {
        Some(TrackRunner::new(
            "msrv",
            format!("MSRV {}", config.msrv),
            Arc::clone(&shared),
            msrv_workspace,
            msrv_setup,
            msrv_instructions,
        )?)
    } else {
        None
    };

    let mut min_versions_runner = if config.should_run(TestType::MinVersions) {
        Some(TrackRunner::new(
            "min_versions",
            "MinVersions",
            Arc::clone(&shared),
            min_versions_workspace,
            min_versions_setup,
            min_versions_instructions,
        )?)
    } else {
        None
    };

    let mut err_span_check_runner = if config.should_run(TestType::ErrSpanCheck)
        && let Some(instructions) = err_span_check_instructions
    {
        Some(TrackRunner::new(
            "err_span_check",
            "ErrSpanCheck",
            Arc::clone(&shared),
            base_workspace,
            err_span_check_setup,
            instructions,
        )?)
    } else {
        None
    };

    // Step 3: Run setup instructions sequentially
    let mut result = RunResult::default();
    for runner in [
        base_runner.as_mut(),
        msrv_runner.as_mut(),
        min_versions_runner.as_mut(),
        err_span_check_runner.as_mut(),
    ] {
        if result.success
            && let Some(runner) = runner
        {
            result &= runner.run_setup()?;
        }
    }

    // Step 4: Run main instructions in parallel
    if result.success {
        std::thread::scope(|s| -> Result<()> {
            let handles = [base_runner, msrv_runner, min_versions_runner]
                .into_iter()
                .flatten()
                .map(|runner| s.spawn(|| runner.run_instructions()))
                .collect::<Vec<_>>();

            for handle in handles {
                result &= handle.join().expect("Failed to join thread")?;
            }
            Ok(())
        })?;
    }

    // Step 5: Run ErrSpanCheck instructions if applicable
    if result.success
        && let Some(err_span_check_runner) = err_span_check_runner
    {
        result &= err_span_check_runner.run_instructions()?;
    }

    // Step 6: Completion Summary
    let elapsed = start_time.elapsed().as_secs();
    if result.success {
        let scope = if config.has_track_selection() {
            "Selected"
        } else {
            "All"
        };
        println!("{scope} tests passed in {elapsed} seconds!");
    } else {
        println!("Full-test unsuccessful in {elapsed} seconds!");
    }

    if !result.coverage_reports.is_empty() {
        // Step 7: Coverage Report
        let mut report = Report::new();
        for part_report in result.coverage_reports {
            report
                .merge(part_report)
                .context("Failed to merge coverage report")?;
        }

        let merged_cov_path = shared.cov_base_dir.join("lcov.info");
        let mut merged_cov_file = std::fs::File::create(&merged_cov_path)
            .context("Failed to create merged coverage file")?;

        #[derive(Default)]
        struct Counter {
            total: u64,
            hit: u64,
        }
        impl Counter {
            fn add(&mut self, hit_count: u64) {
                self.total += 1;
                if hit_count > 0 {
                    self.hit += 1;
                }
            }
        }
        impl std::fmt::Display for Counter {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                let percent = (self.hit as f64 / self.total as f64) * 100.0;
                write!(f, "{}/{} ({percent:.2}%)", self.hit, self.total)
            }
        }

        let mut line_stat = Counter::default();
        let mut function_stat = Counter::default();
        let mut branch_stat = Counter::default();
        for v in report.sections.values() {
            for line in v.lines.values() {
                line_stat.add(line.count);
            }
            for function in v.functions.values() {
                function_stat.add(function.count);
            }
            for branch in v.branches.values() {
                branch_stat.add(branch.taken.unwrap_or(0));
            }
        }
        print!("Coverage summary:");
        if line_stat.total > 0 {
            print!(" lines {line_stat}");
        }
        if function_stat.total > 0 {
            print!(" functions {function_stat}");
        }
        if branch_stat.total > 0 {
            print!(" branches {branch_stat}");
        }
        println!();

        for record in report.into_records() {
            use std::io::Write;
            writeln!(merged_cov_file, "{}", record)
                .context("Failed to write to merged coverage file")?;
        }

        let relative_path = merged_cov_path.strip_prefix(&shared.base_dir).unwrap(); // unwrap: path was created through `join`s
        println!("Merged coverage report: {}", relative_path.display());
    }

    Ok(())
}
