#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -ne 2 ]; then
  printf '%s\n' 'Usage: scripts/verify-release.sh OUTPUT_DIR VERSION' >&2
  exit 2
fi

output_dir="$1"
version="$2"
if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  printf 'error: invalid VERSION: %s\n' "$version" >&2
  exit 2
fi
case "$output_dir" in
  /*) ;;
  *) output_dir="$(pwd)/$output_dir" ;;
esac

source_archive="$output_dir/reginux-$version-source.tar.gz"
binary_archive="$output_dir/reginux-$version-linux-$(uname -m).tar.gz"
sbom="$output_dir/reginux-$version-sbom.cdx.json"
manifest="$output_dir/reginux-$version-manifest.json"
for artifact in "$source_archive" "$binary_archive" "$sbom" "$manifest" "$output_dir/SHA256SUMS"; do
  [ -f "$artifact" ] || {
    printf 'error: missing artifact: %s\n' "$artifact" >&2
    exit 1
  }
done

(cd "$output_dir" && sha256sum -c SHA256SUMS)

source_listing="$(tar -tzf "$source_archive")"
grep -Fx "reginux-$version/Cargo.toml" <<< "$source_listing" >/dev/null
grep -Fx "reginux-$version/Cargo.lock" <<< "$source_listing" >/dev/null
grep -Fx "reginux-$version/scripts/release.sh" <<< "$source_listing" >/dev/null
if grep -qE '(^|/)target(/|$)|(^|/)\.git(/|$)' <<< "$source_listing"; then
  printf '%s\n' 'error: source archive contains build or Git working data' >&2
  exit 1
fi

extract_dir="$(mktemp -d "${TMPDIR:-/tmp}/reginux-verify.XXXXXX")"
trap 'rm -rf "$extract_dir"' EXIT
tar -xzf "$binary_archive" -C "$extract_dir"
bundle_dir="$extract_dir/reginux-$version"
for required in \
  "$bundle_dir/bin/reginux" \
  "$bundle_dir/bin/reginux-sandbox" \
  "$bundle_dir/bin/reginux-helper" \
  "$bundle_dir/share/reginux/sbom.cdx.json" \
  "$bundle_dir/share/reginux/cargo-metadata.json" \
  "$bundle_dir/share/reginux/manifest.json"; do
  [ -f "$required" ] || {
    printf 'error: binary archive is missing: %s\n' "$required" >&2
    exit 1
  }
done

grep -Fx "reginux $version" <("$bundle_dir/bin/reginux" --version) >/dev/null
python3 - "$manifest" "$sbom" "$version" <<'PY'
import json
import sys

manifest_path, sbom_path, expected = sys.argv[1:]
with open(manifest_path, encoding="utf-8") as stream:
    manifest = json.load(stream)
with open(sbom_path, encoding="utf-8") as stream:
    sbom = json.load(stream)
if manifest.get("version") != expected:
    raise SystemExit("manifest version mismatch")
if sbom.get("bomFormat") != "CycloneDX" or sbom.get("specVersion") != "1.5":
    raise SystemExit("SBOM is not CycloneDX 1.5")
if not sbom.get("components"):
    raise SystemExit("SBOM has no dependency components")
PY

printf 'Release artifacts verified: %s\n' "$output_dir"
