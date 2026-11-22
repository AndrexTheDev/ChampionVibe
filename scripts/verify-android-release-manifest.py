#!/usr/bin/env python3
"""Check the Android release manifest, either merged by Gradle or decoded from an APK."""

import argparse
import json
from pathlib import Path
from xml.etree import ElementTree

ANDROID = "{http://schemas.android.com/apk/res/android}"
# File imports use the system file picker. Mix recording captures app output, not the microphone.
DISALLOWED_PERMISSIONS = {
    "MANAGE_EXTERNAL_STORAGE", "READ_EXTERNAL_STORAGE", "WRITE_EXTERNAL_STORAGE",
    "READ_MEDIA_AUDIO", "READ_MEDIA_IMAGES", "READ_MEDIA_VIDEO", "READ_MEDIA_VISUAL_USER_SELECTED",
    "RECORD_AUDIO",
}


def verify(manifest, package, min_sdk, target_sdk):
    if manifest.tag != "manifest" or manifest.get("package") != package:
        raise ValueError(f"expected Android application ID {package}")
    sdk = manifest.find("uses-sdk")
    if sdk is None or (sdk.get(ANDROID + "minSdkVersion"), sdk.get(ANDROID + "targetSdkVersion")) != (
        str(min_sdk), str(target_sdk),
    ):
        raise ValueError(f"expected Android minSdk {min_sdk} and targetSdk {target_sdk}")
    app = manifest.find("application")
    if app is None or app.get(ANDROID + "debuggable", "false") != "false" or app.get(ANDROID + "testOnly", "false") != "false":
        raise ValueError("expected a production application, not a debuggable or test-only app")
    if app.get(ANDROID + "usesCleartextTraffic") != "false":
        raise ValueError("the Android release must explicitly disable cleartext traffic")

    permissions = {
        element.get(ANDROID + "name")
        for element in manifest
        if element.tag == "uses-permission" or element.tag.startswith("uses-permission-sdk-")
    }
    permissions.discard(None)
    forbidden = sorted(permissions & {f"android.permission.{name}" for name in DISALLOWED_PERMISSIONS})
    if forbidden:
        raise ValueError(f"unexpected broad Android permission(s): {', '.join(forbidden)}")
    if "android.permission.INTERNET" not in permissions:
        raise ValueError("missing Android INTERNET permission for model downloads and licence activation")

    providers = [p for p in app.findall("provider") if p.get(ANDROID + "authorities") == f"{package}.fileprovider"]
    if (len(providers) != 1 or providers[0].get(ANDROID + "exported") != "false"
            or providers[0].get(ANDROID + "grantUriPermissions") != "true"):
        raise ValueError("expected a private app FileProvider with per-URI access grants")
    return sorted(permissions)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("manifest", type=Path)
    parser.add_argument("--config", type=Path, default=Path("src-tauri/tauri.conf.json"))
    parser.add_argument("--target-sdk", required=True, type=int)
    args = parser.parse_args()
    try:
        config = json.loads(args.config.read_text(encoding="utf-8"))
        package = config["identifier"]
        min_sdk = config["bundle"]["android"]["minSdkVersion"]
        permissions = verify(ElementTree.parse(args.manifest).getroot(), package, min_sdk, args.target_sdk)
    except (OSError, KeyError, ValueError, ElementTree.ParseError) as error:
        parser.exit(1, f"Android release manifest: {error}\n")
    print(f"Verified Android release manifest: {package}; minSdk {min_sdk}; targetSdk {args.target_sdk}")
    print("Declared permissions: " + ", ".join(permissions))


if __name__ == "__main__":
    main()
