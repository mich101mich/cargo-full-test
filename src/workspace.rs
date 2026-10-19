use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Workspace {
    /// Working directory for Cargo commands.
    pub work_dir: PathBuf,
    /// Target directory for Cargo commands.
    pub cargo_target_dir: PathBuf,
}

impl Workspace {
    /// Creates a new `Workspace` representing the base directory.
    pub fn new_base_dir(base_dir: &Path) -> Self {
        let cargo_target_dir = base_dir.join("target");
        Self {
            work_dir: base_dir.to_path_buf(),
            cargo_target_dir,
        }
    }

    /// Prepares an isolated test directory in `<base_dir>/target/<out_dir_name>` by symlinking
    /// all top-level entries from `base_dir` except `target` and `Cargo.lock`.
    pub fn new_isolated(base_dir: &Path, out_dir_name: &str) -> Result<Self> {
        let work_dir = base_dir.join("target").join(out_dir_name);
        fs_err::create_dir_all(&work_dir)?;

        for entry in fs_err::read_dir(base_dir)? {
            let entry = entry?;
            let file_name = entry.file_name();
            if file_name == "target" || file_name == "Cargo.lock" || file_name == ".git" {
                continue;
            }

            let src_path = entry.path();
            let dst = work_dir.join(&file_name);

            if fs_err::read_link(&dst).is_ok_and(|existing_target| existing_target == src_path) {
                continue;
            }

            remove_existing_entry(&dst)?;
            create_symlink(&src_path, &dst)?;
        }

        let cargo_target_dir = work_dir.join("target");
        Ok(Workspace {
            work_dir,
            cargo_target_dir,
        })
    }
}

fn remove_existing_entry(dst: &Path) -> Result<()> {
    let Ok(meta) = fs_err::symlink_metadata(dst) else {
        return Ok(());
    };

    // On Windows, directory symlinks have `FileType::is_symlink_dir()` (or `is_dir()` on some APIs)
    // and must be removed with `remove_dir`. Try `remove_file` first, falling back to `remove_dir`
    // (or `remove_dir_all` if a real non-symlink directory ever existed at `dst`).
    if meta.file_type().is_dir() && !meta.file_type().is_symlink() {
        fs_err::remove_dir_all(dst)?;
    } else if std::fs::remove_file(dst).is_err() {
        fs_err::remove_dir(dst)?;
    }

    Ok(())
}

#[cfg(windows)]
fn create_symlink(src_path: &Path, dst: &Path) -> Result<()> {
    let res = if src_path.is_dir() {
        std::os::windows::fs::symlink_dir(src_path, dst)
    } else {
        std::os::windows::fs::symlink_file(src_path, dst)
    };

    res.with_context(|| {
        format!(
            "Failed to create symlink from '{}' to '{}'. Ensure Windows Developer Mode is enabled.",
            dst.display(),
            src_path.display()
        )
    })
}

#[cfg(unix)]
fn create_symlink(src_path: &Path, dst: &Path) -> Result<()> {
    std::os::unix::fs::symlink(src_path, dst).with_context(|| {
        format!(
            "Failed to create symlink from '{}' to '{}'",
            dst.display(),
            src_path.display()
        )
    })
}
