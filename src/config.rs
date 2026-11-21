use std::path::PathBuf;

use anyhow::{Context, Result};
use cargo_metadata::{Metadata, MetadataCommand};
use serde::{Deserialize, Deserializer};

use crate::CliArgs;

/// Per-crate or workspace settings deserialized from `[package.metadata.full-test]`
/// or `[workspace.metadata.full_test]`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FullTestMetadata {
    /// Specifies the **extra** Rust toolchains to test against. Default: `["beta", "nightly"]`.
    ///
    /// Note that `stable` and your MSRV will always be tested and does not need to be
    /// included here. So, if you specify e.g. `["beta"]`, this crate will
    /// test `stable`, `<MSRV>`, and `beta`.
    ///
    /// The names should be valid Rust toolchain identifiers (e.g., `"1.88.0"`, `"nightly-2026-01-01"`).
    ///
    /// It is recommended to have at least nightly included in this list.
    #[serde(default = "default_extra_toolchains", alias = "extra-toolchains")]
    pub extra_toolchains: Vec<String>,

    /// Whether to collect coverage information during tests. Default: `true`.
    #[serde(default = "default_run_coverage", alias = "run-coverage")]
    pub run_coverage: bool,

    /// Specifies the name of the (normally ignored) test that runs [`err_span_check`](https://crates.io/crates/err_span_check).
    #[serde(alias = "err-span-check-test")]
    pub err_span_check_test: Option<String>,

    /// List of `"package@version"` overrides applied during the MSRV test step.
    ///
    /// Versions are applied via `cargo +<MSRV> update -p <package> --precise <version>`.
    #[serde(default, alias = "msrv-overrides")]
    pub msrv_overrides: Vec<MsrvOverride>,
}

fn default_extra_toolchains() -> Vec<String> {
    vec!["beta".to_string(), "nightly".to_string()]
}

fn default_run_coverage() -> bool {
    true
}

/// A parsed `"package@version"` override for MSRV dependency pinning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MsrvOverride {
    /// Package name (`-p <package>`).
    pub package: String,
    /// Precise version (`--precise <version>`).
    pub version: String,
}

impl<'de> Deserialize<'de> for MsrvOverride {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        let Some((pkg, ver)) = s.split_once('@') else {
            return Err(serde::de::Error::custom(format!(
                "Invalid msrv-overrides entry '{s}': expected format 'package@version'"
            )));
        };
        let pkg = pkg.trim();
        let ver = ver.trim();
        if pkg.is_empty() || ver.is_empty() {
            return Err(serde::de::Error::custom(format!(
                "Invalid msrv-overrides entry '{s}': expected non-empty 'package@version'"
            )));
        }
        Ok(Self {
            package: pkg.to_owned(),
            version: ver.to_owned(),
        })
    }
}

pub enum TestType {
    Base,
    Msrv,
    MinVersions,
    ErrSpanCheck,
}

/// Resolved configuration for running the full test suite on a crate/workspace.
#[derive(Debug, Clone)]
pub struct Config {
    /// Root directory of the workspace/crate.
    pub base_dir: PathBuf,
    /// Minimum Supported Rust Version (e.g., `"1.88.0"`).
    pub msrv: String,
    /// Options parsed from toml metadata
    pub options: FullTestMetadata,
    /// Parsed CLI arguments.
    pub args: CliArgs,
}

impl Config {
    /// Loads configuration from the Cargo workspace in the current directory (or `manifest_path`).
    pub fn load(args: CliArgs) -> Result<Self> {
        let metadata = MetadataCommand::new()
            .no_deps()
            .exec()
            .context("Failed to execute `cargo metadata --no-deps`")?;
        Self::from_metadata(&metadata, args)
    }

    pub fn has_nightly(&self) -> bool {
        self.options
            .extra_toolchains
            .iter()
            .any(|tc| tc == "nightly")
    }

    pub fn has_track_selection(&self) -> bool {
        self.args.track.base
            || self.args.track.msrv
            || self.args.track.min_versions
            || self.args.track.err_span_check
    }

