use std::{
    process::{Command, Stdio},
    sync::Arc,
    time::Instant,
};

use anyhow::{Result, bail};

use crate::{
    config::Config,
    runner::{
        Shared,
        Toolchain::{Nightly, Stable},
        TrackRunner,
    },
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
        log_dir,
        cov_base_dir,
        multi_progress: indicatif::MultiProgress::new(),
    });

    let base_workspace = Workspace::new_base_dir(&config.base_dir);

    // Phase 1: Pre-flight & Setup (Sequential)
    run_setup_phase(config, base_workspace.clone(), Arc::clone(&shared))?;

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
    let mut base_runner = TrackRunner::new("base", "Base", Arc::clone(&shared), base_workspace)?;
    configure_base_env(&mut base_runner);
    prepare_base_deps(&mut base_runner)?;

    let mut msrv_runner = TrackRunner::new(
        "msrv",
        format!("MSRV ({})", config.msrv),
        Arc::clone(&shared),
        msrv_workspace,
    )?;
    configure_msrv_env(&mut msrv_runner);
    prepare_msrv_deps(config, &mut msrv_runner)?;

    let mut min_versions_runner = TrackRunner::new(
        "min_versions",
        "MinVersions",
        Arc::clone(&shared),
        min_versions_workspace,
    )?;
    configure_min_versions_env(&mut min_versions_runner);
    prepare_min_versions_deps(&mut min_versions_runner)?;

    std::thread::scope(|s| {
        s.spawn(|| run_base_track(config, base_runner));
        s.spawn(|| run_msrv_track(config, msrv_runner));
        s.spawn(|| run_min_versions_track(min_versions_runner));
    });

    // Phase 3: Completion Summary
    let elapsed = start_time.elapsed().as_secs();
    println!("All tests passed in {elapsed} seconds!");

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

fn run_setup_phase(config: &Config, base_workspace: Workspace, shared: Arc<Shared>) -> Result<()> {
    let runner = TrackRunner::new("setup", "Setup", shared, base_workspace)?;

    runner.run_rustup(&["update"])?;
    runner.run_rustup(&["install", "stable"])?;
    runner.run_rustup(&["install", "nightly"])?;
    runner.run_rustup(&["install", &config.msrv])?;
    runner.run_rustup(&[
        "component",
        "add",
        "--toolchain",
        "stable",
        "rustfmt",
        "rust-src",
        "llvm-tools-preview",
    ])?;
    runner.run_rustup(&[
        "component",
        "add",
        "--toolchain",
        "nightly",
        "clippy",
        "rust-src",
        "llvm-tools-preview",
    ])?;
    runner.run_cargo(Stable, &["install", "cargo-llvm-cov"])?;

    runner.finish("Done");
    Ok(())
}

fn configure_base_env(runner: &mut TrackRunner) {
    runner.set_env("CARGO_BUILD_WARNINGS", "deny");
    runner.set_env("RUSTDOCFLAGS", "-D warnings");
}

fn configure_msrv_env(runner: &mut TrackRunner) {
    runner.set_env("RUSTFLAGS", "-D warnings"); // MSRV does not have CARGO_BUILD_WARNINGS yet
}

fn configure_min_versions_env(runner: &mut TrackRunner) {
    runner.set_env("CARGO_BUILD_WARNINGS", "deny");
}

// Update lockfiles and pre-fetch/unpack all crates sequentially before launching
// the parallel tracks so they don't contend on Cargo's global package cache lock.
fn prepare_base_deps(runner: &mut TrackRunner) -> Result<()> {
    runner.run_cargo(Stable, &["update"])?;
    runner.run_cargo(Stable, &["fetch"])?;
    runner.set_env("CARGO_NET_OFFLINE", "true");
    Ok(())
}

fn prepare_msrv_deps(config: &Config, runner: &mut TrackRunner) -> Result<()> {
    runner.run_cargo(&config.msrv, &["update"])?;

    for ovr in &config.msrv_overrides {
        runner.run_cargo(
            &config.msrv,
            &["update", "-p", &ovr.package, "--precise", &ovr.version],
        )?;
    }

    runner.run_cargo(&config.msrv, &["fetch"])?;
    runner.set_env("CARGO_NET_OFFLINE", "true");
    Ok(())
}

fn prepare_min_versions_deps(runner: &mut TrackRunner) -> Result<()> {
    runner.run_cargo(Nightly, &["-Z", "minimal-versions", "update"])?;
    runner.run_cargo(Stable, &["fetch"])?;
    runner.set_env("CARGO_NET_OFFLINE", "true");
    Ok(())
}

fn run_base_track(config: &Config, mut runner: TrackRunner) -> Result<()> {
    runner.run_cargo(Stable, &["test", "--workspace"])?;
    runner.run_coverage(Nightly, "nightly", &["--workspace"])?;
    runner.run_cargo(Nightly, &["doc", "--no-deps", "--workspace"])?;
    runner.run_cargo(Nightly, &["clippy", "--workspace", "--", "-D", "warnings"])?;
    runner.run_cargo(Stable, &["fmt", "--check", "--all"])?;

    if config.run_error_message_tests {
        if config.frozen {
            runner.set_env("ERR_SPAN_CHECK", "frozen");
        }
        runner.run_coverage(
            Stable,
            "err_stable",
            &["error_message_tests", "--workspace", "--", "--ignored"],
        )?;
        runner.run_coverage(
            Nightly,
            "err_nightly",
            &["error_message_tests", "--workspace", "--", "--ignored"],
        )?;
    }

    runner.finish("Passed");
    Ok(())
}

fn run_msrv_track(config: &Config, runner: TrackRunner) -> Result<()> {
    runner.run_cargo(&config.msrv, &["test", "--workspace"])?;

    runner.finish("Passed");
    Ok(())
}

fn run_min_versions_track(runner: TrackRunner) -> Result<()> {
    runner.run_cargo(Stable, &["test", "--workspace"])?;
    runner.run_cargo(Nightly, &["test", "--workspace"])?;

    runner.finish("Passed");
    Ok(())
}
