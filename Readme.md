# cargo-full-test

A subcommand for Rust's package manager, Cargo to run full test, lint, coverage, MSRV, and minimal-versions.

## Features

Runs all of these steps:

```text
C:\My-Awesome-Rust-Project> cargo full-test
[Setup       ] (success)
    rustup update
    rustup install stable nightly 1.88.0
    rustup component add --toolchain stable rustfmt rust-src llvm-tools-preview
    rustup component add --toolchain nightly clippy rust-src llvm-tools-preview
    cargo +stable install cargo-llvm-cov
[Base        ] (success)
    cargo +stable update
    cargo +stable fetch
    cargo +stable test --workspace
    cargo +nightly llvm-cov test --lcov --output-path target\cov\nightly\lcov.info --workspace
    cargo +nightly doc --no-deps --workspace
    cargo +nightly clippy --workspace -- -D warnings
    cargo +stable fmt --check --all
[MSRV 1.88.0 ] (success)
    cargo +1.88.0 update
    cargo +1.88.0 fetch
    cargo +1.88.0 test --workspace
[MinVersions ] (success)
    cargo +nightly -Z minimal-versions update
    cargo +stable fetch
    cargo +stable test --workspace
    cargo +nightly test --workspace
All tests passed in 19 seconds!
```

- **Setup**: Automatically installs the necessary Rust toolchains and components, and sets up the environment for testing.
- **Base**: Runs the standard tests, coverage, documentation, linting, and formatting checks on the stable and nightly toolchains.
- **MSRV**: Tests the project with the Minimum Supported Rust Version (MSRV) to ensure compatibility. MSRV is auto-detected from the `rust-version` field in the `Cargo.toml`.
- **MinVersions**: Tests the project with minimal versions of dependencies to ensure that the semver bounds are actually correct.

This will ensure that your crate always fulfills the requirements of a high quality Rust project!

Additionally:
- Uses multi-`Cargo.lock` caching to make any non-clean run as fast as possible.
- Supports both Windows and Unix-like systems for running the full test suite.
- Integrates with [`err_span_check`] to provide separate running of error message checks with coverage.

## Drawbacks

- Uses symlinks, so Windows users may need to enable Developer Mode.
- Significantly increases the size of the target directory.

## Configuration

To configure `cargo-full-test`, add a `[package.metadata.full-test]` section to your `Cargo.toml` file. For example:

```toml
[package.metadata.full-test]
msrv_overrides = ["some-other-crate@1.2.3"]
err_span_check_test = "error_message_tests"
```

### MSRV Overrides

When the dependency of a dependency inevitably breaks your MSRV, you can use the `msrv_overrides` option to pin the
crate to a specific version specifically for MSRV testing. For example:

```toml
msrv_overrides = ["some-other-crate@1.2.3"]
```

will cause `cargo-full-test` to run

```bash
cargo +<MSRV> update --package some-other-crate --precise 1.2.3
```

before running the MSRV tests.

### Error Span Check

When using [`err_span_check`], it is common to have the error message test ignored by default. The `err_span_check_test`
option allows you to specify the name of that test, causing `cargo-full-test` to run it separately with coverage
on both the stable and nightly toolchains.


[`err_span_check`]: https://crates.io/crates/err_span_check
