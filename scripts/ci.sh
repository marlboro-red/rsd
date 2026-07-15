#!/bin/bash
# rsd CI gate: fmt, clippy (deny warnings), full test suite.
# The convergence harness (P1.6) and crash-injection suite (P2.4) run as part of
# `cargo test` and are permanent gates — see IMPLEMENTATION.md working agreements.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "==> cargo fmt --check"
cargo fmt --all --check

echo "==> cargo clippy"
cargo clippy --workspace --all-targets -- -D warnings

echo "==> cargo test"
cargo test --workspace

echo "CI gate: OK"
