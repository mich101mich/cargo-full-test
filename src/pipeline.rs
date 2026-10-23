use std::{
    process::{Command, Stdio},
    sync::Arc,
    time::Instant,
};

use anyhow::{Result, bail};

use crate::{
    args::{Instruction, args, cmd, set},
    config::{Config, TestType},
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
    let mut success = run_setup_instructions(config, Arc::clone(&shared), base_workspace.clone())?;

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

    let msrv_runner = TrackRunner::new(
        "msrv",
        format!("MSRV {}", config.msrv),
        Arc::clone(&shared),
        msrv_workspace,
    )?;

    let min_versions_runner = TrackRunner::new(
        "min_versions",
        "MinVersions",
        Arc::clone(&shared),
        min_versions_workspace,
    )?;

    if success {
        success = std::thread::scope(|s| {
            let base = s.spawn(|| run_base_track(config, base_runner));
            let msrv = s.spawn(|| run_msrv_track(config, msrv_runner));
            let min_versions = s.spawn(|| run_min_versions_track(config, min_versions_runner));

            base.join().is_ok_and(|r| r.is_ok_and(|r| r))
                && msrv.join().is_ok_and(|r| r.is_ok_and(|r| r))
                && min_versions.join().is_ok_and(|r| r.is_ok_and(|r| r))
        });
    }

    // Phase 3: Completion Summary
    let elapsed = start_time.elapsed().as_secs();
    if success {
        let scope = if config.has_only() { "Selected" } else { "All" };
        println!("{scope} tests passed in {elapsed} seconds!");
    } else {
        println!("Full-test unsuccessful in {elapsed} seconds!");
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

fn run_setup_instructions(
    config: &Config,
    shared: Arc<Shared>,
    workspace: Workspace,
) -> Result<bool> {
    let setup_runner = TrackRunner::new("setup", "Setup", shared, workspace)?;

    setup_runner.run_instructions(vec![
        cmd!(rustup "update"; "rustup update"),
        cmd!(rustup "install" "stable" "nightly" {&config.msrv}; "rustup install"),
        cmd![rustup "component" "add" "--toolchain" "stable" "rustfmt" "rust-src" "llvm-tools-preview"; "add stable components"],
        cmd![rustup "component" "add" "--toolchain" "nightly" "clippy" "rust-src" "llvm-tools-preview"; "add nightly components"],
        cmd!(cargo +stable "install" "cargo-llvm-cov"; "install cargo-llvm-cov"),
    ])
}

fn run_base_track(config: &Config, runner: TrackRunner) -> Result<bool> {
    if !config.should_run(TestType::Base) && !config.should_run(TestType::ErrSpanCheck) {
        return Ok(true);
    }

    let cov_dir = &runner.shared.cov_base_dir;
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
            cmd!(coverage +nightly --out-dir {cov_dir/"nightly"} "--workspace"; "nightly test coverage"),
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
            cmd!(coverage +stable --out-dir {cov_dir/"err_stable"} test_name "--" "--ignored"; "err_span_check stable"),
            set!(ERR_SPAN_CHECK_SPLIT_ON_CHANGE = "true"),
            cmd!(coverage +nightly --out-dir {cov_dir/"err_nightly"} test_name "--" "--ignored"; "err_span_check nightly"),
        ]);
    }

    runner.run_instructions(instructions)
}

fn run_msrv_track(config: &Config, runner: TrackRunner) -> Result<bool> {
    if !config.should_run(TestType::Msrv) {
        return Ok(true);
    }

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

    runner.run_instructions(instructions)
}

fn run_min_versions_track(config: &Config, runner: TrackRunner) -> Result<bool> {
    if !config.should_run(TestType::MinVersions) {
        return Ok(true);
    }

    runner.run_instructions([
        set!(CARGO_BUILD_WARNINGS = "deny"),
        Instruction::TakeMutex,
        cmd!(cargo +nightly "-Z" "minimal-versions" "update"; "minimal-versions update"),
        cmd!(cargo +stable "fetch"; "cargo fetch"),
        Instruction::ReleaseMutex,
        set!(CARGO_NET_OFFLINE = "true"), // we can set this now that fetch is complete
        cmd!(cargo +stable "test" "--workspace"; "stable test"),
        cmd!(cargo +nightly "test" "--workspace"; "nightly test"),
    ])
}
