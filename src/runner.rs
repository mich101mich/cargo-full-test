use std::{
    borrow::Cow,
    ffi::OsString,
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};

use crate::{
    args::{Args, Instruction},
    workspace::Workspace,
};

#[derive(Debug)]
pub struct Shared {
    /// Base directory of the project.
    pub base_dir: PathBuf,
    /// Directory for storing log files.
    pub log_dir: PathBuf,
    /// Directory for storing coverage output.
    pub cov_base_dir: PathBuf,
    /// Multi-progress indicator for displaying live progress bars.
    pub multi_progress: indicatif::MultiProgress,
    /// Mutex for synchronizing fetch operations.
    ///
    /// Cargo locks the package registry in a way that deadlocks between MSRV and current cargo.
    pub fetch_mutex: Mutex<()>,
}

/// Execution context for a single track (e.g. `Setup`, `Base`, `MSRV`, `MinVersions`).
#[derive(Debug)]
pub struct TrackRunner {
    /// Display name for the track (e.g., `"Base"`).
    pub display_name: Cow<'static, str>,
    /// Directories associated with this track.
    pub workspace: Workspace,
    /// Shared information.
    pub shared: Arc<Shared>,
    /// Path to the track's log file (`<base_dir>/target/test_<track_id>.log`).
    pub log_path: PathBuf,
    /// Environment variables to set on every command in this track.
    pub env_set: Vec<(String, OsString)>,
    /// Live progress bar for this track.
    pub pb: ProgressBar,
    /// Commands that were executed in this track.
    pub commands: Vec<String>,
    /// Output of a failed command, if any.
    pub failed_output: Option<String>,
}

impl TrackRunner {
    /// Creates a new [`TrackRunner`] attached to `multi_progress`.
    pub fn new(
        track_id: &str,
        display_name: impl Into<Cow<'static, str>>,
        shared: Arc<Shared>,
        workspace: Workspace,
    ) -> Result<Self> {
        fs_err::create_dir_all(&shared.log_dir)?;
        let log_path = shared.log_dir.join(format!("test_{track_id}.log"));
        let display_name = display_name.into();

        let pb = Self::create_progress_bar(display_name.clone(), &shared.multi_progress);

        let env_set = vec![(
            "CARGO_TARGET_DIR".into(),
            workspace.cargo_target_dir.clone().into(),
        )];

        Ok(Self {
            display_name,
            workspace,
            shared,
            log_path,
            env_set,
            pb,
            commands: Vec::new(),
            failed_output: None,
        })
    }

    fn create_progress_bar(
        display_name: Cow<'static, str>,
        multi_progress: &indicatif::MultiProgress,
    ) -> ProgressBar {
        let style = ProgressStyle::with_template(
            "[{prefix:12.bold}] {bar:10.cyan/blue} {pos}/{len} {wide_msg}",
        )
        .unwrap();
        let pb = multi_progress.add(ProgressBar::new(0));
        pb.set_style(style);
        pb.set_prefix(display_name);
        pb.enable_steady_tick(std::time::Duration::from_millis(100));
        pb
    }

    /// Sets an environment variable for commands in this track.
    pub fn set_env(&mut self, key: impl Into<String>, val: impl Into<OsString>) {
        let key = key.into();
        let val = val.into();
        if let Some((_, existing)) = self.env_set.iter_mut().find(|(k, _)| k == &key) {
            *existing = val;
        } else {
            self.env_set.push((key, val));
        }
    }

