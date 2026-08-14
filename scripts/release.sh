#!/usr/bin/env bash
set -euo pipefail

usage() {
  printf '%s\n' 'Usage: scripts/release.sh VERSION [OUTPUT_DIR]' >&2
}

if [ "$#" -lt 1 ] || [ "$#" -gt 2 ]; then
  usage
  exit 2
fi

version="$1"
output_dir="${2:-dist}"
if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  printf 'error: VERSION must be a stable numeric SemVer, got %s\n' "$version" >&2
  exit 2
fi

project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project_root"

case "$output_dir" in
  /*) ;;
  *) output_dir="$project_root/$output_dir" ;;
esac
if [ -e "$output_dir" ] && [ ! -d "$output_dir" ]; then
  printf 'error: output path is not a directory: %s\n' "$output_dir" >&2
  exit 1
fi
if [ -d "$output_dir" ] && find "$output_dir" -mindepth 1 -maxdepth 1 -print -quit | grep -q .; then
  printf 'error: output directory is not empty: %s\n' "$output_dir" >&2
  exit 1
fi
mkdir -p "$output_dir"

if ! git diff --quiet || ! git diff --cached --quiet; then
  printf '%s\n' 'error: release requires a clean Git working tree' >&2
  exit 1
fi

stage_dir="$(mktemp -d "${TMPDIR:-/tmp}/reginux-release.XXXXXX")"
trap 'rm -rf "$stage_dir"' EXIT

metadata_json="$stage_dir/cargo-metadata.json"
cargo metadata --locked --format-version 1 > "$metadata_json"
python3 - "$version" "$metadata_json" <<'PY'
import json
import sys

expected = sys.argv[1]
with open(sys.argv[2], encoding="utf-8") as stream:
    metadata = json.load(stream)
workspace = set(metadata["workspace_members"])
packages = {package["id"]: package for package in metadata["packages"]}
versions = {
    package["version"]
    for package_id, package in packages.items()
    if package_id in workspace
}
if versions != {expected}:
    raise SystemExit(f"workspace versions {sorted(versions)!r} do not match {expected!r}")
PY

cargo fmt --all -- --check
cargo build --release --locked --workspace --bins
cargo test --release --locked --workspace
cargo run --release --locked -q -p reginux-tui --bin reginux -- --version > "$stage_dir/cli-version.txt"
grep -Fx "reginux $version" "$stage_dir/cli-version.txt" >/dev/null

sbom_path="$stage_dir/sbom.cdx.json"
python3 scripts/build-sbom.py "$metadata_json" "$sbom_path"

archive_root="$stage_dir/reginux-$version"
mkdir -p "$archive_root/bin" "$archive_root/share/reginux"
install -m 0755 target/release/reginux "$archive_root/bin/reginux"
install -m 0755 target/release/reginux-sandbox "$archive_root/bin/reginux-sandbox"
install -m 0755 target/release/reginux-helper "$archive_root/bin/reginux-helper"
cp Cargo.lock Cargo.toml CHANGELOG.md LICENSE README.md "$archive_root/"
cp -a config crates docs plugins resources scripts "$archive_root/"
cp "$sbom_path" "$archive_root/share/reginux/sbom.cdx.json"
cp "$metadata_json" "$archive_root/share/reginux/cargo-metadata.json"

commit="$(git rev-parse HEAD)"
arch="$(uname -m)"
rustc_version="$(rustc --version)"
python3 - "$archive_root/share/reginux/manifest.json" "$version" "$commit" "$arch" "$rustc_version" <<'PY'
import json
import sys

path, version, commit, arch, rustc = sys.argv[1:]
with open(path, "w", encoding="utf-8") as stream:
    json.dump(
        {
            "version": version,
            "commit": commit,
            "architecture": arch,
            "rustc": rustc,
        },
        stream,
        ensure_ascii=False,
        indent=2,
        sort_keys=True,
    )
    stream.write("\n")
PY

source_archive="$output_dir/reginux-$version-source.tar.gz"
binary_archive="$output_dir/reginux-$version-linux-$arch.tar.gz"
git archive --format=tar.gz --prefix="reginux-$version/" -o "$source_archive" HEAD
tar -C "$stage_dir" -czf "$binary_archive" "reginux-$version"
cp "$sbom_path" "$output_dir/reginux-$version-sbom.cdx.json"
cp "$archive_root/share/reginux/manifest.json" "$output_dir/reginux-$version-manifest.json"

(cd "$output_dir" && sha256sum \
  "reginux-$version-source.tar.gz" \
  "reginux-$version-linux-$arch.tar.gz" \
  "reginux-$version-sbom.cdx.json" \
  "reginux-$version-manifest.json" > SHA256SUMS)

printf 'Release artifacts written to %s\n' "$output_dir"
