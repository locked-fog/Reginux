#!/usr/bin/env bash
set -euo pipefail

project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project_root"

cargo fmt --all -- --check
cargo check --locked --workspace
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets -- -D warnings

bash -n scripts/install-local.sh

# Exercise the real command and Lua syntax used by the examples through the
# core test suite; manifests are parsed by plugin unit tests.
/usr/bin/date '+[{"id":"local","current":"%FT%T%:z"}]' >/dev/null

printf '%s\n' "All Rust and example-plugin checks passed."
