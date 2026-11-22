#!/usr/bin/env python3
"""Report publisher declarations in POMs from the resolved Android runtime graph."""

import argparse
import hashlib
import json
from pathlib import Path
import re
from xml.etree import ElementTree


LISTENABLEFUTURE = "com.google.guava:listenablefuture:1.0"
LISTENABLEFUTURE_PARENT = "com.google.guava:guava-parent:26.0-android"
LISTENABLEFUTURE_SOURCE = "https://github.com/google/guava/blob/b62d52926b904a8def90fd427acad53d79fa0e9f"


def text(element, key):
    return (element.findtext(f"{{*}}{key}") or "").strip()


def declared_licenses(pom):
    return [
        {key: text(entry, key) for key in ("name", "url")}
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
            pom = ElementTree.fromstring(metadata)
            entry["licenses"] = declared_licenses(pom)
            # An inherited or interpolated license still needs review; don't infer a license.
            usable = any(
                license["name"] and "${" not in license["name"] and "${" not in license["url"]
                for license in entry["licenses"]
            )
            entry["licenseStatus"] = "declared" if usable else "no-usable-direct-declaration"
            parent = pom.find("{*}parent")
            if not usable and coordinates == LISTENABLEFUTURE and parent is not None:
                parent_coordinates = ":".join(text(parent, key) for key in ("groupId", "artifactId", "version"))
                if parent_coordinates == LISTENABLEFUTURE_PARENT:
                    entry["upstreamEvidence"] = {
                        "license": "Apache-2.0",
                        "parent": parent_coordinates,
                        "source": f"{LISTENABLEFUTURE_SOURCE}/guava/src/com/google/common/util/concurrent/ListenableFuture.java",
                    }
                    entry["licenseStatus"] = "upstream-source-documented"
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
    pending = [entry["coordinates"] for entry in result["modules"] if entry["licenseStatus"] not in {"declared", "upstream-source-documented"}]
    print(f"Android release runtime: {len(result['modules'])} Maven modules; {len(pending)} without direct or documented upstream license evidence.")
    for module in pending:
        print(f"Review: {module}")


if __name__ == "__main__":
    main()