    pub fn should_run(&self, test_type: TestType) -> bool {
        if !self.has_track_selection() {
            return true;
        }
        match test_type {
            TestType::Base => self.args.track.base,
            TestType::Msrv => self.args.track.msrv,
            TestType::MinVersions => self.args.track.min_versions,
            TestType::ErrSpanCheck => self.args.track.err_span_check,
        }
    }

    /// Constructs a [`Config`] from an already-loaded [`Metadata`].
    fn from_metadata(metadata: &Metadata, args: CliArgs) -> Result<Self> {
        let base_dir = metadata.workspace_root.clone().into_std_path_buf();

        let root_package = metadata
            .root_package()
            .or_else(|| metadata.workspace_default_packages().into_iter().next())
            .or_else(|| metadata.workspace_packages().into_iter().next())
            .context("Failed to find a package in the Cargo workspace")?;

        let msrv = root_package
            .rust_version
            .as_ref()
            .context("Missing rust-version (MSRV) in Cargo.toml. cargo-full-test requires specifying an MSRV")?
            .to_string();

        let raw_meta = root_package
            .metadata
            .get("full-test")
            .or_else(|| root_package.metadata.get("full_test"))
            .or_else(|| metadata.workspace_metadata.get("full-test"))
            .or_else(|| metadata.workspace_metadata.get("full_test"));

        let parsed_meta: FullTestMetadata = match raw_meta {
            Some(val) => serde_json::from_value(val.clone())
                .context("Failed to parse [package.metadata.full-test] in Cargo.toml")?,
            None => {
                // we want to reuse serde's default values for FullTestMetadata
                serde_json::from_value(serde_json::json!({}))
                    .expect("Failed to create default FullTestMetadata")
            }
        };

        Ok(Self {
            base_dir,
            msrv,
            options: parsed_meta,
            args,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_test_metadata_kebab_and_snake() {
        let json_kebab = serde_json::json!({
            "err-span-check-test": "test_name",
            "msrv-overrides": ["trybuild@1.0.109", "toml@0.9.6"]
        });
        let meta_kebab: FullTestMetadata = serde_json::from_value(json_kebab).unwrap();
        assert_eq!(
            meta_kebab.err_span_check_test,
            Some("test_name".to_string())
        );
        assert_eq!(
            meta_kebab.msrv_overrides,
            vec![
                MsrvOverride {
                    package: "trybuild".to_string(),
                    version: "1.0.109".to_string(),
                },
                MsrvOverride {
                    package: "toml".to_string(),
                    version: "0.9.6".to_string(),
                }
            ]
        );

        let json_snake = serde_json::json!({
            "err_span_check_test": "test_name",
            "msrv_overrides": ["serde_spanned@1.0.1"]
        });
        let meta_snake: FullTestMetadata = serde_json::from_value(json_snake).unwrap();
        assert_eq!(
            meta_snake.err_span_check_test,
            Some("test_name".to_string())
        );
        assert_eq!(
            meta_snake.msrv_overrides,
            vec![MsrvOverride {
                package: "serde_spanned".to_string(),
                version: "1.0.1".to_string(),
            }]
        );
    }

    #[test]
    fn parse_msrv_override_validation() {
        fn parse(s: &str) -> Result<MsrvOverride, serde_json::Error> {
            serde_json::from_str::<MsrvOverride>(&format!("\"{}\"", s))
        }
        let valid = parse("trybuild@1.0.109").unwrap();
        assert_eq!(valid.package, "trybuild");
        assert_eq!(valid.version, "1.0.109");

        assert!(parse("missing_at_sign").is_err());
        assert!(parse("@1.0.0").is_err());
        assert!(parse("pkg@").is_err());
    }

    #[test]
    #[expect(clippy::bool_assert_comparison, reason = "it looks better this way")]
    fn parse_full_test_metadata_default() {
        let json_empty = serde_json::json!({});
        let meta: FullTestMetadata = serde_json::from_value(json_empty).unwrap();
        assert_eq!(meta.err_span_check_test, None);
        assert_eq!(meta.msrv_overrides, []);
        assert_eq!(meta.run_coverage, true);
        assert_eq!(meta.extra_toolchains, ["beta", "nightly"]);
    }
}
