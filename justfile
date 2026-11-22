# rustfmt.toml uses nightly-only options, so formatting runs on
# nightly. Everything else uses the stable toolchain that
# rust-toolchain.toml pins.

# List the recipes.
default:
    @just --list

# Format every crate.
fmt:
    cargo +nightly fmt --all

# Fail if any file is not formatted.
fmt-check:
    cargo +nightly fmt --all --check

# Lint every crate and target, with warnings as errors.
lint:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

# Run the tests of every crate.
test:
    cargo nextest run --workspace

# Measure test coverage of every crate.
cov:
    cargo llvm-cov nextest --workspace

# Run the format check, the lints and the tests.
check: fmt-check lint test
