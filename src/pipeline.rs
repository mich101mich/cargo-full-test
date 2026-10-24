use std::{
    process::{Command, Stdio},
    sync::Arc,
    time::Instant,
};

use anyhow::{Context, Result, bail};
use lcov::Report;

use crate::{
    config::{Config, TestType},
    instruction::{Instruction, args, cmd, set},
    runner::{Shared, TrackRunner},
    workspace::Workspace,
};

/// Runs the full test pipeline (Setup -> Parallel Base / MSRV / Minimal-Versions -> Summary).
pub fn run_pipeline(config: &Config) -> Result<()> {
    let start_time = Instant::now();

    verify_required_executables()?;

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
        fetch_mutex: std::sync::Mutex::new(()),
    });

    let base_workspace = Workspace::new_base_dir(&config.base_dir);

    // Phase 1: Pre-flight & Setup (Sequential)
    let setup_runner = TrackRunner::new(
        "setup",
        "Setup",
        Arc::clone(&shared),
        base_workspace.clone(),
    )?;
    let mut result = setup_runner.run_instructions(setup_instructions(config))?;

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

    // Phase 2: Parallel Test Tracks
    let base_runner = TrackRunner::new("base", "Base", Arc::clone(&shared), base_workspace)?;
    let base_instructions = base_track(config);

    let msrv_runner = TrackRunner::new(
        "msrv",
        format!("MSRV {}", config.msrv),
        Arc::clone(&shared),
        msrv_workspace,
    )?;
    let msrv_instructions = msrv_track(config);

    let min_versions_runner = TrackRunner::new(
        "min_versions",
        "MinVersions",
        Arc::clone(&shared),
        min_versions_workspace,
    )?;
    let min_versions_instructions = min_versions_track(config);

    if result.success {
        std::thread::scope(|s| -> Result<()> {
            let should_run_base =
                config.should_run(TestType::Base) || config.should_run(TestType::ErrSpanCheck);
            let base = should_run_base
                .then(|| s.spawn(|| base_runner.run_instructions(base_instructions)));

            let msrv = config
                .should_run(TestType::Msrv)
                .then(|| s.spawn(|| msrv_runner.run_instructions(msrv_instructions)));

            let min_versions = config.should_run(TestType::MinVersions).then(|| {
                s.spawn(|| min_versions_runner.run_instructions(min_versions_instructions))
            });

            if let Some(base) = base {
                result.and_assign(base.join().expect("Failed to join thread")?);
            }
            if let Some(msrv) = msrv {
                result.and_assign(msrv.join().expect("Failed to join thread")?);
            }
            if let Some(min_versions) = min_versions {
                result.and_assign(min_versions.join().expect("Failed to join thread")?);
            }
            Ok(())
        })?;
    }

    // Phase 3: Completion Summary
    let elapsed = start_time.elapsed().as_secs();
    if result.success {
        let scope = if config.has_only() { "Selected" } else { "All" };
        println!("{scope} tests passed in {elapsed} seconds!");
    } else {
        println!("Full-test unsuccessful in {elapsed} seconds!");
    }

    if !result.coverage_reports.is_empty() {
        // Phase 4: Coverage Report
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

fn verify_required_executables() -> Result<()> {
    for cmd in ["rustup", "cargo", "git"] {
        let ok = Command::new(cmd)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());

        if !ok {
            bail!("cargo-full-test requires command '{cmd}' in PATH. Please install it.");
        }
    }
    Ok(())
}

fn setup_instructions(config: &Config) -> Vec<Instruction> {
    vec![
        cmd!(rustup "update"; "rustup update"),
        cmd!(rustup "install" "stable" "nightly" {&config.msrv}; "rustup install"),
        cmd![rustup "component" "add" "--toolchain" "stable" "rustfmt" "rust-src" "llvm-tools-preview"; "add stable components"],
        cmd![rustup "component" "add" "--toolchain" "nightly" "clippy" "rust-src" "llvm-tools-preview"; "add nightly components"],
        cmd!(cargo +stable "install" "cargo-llvm-cov"; "install cargo-llvm-cov"),
    ]
}

fn base_track(config: &Config) -> Vec<Instruction> {
    let mut instructions = vec![
        set!(CARGO_BUILD_WARNINGS = "deny"),
        set!(RUSTDOCFLAGS = "-D warnings"),
        Instruction::TakeMutex,
        cmd!(cargo +stable "update"; "cargo update"),
        cmd!(cargo +stable "fetch"; "cargo fetch"),
        Instruction::ReleaseMutex,
        set!(CARGO_NET_OFFLINE = "true"),
    ];

    if config.should_run(TestType::Base) {
        instructions.extend([
            cmd!(cargo +stable "test" "--workspace"; "stable test"),
            cmd!(coverage +nightly "--workspace"; "nightly"; "nightly test coverage"),
            cmd!(cargo +nightly "doc" "--no-deps" "--workspace"; "doc"),
            cmd!(cargo +nightly "clippy" "--workspace" "--" "-D" "warnings"; "clippy"),
            cmd!(cargo +stable "fmt" "--check" "--all"; "check fmt"),
        ]);
    }

    if config.should_run(TestType::ErrSpanCheck)
        && let Some(test_name) = &config.err_span_check_test
    {
        if config.args.frozen {
            instructions.push(set!(ERR_SPAN_CHECK = "frozen"));
        }

        instructions.extend([
            cmd!(coverage +stable test_name "--" "--ignored"; "err_stable"; "err_span_check stable"),
            set!(ERR_SPAN_CHECK_SPLIT_ON_CHANGE = "true"),
            cmd!(coverage +nightly test_name "--" "--ignored"; "err_nightly"; "err_span_check nightly"),
        ]);
    }

    instructions
}

fn msrv_track(config: &Config) -> Vec<Instruction> {
    let mut instructions = vec![
        set!(RUSTFLAGS = "-D warnings"), // MSRV might not have CARGO_BUILD_WARNINGS yet
        Instruction::TakeMutex,
        cmd!(cargo +{config.msrv} "update"; "cargo update"),
    ];

    instructions.extend(config.msrv_overrides.iter().map(
        |ovr| cmd!(cargo +{config.msrv} "update" "--package" {&ovr.package} "--precise" {&ovr.version}; "msrv override update"),
    ));

    instructions.extend([
        cmd!(cargo +{config.msrv} "fetch"; "cargo fetch"),
        Instruction::ReleaseMutex,
        set!(CARGO_NET_OFFLINE = "true"),
        cmd!(cargo +{config.msrv} "test" "--workspace"; "msrv test"),
    ]);

    instructions
}

fn min_versions_track(_config: &Config) -> Vec<Instruction> {
    vec![
        set!(CARGO_BUILD_WARNINGS = "deny"),
        Instruction::TakeMutex,
        cmd!(cargo +nightly "-Z" "minimal-versions" "update"; "minimal-versions update"),
        cmd!(cargo +stable "fetch"; "cargo fetch"),
        Instruction::ReleaseMutex,
        set!(CARGO_NET_OFFLINE = "true"), // we can set this now that fetch is complete
        cmd!(cargo +stable "test" "--workspace"; "stable test"),
        cmd!(cargo +nightly "test" "--workspace"; "nightly test"),
    ]
}
