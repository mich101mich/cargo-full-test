use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;
use std::{
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::{Result, bail};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

use crate::config::Config;
use crate::runner::TrackRunner;
use crate::test_dir::{IsolatedDir, prepare_isolated_test_dir};

/// Runs the full test pipeline (Setup -> Parallel Base / MSRV / Minimal-Versions -> Summary).
pub fn run_pipeline(config: &Config) -> Result<()> {
    let start_time = Instant::now();

    verify_required_executables()?;

    // Ensure coverage directories exist
    let cov_base = config.base_dir.join("target").join("cov");
    fs_err::create_dir_all(cov_base.join("nightly"))?;
    if config.run_error_message_tests {
        fs_err::create_dir_all(cov_base.join("err_stable"))?;
        fs_err::create_dir_all(cov_base.join("err_nightly"))?;
    }

    let multi_progress = MultiProgress::new();
    let cancelled = Arc::new(AtomicBool::new(false));

    // Phase 1: Pre-flight & Setup (Sequential)
    run_setup_phase(
        config,
        create_progress(&multi_progress, "Setup", 5),
        Arc::clone(&cancelled),
    )?;

    let msrv_dir_name = format!("msrv_{}", config.msrv);
    let msrv_dir = prepare_isolated_test_dir(&config.base_dir, &msrv_dir_name)?;
    let min_versions_dir = prepare_isolated_test_dir(&config.base_dir, "min_versions")?;

    let base_name = "Base";
    let msrv_name = &format!("MSRV ({})", config.msrv);
    let min_versions_name = "MinVersions";

    let name_length = base_name
        .len()
        .max(msrv_name.len())
        .max(min_versions_name.len());

    // Phase 2: Parallel Test Tracks
    let mut base_runner = TrackRunner::new(
        "base",
        base_name,
        &config.base_dir,
        &config.base_dir,
        create_progress(&multi_progress, base_name, name_length),
        Arc::clone(&cancelled),
    )?;
    let mut msrv_runner = TrackRunner::new(
        "msrv",
        msrv_name,
        &config.base_dir,
        &msrv_dir.work_dir,
        create_progress(&multi_progress, msrv_name, name_length),
        Arc::clone(&cancelled),
    )?;
    let mut min_versions_runner = TrackRunner::new(
        "min_versions",
        min_versions_name,
        &config.base_dir,
        &min_versions_dir.work_dir,
        create_progress(&multi_progress, min_versions_name, name_length),
        Arc::clone(&cancelled),
    )?;

    configure_base_env(config, &mut base_runner);
    configure_msrv_env(&msrv_dir, &mut msrv_runner);
    configure_min_versions_env(&min_versions_dir, &mut min_versions_runner);

    // Update lockfiles and pre-fetch/unpack all crates sequentially before launching
    // the parallel tracks so they don't contend on Cargo's global package cache lock.
    prepare_base_deps(&mut base_runner)?;
    prepare_msrv_deps(config, &mut msrv_runner)?;
    prepare_min_versions_deps(&mut min_versions_runner)?;

    let (base_res, msrv_res, min_res) = std::thread::scope(|s| {
        let base_handle = s.spawn(|| run_base_track(config, base_runner));
        let msrv_handle = s.spawn(|| run_msrv_track(config, msrv_runner));
        let min_handle = s.spawn(|| run_min_versions_track(min_versions_runner));

        let r1 = base_handle
            .join()
            .unwrap_or_else(|_| bail!("Base track thread panicked"));
        let r2 = msrv_handle
            .join()
            .unwrap_or_else(|_| bail!("MSRV track thread panicked"));
        let r3 = min_handle
            .join()
            .unwrap_or_else(|_| bail!("Minimal-Versions track thread panicked"));
        (r1, r2, r3)
    });

    base_res?;
    msrv_res?;
    min_res?;

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

fn create_progress(multi_progress: &MultiProgress, name: &str, name_length: usize) -> ProgressBar {
    let style = ProgressStyle::with_template("{spinner:.cyan} [{prefix:.bold}] {wide_msg}")
        .unwrap_or_else(|_| ProgressStyle::default_spinner());

    let pb = multi_progress.add(ProgressBar::new_spinner());
    pb.set_style(style);
    pb.set_prefix(format!("{:width$}", name, width = name_length));
    pb.enable_steady_tick(Duration::from_millis(100));
    pb
}

fn run_setup_phase(config: &Config, pb: ProgressBar, cancelled: Arc<AtomicBool>) -> Result<()> {
    let runner = TrackRunner::new(
        "setup",
        "Setup",
        &config.base_dir,
        &config.base_dir,
        pb,
        cancelled,
    )?;

    runner.run_cmd("rustup", &["update"])?;
    runner.run_cmd("rustup", &["install", "stable"])?;
    runner.run_cmd("rustup", &["install", "nightly"])?;
    runner.run_cmd("rustup", &["install", &config.msrv])?;
    runner.run_cmd(
        "rustup",
        &[
            "component",
            "add",
            "--toolchain",
            "stable",
            "rustfmt",
            "rust-src",
            "llvm-tools-preview",
        ],
    )?;
    runner.run_cmd(
        "rustup",
        &[
            "component",
            "add",
            "--toolchain",
            "nightly",
            "clippy",
            "rust-src",
            "llvm-tools-preview",
        ],
    )?;
    runner.run_cmd("cargo", &["install", "cargo-llvm-cov"])?;

    runner.finish("Done");
    Ok(())
}

fn configure_base_env(config: &Config, runner: &mut TrackRunner) {
    let target_dir = config.base_dir.join("target");
    runner.set_env("CARGO_TARGET_DIR", target_dir.to_string_lossy());
    runner.set_env("CARGO_BUILD_WARNINGS", "deny");
    runner.set_env("RUSTDOCFLAGS", "-D warnings");
    runner.remove_env("RUSTFLAGS");
}

fn configure_msrv_env(msrv_dir: &IsolatedDir, runner: &mut TrackRunner) {
    runner.set_env(
        "CARGO_TARGET_DIR",
        msrv_dir.cargo_target_dir.to_string_lossy(),
    );
    runner.set_env("RUSTFLAGS", "-D warnings");
    runner.remove_env("CARGO_BUILD_WARNINGS");
}

fn configure_min_versions_env(min_versions_dir: &IsolatedDir, runner: &mut TrackRunner) {
    runner.set_env(
        "CARGO_TARGET_DIR",
        min_versions_dir.cargo_target_dir.to_string_lossy(),
    );
    runner.set_env("CARGO_BUILD_WARNINGS", "deny");
    runner.set_env("RUSTDOCFLAGS", "-D warnings");
    runner.remove_env("RUSTFLAGS");
}

fn prepare_base_deps(runner: &mut TrackRunner) -> Result<()> {
    runner.run_cmd("cargo", &["update"])?;
    runner.run_cmd("cargo", &["+stable", "fetch"])?;
    runner.set_env("CARGO_NET_OFFLINE", "true");
    Ok(())
}

fn prepare_msrv_deps(config: &Config, runner: &mut TrackRunner) -> Result<()> {
    let plus_msrv = format!("+{}", config.msrv);
    runner.run_cmd("cargo", &[&plus_msrv, "update"])?;

    for ovr in &config.msrv_overrides {
        runner.run_cmd(
            "cargo",
            &[
                &plus_msrv,
                "update",
                "-p",
                &ovr.package,
                "--precise",
                &ovr.version,
            ],
        )?;
    }

    runner.run_cmd("cargo", &[&plus_msrv, "fetch"])?;
    runner.set_env("CARGO_NET_OFFLINE", "true");
    Ok(())
}

fn prepare_min_versions_deps(runner: &mut TrackRunner) -> Result<()> {
    runner.run_cmd("cargo", &["+nightly", "-Z", "minimal-versions", "update"])?;
    runner.run_cmd("cargo", &["+stable", "fetch"])?;
    runner.set_env("CARGO_NET_OFFLINE", "true");
    Ok(())
}

fn run_base_track(config: &Config, mut runner: TrackRunner) -> Result<()> {
    runner.run_cmd("cargo", &["+stable", "test", "--workspace"])?;
    runner.run_coverage("nightly", "nightly", &["--workspace"])?;
    runner.run_cmd("cargo", &["+nightly", "doc", "--no-deps", "--workspace"])?;
    runner.run_cmd(
        "cargo",
        &["+nightly", "clippy", "--workspace", "--", "-D", "warnings"],
    )?;
    runner.run_cmd("cargo", &["+stable", "fmt", "--check", "--all"])?;

    if config.run_error_message_tests {
        if config.frozen {
            runner.set_env("ERR_SPAN_CHECK", "frozen");
        }
        runner.run_coverage(
            "stable",
            "err_stable",
            &["error_message_tests", "--workspace", "--", "--ignored"],
        )?;
        runner.run_coverage(
            "nightly",
            "err_nightly",
            &["error_message_tests", "--workspace", "--", "--ignored"],
        )?;
    }

    runner.finish("Passed");
    Ok(())
}

fn run_msrv_track(config: &Config, runner: TrackRunner) -> Result<()> {
    let plus_msrv = format!("+{}", config.msrv);
    runner.run_cmd("cargo", &[&plus_msrv, "test", "--workspace"])?;

    runner.finish("Passed");
    Ok(())
}

fn run_min_versions_track(runner: TrackRunner) -> Result<()> {
    runner.run_cmd("cargo", &["+stable", "test", "--workspace"])?;
    runner.run_cmd("cargo", &["+nightly", "test", "--workspace"])?;

    runner.finish("Passed");
    Ok(())
}
