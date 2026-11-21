#!/usr/bin/env python3
"""Align and sign one release APK; publish the output only after verification."""

import argparse
import base64
import binascii
import os
from pathlib import Path
import re
import subprocess
import tempfile

SIGNING_ENV = (
    "ANDROID_RELEASE_KEYSTORE_BASE64",
    "ANDROID_RELEASE_STORE_PASSWORD",
    "ANDROID_RELEASE_KEY_ALIAS",
    "ANDROID_RELEASE_KEY_PASSWORD",
    "ANDROID_RELEASE_CERT_SHA256",
)
SIGNER_DIGEST = re.compile(r"^Signer #\d+ certificate SHA-256 digest:\s*([0-9a-f:]+)$", re.I | re.M)


def normalize_digest(digest):
    value = digest.strip().replace(":", "").lower()
    if not re.fullmatch(r"[0-9a-f]{64}", value):
        raise ValueError("the expected signing certificate digest must be a SHA-256 fingerprint")
    return value


def load_credentials(environment):
    missing = [name for name in SIGNING_ENV if not environment.get(name)]
    if missing:
        raise ValueError(f"missing Android release configuration: {', '.join(missing)}")
    try:
        store = base64.b64decode("".join(environment[SIGNING_ENV[0]].split()), validate=True)
    except binascii.Error as error:
        raise ValueError("the Android release keystore is not valid base64") from error
    if not store:
        raise ValueError("the Android release keystore is empty")
    return store, normalize_digest(environment[SIGNING_ENV[4]])


def find_release_apk(project):
    candidates = sorted((project / "app/build/outputs/apk").glob("**/release/*.apk"))
    if len(candidates) != 1:
        raise ValueError(f"expected exactly one Gradle release APK, found {len(candidates)}")
    return candidates[0]


def sign_apk(source, output, build_tools, environment):
    store, expected_digest = load_credentials(environment)
    if source.resolve() == output.resolve() or output.exists():
        raise ValueError("the signed output must be a new file, distinct from the source")
    zipalign = str(build_tools / "zipalign")
    apksigner = str(build_tools / "apksigner")
    for tool in (zipalign, apksigner):
        if not os.access(tool, os.X_OK):
            raise ValueError(f"missing executable Android build tool: {tool}")
    child_env = {name: value for name, value in environment.items() if name not in SIGNING_ENV}
    signing_env = child_env | {name: environment[name] for name in (SIGNING_ENV[1], SIGNING_ENV[3])}
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="android-sign-", dir=output.parent) as directory:
        root = Path(directory)
        aligned, signed, keystore = (root / name for name in ("aligned.apk", "signed.apk", "release.keystore"))
        fd = os.open(keystore, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, "wb") as file:
            file.write(store)
        subprocess.run([zipalign, "-P", "16", "-f", "4", str(source), str(aligned)], check=True, env=child_env)
        subprocess.run([
            apksigner, "sign", "--ks", str(keystore), "--ks-key-alias", environment[SIGNING_ENV[2]],
            "--ks-pass", f"env:{SIGNING_ENV[1]}", "--key-pass", f"env:{SIGNING_ENV[3]}",
            "--out", str(signed), str(aligned),
        ], check=True, env=signing_env)
        subprocess.run([zipalign, "-c", "-P", "16", "4", str(signed)], check=True, env=child_env)
        result = subprocess.run([apksigner, "verify", "--print-certs", str(signed)], check=True,
                                capture_output=True, text=True, env=child_env)
        if [normalize_digest(value) for value in SIGNER_DIGEST.findall(result.stdout)] != [expected_digest]:
            raise ValueError("the APK does not have exactly the expected release signer")
        signed.replace(output)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("check-config")
    sign = commands.add_parser("sign")
    sign.add_argument("--project", required=True, type=Path)
    sign.add_argument("--output", required=True, type=Path)
    sign.add_argument("--build-tools", required=True, type=Path)
    args = parser.parse_args()
    try:
        if args.command == "check-config":
            load_credentials(os.environ)
            print("Android release signing configuration is present.")
        else:
            sign_apk(find_release_apk(args.project), args.output, args.build_tools, os.environ)
            print(f"Verified APK signature and ZIP alignment: {args.output}")
    except subprocess.CalledProcessError as error:
        parser.exit(1, f"Android signing: {Path(error.cmd[0]).name} failed (exit {error.returncode}).\n")
    except (OSError, ValueError) as error:
        parser.exit(1, f"Android signing: {error}\n")


if __name__ == "__main__":
    main()
