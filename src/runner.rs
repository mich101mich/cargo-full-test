use std::{
    borrow::Cow,
    ffi::OsString,
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    process::{Command, Stdio},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use indicatif::{ProgressBar, ProgressStyle};

use crate::workspace::Workspace;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Toolchain {
    Stable,
    Nightly,
    Specific(String),
}
impl<'a> From<&'a String> for Toolchain {
    fn from(raw: &'a String) -> Self {
        Self::Specific(raw.to_owned())
    }
}
impl std::fmt::Display for Toolchain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Toolchain::Stable => write!(f, "+stable"),
            Toolchain::Nightly => write!(f, "+nightly"),
            Toolchain::Specific(version) => write!(f, "+{version}"),
        }
    }
}

#[derive(Debug)]
pub struct Shared {
    /// Directory for storing log files.
    pub log_dir: PathBuf,
    /// Directory for storing coverage output.
    pub cov_base_dir: PathBuf,
    /// Multi-progress indicator for displaying live progress bars.
    pub multi_progress: indicatif::MultiProgress,
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
        })
    }

    fn create_progress_bar(
        display_name: Cow<'static, str>,
        multi_progress: &indicatif::MultiProgress,
    ) -> ProgressBar {
        let style = ProgressStyle::with_template("{spinner:.cyan} [{prefix:.bold}] {wide_msg}")
            .unwrap_or_else(|_| ProgressStyle::default_spinner());
        let pb = multi_progress.add(ProgressBar::new_spinner());
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
    pub fn run_cmd(&self, program: &str, args: &[&str]) -> Result<()> {
        let cmd_str = if args.is_empty() {
            program.to_owned()
        } else {
            format!("{program} {}", args.join(" "))
        };

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
            self.pb.finish_and_clear();
            let mut stderr = std::io::stderr().lock();
            let _ = writeln!(
                stderr,
                "\n=== [{}] Command failed: `{cmd_str}` ===",
                self.display_name
            );
            let _ = write!(stderr, "{raw_output}");
            let _ = stderr.flush();
            bail!(
                "[{}] Command `{cmd_str}` failed with status {status}",
                self.display_name
            );
        }

        Ok(())
    }

    pub fn run_cargo(&self, toolchain: impl Into<Toolchain>, args: &[&str]) -> Result<()> {
        let plus_toolchain = toolchain.into().to_string();
        let mut cmd_args = vec![plus_toolchain.as_str()];
        cmd_args.extend_from_slice(args);
        self.run_cmd("cargo", &cmd_args)
    }

    pub fn run_rustup(&self, args: &[&str]) -> Result<()> {
        self.run_cmd("rustup", args)
    }

    /// Runs `cargo +<toolchain> llvm-cov test --lcov --output-path target/cov/<cov_name>/lcov.info <extra_args...>`.
    pub fn run_coverage(
        &self,
        toolchain: impl Into<Toolchain>,
        cov_name: &str,
        extra_args: &[&str],
    ) -> Result<()> {
        fs_err::create_dir_all(&self.shared.cov_base_dir)?;

        let output_parent = self.shared.cov_base_dir.join(cov_name);
        fs_err::create_dir_all(&output_parent)?;
        let output_path = output_parent.join("lcov.info");
        let output_path = output_path.to_string_lossy();

        let mut args = vec![
            "llvm-cov",
            "test",
            "--lcov",
            "--output-path",
            output_path.as_ref(),
        ];
        args.extend_from_slice(extra_args);

        self.run_cargo(toolchain, &args)
    }

    /// Marks the track progress spinner as finished.
    pub fn finish(&self, msg: &str) {
        self.pb.finish_with_message(msg.to_owned());
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
