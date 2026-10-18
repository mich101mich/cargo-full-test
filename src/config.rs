use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use cargo_metadata::{DependencyKind, Metadata, MetadataCommand};
use serde::Deserialize;

/// Per-crate or workspace settings deserialized from `[package.metadata.full-test]`
/// or `[workspace.metadata.full-test]`.
#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct FullTestMetadata {
    /// Override whether `error_message_tests` is run.
    /// If omitted, auto-detected from `err_span_check` in dev-dependencies.
    #[serde(alias = "error_message_tests")]
    pub error_message_tests: Option<bool>,

    /// List of `"package@version"` overrides applied during the MSRV test step
    /// via `cargo +<MSRV> update -p <package> --precise <version>`.
    #[serde(default, alias = "msrv_overrides")]
    pub msrv_overrides: Vec<String>,
}

/// A parsed `"package@version"` override for MSRV dependency pinning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MsrvOverride {
    /// Package name (`-p <package>`).
    pub package: String,
    /// Precise version (`--precise <version>`).
    pub version: String,
}

impl MsrvOverride {
    /// Parses a `"package@version"` string into an [`MsrvOverride`].
    pub fn parse(raw: &str) -> Result<Self> {
        let Some((pkg, ver)) = raw.split_once('@') else {
            bail!("Invalid msrv-overrides entry '{raw}': expected format 'package@version'");
        };
        let pkg = pkg.trim();
        let ver = ver.trim();
        if pkg.is_empty() || ver.is_empty() || ver.contains('@') {
            bail!("Invalid msrv-overrides entry '{raw}': expected non-empty 'package@version'");
        }
        Ok(Self {
            package: pkg.to_owned(),
            version: ver.to_owned(),
        })
    }
}

/// Resolved configuration for running the full test suite on a crate/workspace.
#[derive(Debug, Clone)]
pub struct Config {
    /// Root directory of the workspace/crate.
    pub base_dir: PathBuf,
    /// Minimum Supported Rust Version (e.g., `"1.88.0"`).
    pub msrv: String,
    /// Whether to run `error_message_tests` on stable and nightly.
    pub run_error_message_tests: bool,
    /// Dependency version overrides for MSRV testing.
    pub msrv_overrides: Vec<MsrvOverride>,
    /// Whether `--frozen` was passed on the CLI (`ERR_SPAN_CHECK=frozen`).
    pub frozen: bool,
}

impl Config {
    /// Loads configuration from the Cargo workspace in the current directory (or `manifest_path`).
    pub fn load(manifest_path: Option<&Path>, frozen: bool) -> Result<Self> {
        let mut cmd = MetadataCommand::new();
        cmd.no_deps();
        if let Some(path) = manifest_path {
            cmd.manifest_path(path);
        }
        let metadata = cmd
            .exec()
            .context("Failed to execute `cargo metadata --no-deps`")?;
        Self::from_metadata(&metadata, frozen)
    }

    /// Constructs a [`Config`] from an already-loaded [`Metadata`].
    pub fn from_metadata(metadata: &Metadata, frozen: bool) -> Result<Self> {
        let base_dir = metadata.workspace_root.clone().into_std_path_buf();

        let root_package = metadata
            .root_package()
            .or_else(|| metadata.workspace_default_packages().into_iter().next())
            .or_else(|| metadata.workspace_packages().into_iter().next())
            .context("Failed to find a package in the Cargo workspace")?;

        let msrv = root_package
            .rust_version
            .as_ref()
            .map(ToString::to_string)
            .context("Failed to read rust-version (MSRV) from Cargo.toml")?;

        let raw_meta = root_package
            .metadata
            .get("full-test")
            .or_else(|| root_package.metadata.get("full_test"))
            .or_else(|| metadata.workspace_metadata.get("full-test"))
            .or_else(|| metadata.workspace_metadata.get("full_test"));

        let parsed_meta: FullTestMetadata = match raw_meta {
            Some(val) => serde_json::from_value(val.clone())
                .context("Failed to parse [package.metadata.full-test] in Cargo.toml")?,
            None => FullTestMetadata::default(),
        };

        let msrv_overrides = parsed_meta
            .msrv_overrides
            .iter()
            .map(|s| MsrvOverride::parse(s))
            .collect::<Result<Vec<_>>>()?;

        let has_err_span_check_dev_dep = root_package
            .dependencies
            .iter()
            .any(|dep| dep.name == "err_span_check" && dep.kind == DependencyKind::Development);

        let run_error_message_tests = parsed_meta
            .error_message_tests
            .unwrap_or(has_err_span_check_dev_dep);

        Ok(Self {
            base_dir,
            msrv,
            run_error_message_tests,
            msrv_overrides,
            frozen,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_test_metadata_kebab_and_snake() {
        let json_kebab = serde_json::json!({
            "error-message-tests": true,
            "msrv-overrides": ["trybuild@1.0.109", "toml@0.9.6"]
        });
        let meta_kebab: FullTestMetadata = serde_json::from_value(json_kebab).unwrap();
        assert_eq!(meta_kebab.error_message_tests, Some(true));
        assert_eq!(
            meta_kebab.msrv_overrides,
            vec!["trybuild@1.0.109", "toml@0.9.6"]
        );

        let json_snake = serde_json::json!({
            "error_message_tests": false,
            "msrv_overrides": ["serde_spanned@1.0.1"]
        });
        let meta_snake: FullTestMetadata = serde_json::from_value(json_snake).unwrap();
        assert_eq!(meta_snake.error_message_tests, Some(false));
        assert_eq!(meta_snake.msrv_overrides, vec!["serde_spanned@1.0.1"]);
    }

    #[test]
    fn parse_msrv_override_validation() {
        let valid = MsrvOverride::parse("trybuild@1.0.109").unwrap();
        assert_eq!(valid.package, "trybuild");
        assert_eq!(valid.version, "1.0.109");

        assert!(MsrvOverride::parse("missing_at_sign").is_err());
        assert!(MsrvOverride::parse("@1.0.0").is_err());
        assert!(MsrvOverride::parse("pkg@").is_err());
        assert!(MsrvOverride::parse("pkg@1.0@2.0").is_err());
    }

    #[test]
    fn detect_err_span_check_and_sscanf_manifests() {
        let err_span_check_manifest = Path::new(
            r"c:\Users\mich101mich\Desktop\Programmieren\git\rs\OwnCrates\err_span_check\Cargo.toml",
        );
        if err_span_check_manifest.exists() {
            let cfg = Config::load(Some(err_span_check_manifest), false).unwrap();
            assert_eq!(cfg.msrv, "1.88.0");
            assert!(!cfg.run_error_message_tests);
        }

        let sscanf_manifest = Path::new(
            r"c:\Users\mich101mich\Desktop\Programmieren\git\rs\OwnCrates\sscanf\Cargo.toml",
        );
        if sscanf_manifest.exists() {
            let cfg = Config::load(Some(sscanf_manifest), true).unwrap();
            assert_eq!(cfg.msrv, "1.88.0");
            assert!(cfg.run_error_message_tests);
            assert!(cfg.frozen);
        }
    }
}
