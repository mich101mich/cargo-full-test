use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::{
    ffi::OsString,
    io::{BufRead, BufReader, Write},
};

use anyhow::{Context, Result, bail};
use indicatif::ProgressBar;

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

/// Execution context for a single track (e.g. `Setup`, `Base`, `MSRV`, `MinVersions`).
#[derive(Debug)]
pub struct TrackRunner {
    /// Display name for the track (e.g., `"Base"`).
    pub display_name: String,
    /// Root workspace directory (`<base_dir>`).
    pub base_dir: PathBuf,
    /// Working directory where commands are executed.
    pub work_dir: PathBuf,
    /// Path to the track's log file (`<base_dir>/target/test_<track_id>.log`).
    pub log_path: PathBuf,
    /// Environment variables to set on every command in this track.
    pub env_set: Vec<(String, OsString)>,
    /// Environment variables to remove on every command in this track.
    pub env_remove: Vec<String>,
    /// Live progress bar for this track.
    pub pb: ProgressBar,
    /// Shared cancellation flag across tracks.
    pub cancelled: Arc<AtomicBool>,
}

impl TrackRunner {
    /// Creates a new [`TrackRunner`] attached to `multi_progress`.
    pub fn new(
        track_id: &str,
        display_name: &str,
        base_dir: &Path,
        work_dir: &Path,
        pb: ProgressBar,
        cancelled: Arc<AtomicBool>,
    ) -> Result<Self> {
        let target_dir = base_dir.join("target");
        fs_err::create_dir_all(&target_dir)?;
        let log_path = target_dir.join(format!("test_{track_id}.log"));

        Ok(Self {
            display_name: display_name.to_owned(),
            base_dir: base_dir.to_path_buf(),
            work_dir: work_dir.to_path_buf(),
            log_path,
            env_set: Vec::new(),
            env_remove: Vec::new(),
            pb,
            cancelled,
        })
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

    /// Removes an environment variable for commands in this track.
    pub fn remove_env(&mut self, key: impl Into<String>) {
        self.env_remove.push(key.into());
    }

    /// Runs a command in this track's working directory, logging raw output to `self.log_path`
    /// and updating the live spinner with the most recent output line.
    pub fn run_cmd(&self, program: &str, args: &[&str]) -> Result<()> {
        if self.cancelled.load(Ordering::Relaxed) {
            bail!(
                "[{}] Cancelled due to failure in another track",
                self.display_name
            );
        }

        let cmd_str = if args.is_empty() {
            program.to_owned()
        } else {
            format!("{program} {}", args.join(" "))
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
            .current_dir(&self.work_dir)
            .env("CARGO_TERM_COLOR", "always")
            .env("CARGO_CACHE_AUTO_CLEAN_FREQUENCY", "never") // avoid package lock from GC
            .stdout(Stdio::from(writer))
            .stderr(Stdio::from(writer_err));

        for key in &self.env_remove {
            cmd.env_remove(key);
        }
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
                    .set_message(format!("Running {cmd_str} > {trimmed}"));
            }
        }

        let status = child
            .wait()
            .with_context(|| format!("Failed waiting for `{cmd_str}`"))?;

        if !status.success() {
            self.cancelled.store(true, Ordering::Relaxed);
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
        let cov_dir = self.base_dir.join("target").join("cov").join(cov_name);
        fs_err::create_dir_all(&cov_dir)?;

        let output_path = format!("target/cov/{cov_name}/lcov.info");

        let mut args = vec![
            "llvm-cov",
            "test",
            "--lcov",
            "--output-path",
            output_path.as_str(),
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
