use std::process::{Command, Stdio};

use crate::{config::Config, instruction::cmd, runner::TrackRunner};

use anyhow::{Result, bail};

pub(crate) fn setup_tools(config: &Config, runner: TrackRunner) -> Result<()> {
    let mut check = ToolCheck { runner };
    check.commands(["rustup", "cargo"])?;
    check.toolchains(["stable", "nightly", &config.msrv])?;
    check.components("stable", ["rustfmt", "rust-src", "llvm-tools"])?;
    check.components("nightly", ["clippy", "rust-src", "llvm-tools"])?;
    check.subcommands(["llvm-cov"])?;
    check.runner.print_summary();
    Ok(())
}

struct ToolCheck {
    runner: TrackRunner,
}
impl ToolCheck {
    fn commands<const N: usize>(&mut self, required_commands: [&str; N]) -> Result<()> {
        for cmd in required_commands {
            let installed = Command::new(cmd)
                .arg("--version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success());

            if !installed {
                bail!("cargo-full-test requires command '{cmd}' in PATH. Please install it.");
            }
        }
        Ok(())
    }

    fn toolchains<const N: usize>(&mut self, required_toolchains: [&str; N]) -> Result<()> {
        let installed_toolchains = Command::new("rustup")
            .args(["toolchain", "list"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .map(|output| {
                let stdout = String::from_utf8_lossy(&output.stdout);
                stdout
                    .lines()
                    .map(|line| line.to_string())
                    .collect::<Vec<_>>()
            })?;
        let missing_toolchains = required_toolchains
            .iter()
            .filter(|toolchain| !contains_prefix(&installed_toolchains, toolchain))
            .map(|&s| s.to_string())
            .collect::<Vec<_>>();
        if !missing_toolchains.is_empty() {
            println!(
                "The following toolchains are required but not installed: {missing_toolchains:?}"
            );
            if !confirm("Install them now?")? {
                bail!(
                    "Required toolchains not installed. Please install the following toolchains manually: {missing_toolchains:?}"
                );
            }

            let command =
                cmd!(rustup "install" "--no-self-update" {..missing_toolchains}; "rustup install"); // We can't self-update from inside a cargo subcommand
            if !self.runner.run_one(command)? {
                bail!("Failed to install missing toolchains");
            }
        }

        Ok(())
    }

    fn components<const N: usize>(
        &mut self,
        toolchain: &'static str,
        required: [&'static str; N],
    ) -> Result<()> {
        let installed_components = Command::new("rustup")
            .args(["component", "list", "--toolchain", toolchain])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .map(|output| {
                let stdout = String::from_utf8_lossy(&output.stdout);
                stdout
                    .lines()
                    .filter(|line| line.contains("installed"))
                    .map(|line| line.to_string())
                    .collect::<Vec<_>>()
            })?;

        let missing_components = required
            .iter()
            .filter(|component| !contains_prefix(&installed_components, component))
            .map(|&s| s.to_string())
            .collect::<Vec<_>>();

        if missing_components.is_empty() {
            return Ok(());
        }

        println!(
            "The following components are required but not installed for toolchain {toolchain}: {missing_components:?}"
        );
        if !confirm("Install them now?")? {
            bail!(
                "Required components not installed for toolchain {toolchain}. Please install the following components manually: {missing_components:?}"
            );
        }

        let command = cmd!(rustup "component" "add" "--toolchain" {toolchain} {..missing_components}; "add missing components");
        if !self.runner.run_one(command)? {
            bail!("Failed to install missing components for toolchain {toolchain}");
        }
        Ok(())
    }

    fn subcommands<const N: usize>(&mut self, subcommands: [&str; N]) -> Result<(), anyhow::Error> {
        let mut missing_subcommands = Vec::new();
        for subcommand in subcommands {
            let full_name = format!("cargo-{subcommand}");
            let installed = Command::new("cargo")
                .args([subcommand, "--version"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success());

            if !installed {
                missing_subcommands.push(full_name.clone());
            }
        }

        if missing_subcommands.is_empty() {
            return Ok(());
        }

        println!("Missing required tool(s): {missing_subcommands:?}");
        if !confirm("Install missing tools now?")? {
            bail!("Required tools not installed: {missing_subcommands:?}");
        }

        let command =
            cmd!(cargo +stable "install" {..missing_subcommands}; "install missing tools");
        if !self.runner.run_one(command)? {
            bail!("Failed to install missing tools");
        }

        Ok(())
    }
}

fn confirm(prompt: &str) -> std::result::Result<bool, dialoguer::Error> {
    dialoguer::Confirm::new()
        .with_prompt(prompt)
        .default(true)
        .interact()
}

fn contains_prefix(list: &[String], prefix: &str) -> bool {
    list.iter().any(|entry| entry.starts_with(prefix))
}
