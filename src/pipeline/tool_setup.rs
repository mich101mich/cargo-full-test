use std::process::{Command, Stdio};

use crate::{
    config::Config,
    instruction::{Instruction, args},
};

pub(crate) fn setup_tools(config: &Config) -> Vec<Instruction> {
    let mut toolchains = vec!["stable", &config.msrv];
    toolchains.extend(config.options.extra_toolchains.iter().map(|s| s.as_str()));
    toolchains.sort_unstable();
    toolchains.dedup();

    let has_nightly = toolchains.contains(&"nightly");

    let mut stable_components = vec!["rustfmt"];
    let mut nightly_components = vec![];
    let mut subcommands = vec![];

    if has_nightly {
        nightly_components.push("clippy");
    } else {
        stable_components.push("clippy");
    }

    if config.options.run_coverage {
        stable_components.push("llvm-tools");
        nightly_components.push("llvm-tools");
        subcommands.push("cargo-llvm-cov");
    }

    if config.options.err_span_check_test.is_some() {
        stable_components.push("rust-src");
        nightly_components.push("rust-src");
    }

    let mut instructions = Vec::new();
    instructions.push(check_commands(["rustup", "cargo"]));
    instructions.push(check_toolchains(toolchains));
    if !stable_components.is_empty() {
        instructions.push(check_components("stable", stable_components));
    }
    if has_nightly && !nightly_components.is_empty() {
        instructions.push(check_components("nightly", nightly_components));
    }
    if !subcommands.is_empty() {
        instructions.push(check_subcommands(subcommands));
    }

    instructions
}

fn check_commands<'a>(required: impl IntoIterator<Item = &'a str>) -> Instruction {
    let remove_installed = |required: &mut Vec<String>| {
        required.retain(|cmd| {
            let installed = Command::new(cmd)
                .arg("--version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            !installed
        });
        Ok(())
    };
    Instruction::tool_check("commands", to_string_vec(required), remove_installed, None)
}

fn check_toolchains<'a>(required: impl IntoIterator<Item = &'a str>) -> Instruction {
    let remove_installed = |required: &mut Vec<String>| {
        let installed_toolchains = Command::new("rustup")
            .args(["toolchain", "list"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .map(|output| to_string_vec(String::from_utf8_lossy(&output.stdout).lines()))?;
        required.retain(|toolchain| !contains_prefix(&installed_toolchains, toolchain));
        Ok(())
    };

    Instruction::tool_check(
        "toolchains",
        to_string_vec(required),
        remove_installed,
        ("rustup", args!("install" "--no-self-update")),
    )
}

fn check_components<'a>(
    toolchain: &'static str,
    required: impl IntoIterator<Item = &'a str>,
) -> Instruction {
    let remove_installed_components = move |required: &mut Vec<String>| {
        let installed_components = Command::new("rustup")
            .args(["component", "list", "--toolchain", toolchain])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .map(|output| {
                let stdout = String::from_utf8_lossy(&output.stdout);
                to_string_vec(stdout.lines().filter(|line| line.contains("installed")))
            })?;

        required.retain(|component| !contains_prefix(&installed_components, component));
        Ok(())
    };

    Instruction::tool_check(
        format!("{toolchain} components"),
        to_string_vec(required),
        remove_installed_components,
        ("rustup", args!("component" "add" "--toolchain" {toolchain})),
    )
}

fn check_subcommands<'a>(subcommands: impl IntoIterator<Item = &'a str>) -> Instruction {
    let remove_installed = move |required: &mut Vec<String>| {
        required.retain(|name| {
            let subcommand = name.strip_prefix("cargo-").unwrap_or(name);
            let installed = Command::new("cargo")
                .args([subcommand, "--version"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            !installed
        });
        Ok(())
    };
    Instruction::tool_check(
        "subcommands",
        to_string_vec(subcommands),
        remove_installed,
        ("cargo", args!("install")),
    )
}

fn contains_prefix(list: &[String], prefix: &str) -> bool {
    list.iter().any(|entry| entry.starts_with(prefix))
}

fn to_string_vec<'a>(iter: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    iter.into_iter().map(|s| s.to_string()).collect()
}
