#!/usr/bin/env bash
# Full verification: fmt, clippy, tests, server build, wasm client build.
# Every PR must pass this before merge.
set -euo pipefail
cd "$(dirname "$0")/.."
scripts/dev.sh cargo fmt --all --check
scripts/dev.sh cargo clippy --workspace --exclude ferraria-client -- -D warnings
scripts/dev.sh cargo clippy -p ferraria-client --target wasm32-unknown-unknown -- -D warnings
# The client also compiles natively (its pure logic carries native unit
# tests below), so that cfg path needs its own -D warnings gate.
scripts/dev.sh cargo clippy -p ferraria-client -- -D warnings
scripts/dev.sh cargo test
# The client is excluded from default-members (it ships wasm-only), but its
# pure logic (lighting) carries native unit tests.
scripts/dev.sh cargo test -p ferraria-client
scripts/dev.sh cargo build -p ferraria-client --target wasm32-unknown-unknown
echo "ALL CHECKS PASSED"
