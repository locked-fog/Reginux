#!/usr/bin/env bash
set -euo pipefail

project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project_root"

cargo fmt --all -- --check
cargo check --locked --workspace
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets -- -D warnings

bash -n scripts/install-local.sh scripts/release.sh scripts/verify-release.sh
python3 -c 'import ast; ast.parse(open("scripts/build-sbom.py", encoding="utf-8").read())'

# Exercise the real command and Lua syntax used by the examples through the
# core test suite; manifests are parsed by plugin unit tests.
/usr/bin/date '+[{"id":"local","current":"%FT%T%:z"}]' >/dev/null
test "$(cargo run --locked -q -p reginux-tui --bin reginux -- --version)" = 'reginux 1.0.0'

printf '%s\n' "All Rust and example-plugin checks passed."