    /// Runs a command in this track's working directory, logging raw output to `self.log_path`
    /// and updating the live spinner with the most recent output line.
    pub fn run_cmd(&mut self, program: &str, args: Args) -> Result<bool> {
        let mut cmd_str = program.to_owned();
        args.write_to(&mut cmd_str, &self.shared.base_dir);

        self.commands.push(cmd_str.clone());

        let short_cmd_str = if cmd_str.len() > 40 {
            format!("{}..", &cmd_str[..38])
        } else {
            cmd_str.clone()
        };

        self.pb.set_message(format!("Running {cmd_str}"));

        let mut log_file = fs_err::File::create(&self.log_path)?;
        let log_path_name = self.log_path.display();
        writeln!(
            log_file,
            r#"################################################################################
### Log for {cmd_str}
### This file is meant to be output to a terminal, so it still contains escape
### sequences for coloring. If you want to read the log, use
###     tail -n +7 "{log_path_name}"
################################################################################"#
        )?;

        let (reader, writer) = std::io::pipe().context("Failed to create OS pipe")?;
        let writer_err = writer
            .try_clone()
            .context("Failed to clone OS pipe writer")?;

        let mut cmd = Command::new(program);
        cmd.args(args)
            .current_dir(&self.workspace.work_dir)
            .env("CARGO_TERM_COLOR", "always")
            .env("CARGO_CACHE_AUTO_CLEAN_FREQUENCY", "never") // avoid package lock from GC
            .stdout(Stdio::from(writer))
            .stderr(Stdio::from(writer_err));

        for (key, val) in &self.env_set {
            cmd.env(key, val);
        }

        let mut child = cmd
            .spawn()
            .with_context(|| format!("Failed to spawn `{cmd_str}`"))?;
        // Drop the parent's copies of the pipe writers by dropping `cmd` so EOF is reached when child exits.
        drop(cmd);

        let mut buf_reader = BufReader::new(reader);
        let mut line_buf = Vec::new();
        let mut raw_output = String::new();

        loop {
            line_buf.clear();
            let bytes_read = buf_reader
                .read_until(b'\n', &mut line_buf)
                .context("Failed reading subprocess output")?;
            if bytes_read == 0 {
                break;
            }

            log_file.write_all(&line_buf)?;
            let line_lossy = String::from_utf8_lossy(&line_buf);
            raw_output.push_str(&line_lossy);

            let stripped = strip_ansi_codes(&line_lossy);
            let trimmed = stripped.trim();
            if !trimmed.is_empty() && !trimmed.starts_with("Blocking waiting for file lock") {
                self.pb
                    .set_message(format!("Running {short_cmd_str} > {trimmed}"));
            }
        }

        let status = child
            .wait()
            .with_context(|| format!("Failed waiting for `{cmd_str}`"))?;

        if !status.success() {
            self.failed_output = Some(format!(
                r#"        Failed with status {status}
========== output ==========
{raw_output}
============================
"#
            ));
            return Ok(false);
        }

        Ok(true)
    }

    pub fn print_summary(self) {
        self.pb.finish_and_clear();

        self.pb.suspend(|| {
            let outcome = if self.failed_output.is_some() {
                "failed"
            } else {
                "success"
            };
            println!(
                "[{:12}] ({outcome})",
                console::style(&self.display_name).bold()
            );

            for cmd in &self.commands {
                println!("    {}", cmd);
            }
            if let Some(failed_output) = &self.failed_output {
                println!("{failed_output}");
            }
        });
    }

    pub(crate) fn run_instructions(
        mut self,
        instructions: impl Into<Vec<Instruction>>,
    ) -> Result<bool> {
        let instructions = instructions.into();
        let total_commands = instructions
            .iter()
            .filter(|i| matches!(i, Instruction::Command { .. }))
            .count();
        self.pb.set_length(total_commands as u64);

        let mut commands_run = 0;
        self.pb.set_position(commands_run as u64);

        let shared = Arc::clone(&self.shared);
        let mut lock = None;

        let mut result = true;

        for instruction in instructions {
            match instruction {
                Instruction::Command { program, args } => {
                    if !self.run_cmd(program, args)? {
                        result = false;
                    }
                    commands_run += 1;
                    self.pb.set_position(commands_run as u64);
                }
                Instruction::SetEnv { key, value } => {
                    self.set_env(key, value);
                }
                Instruction::TakeMutex => {
                    self.pb.set_message("Waiting for global fetch lock");
                    lock = Some(shared.fetch_mutex.lock().unwrap());
                }
                Instruction::ReleaseMutex => {
                    drop(lock.take());
                }
            }
        }

        self.print_summary();

        Ok(result)
    }
}

/// Strips ANSI CSI/OSC escape sequences and control characters from a single line
/// so that it can be safely displayed inside an `indicatif` progress message.
pub fn strip_ansi_codes(input: &str) -> String {
    console::strip_ansi_codes(input)
        .chars()
        .filter(|c| !c.is_control())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_ansi_escape_sequences() {
        let colored = "\x1b[1;32m   Compiling\x1b[0m foo v0.1.0\r\n";
        assert_eq!(strip_ansi_codes(colored).trim(), "Compiling foo v0.1.0");
    }
}
