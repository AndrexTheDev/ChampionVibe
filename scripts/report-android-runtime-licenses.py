#!/usr/bin/env python3
"""Report publisher declarations in POMs from the resolved Android runtime graph."""

import argparse
import hashlib
import json
from pathlib import Path
import re
from xml.etree import ElementTree


def declared_licenses(metadata):
    pom = ElementTree.fromstring(metadata)
    return [
        {key: (entry.findtext(f"{{*}}{key}") or "").strip() for key in ("name", "url")}
        for entry in pom.findall("{*}licenses/{*}license")
    ]


def collect(root):
    modules = []
    seen = set()
    for line in (root / "modules.tsv").read_text(encoding="utf-8").splitlines():
        group, artifact, version, pom_name = line.split("\t")
        coordinates = f"{group}:{artifact}:{version}"
        if not group or not artifact or not version or coordinates in seen:
            raise ValueError(f"Incomplete or duplicate Android runtime coordinates: {coordinates}")
        seen.add(coordinates)
        entry = {"coordinates": coordinates, "pomSha256": None, "licenseStatus": "pom-unavailable", "licenses": []}
        if pom_name:
            if not re.fullmatch(r"[0-9]+\.pom", pom_name):
                raise ValueError("Unexpected Gradle POM file name")
            metadata = (root / "poms" / pom_name).read_bytes()
            entry["pomSha256"] = hashlib.sha256(metadata).hexdigest()
            entry["licenses"] = declared_licenses(metadata)
            # An inherited or interpolated license still needs review; don't infer a license.
            usable = any(
                license["name"] and "${" not in license["name"] and "${" not in license["url"]
                for license in entry["licenses"]
            )
            entry["licenseStatus"] = "declared" if usable else "no-usable-direct-declaration"
        modules.append(entry)
    if not modules:
        raise ValueError("Android release runtime inventory is empty")
    return {"configuration": "arm64ReleaseRuntimeClasspath", "modules": sorted(modules, key=lambda item: item["coordinates"])}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("inventory", type=Path)
    parser.add_argument("report", type=Path)
    args = parser.parse_args()
    result = collect(args.inventory)
    args.report.write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    pending = [entry["coordinates"] for entry in result["modules"] if entry["licenseStatus"] != "declared"]
    print(f"Android release runtime: {len(result['modules'])} Maven modules; {len(pending)} without a usable direct POM license declaration.")
    for module in pending:
        print(f"Review: {module}")


if __name__ == "__main__":
    main()
