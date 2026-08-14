#!/usr/bin/env python3
"""Build a deterministic CycloneDX SBOM from cargo metadata."""

import json
import sys
from pathlib import Path


def main() -> int:
    if len(sys.argv) != 3:
        print(f"usage: {sys.argv[0]} CARGO_METADATA_JSON OUTPUT_JSON", file=sys.stderr)
        return 2

    metadata_path = Path(sys.argv[1])
    output_path = Path(sys.argv[2])
    metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
    packages = metadata.get("packages")
    if not isinstance(packages, list):
        raise ValueError("cargo metadata does not contain a packages array")

    components = []
    seen = set()
    for package in packages:
        name = package.get("name")
        version = package.get("version")
        source = package.get("source") or "path"
        if not isinstance(name, str) or not isinstance(version, str):
            raise ValueError("cargo metadata contains a package without name/version")
        key = (name, version, source)
        if key in seen:
            continue
        seen.add(key)
        purl = f"pkg:cargo/{name}@{version}"
        component = {
            "bom-ref": purl,
            "name": name,
            "purl": purl,
            "scope": "required",
            "type": "library",
            "version": version,
        }
        if source != "path":
            component["externalReferences"] = [{"type": "distribution", "url": source}]
        components.append(component)

    components.sort(key=lambda item: (item["name"], item["version"], item["bom-ref"]))
    bom = {
        "bomFormat": "CycloneDX",
        "components": components,
        "metadata": {
            "tools": [{"name": "Reginux build-sbom.py", "vendor": "Reginux"}]
        },
        "specVersion": "1.5",
        "version": 1,
    }
    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text(
        json.dumps(bom, ensure_ascii=False, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError, json.JSONDecodeError) as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1)
