#!/usr/bin/env python3
"""Stage only the native libraries needed by the release and check the APK layout."""

import argparse
import hashlib
import json
import re
import shutil
import struct
import tempfile
import urllib.request
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
VERSION = "1.24.3"  # ort rc.12 uses native API 24
# Official ONNX Runtime DirectML distribution, pinned to the PyPI file hash.
WINDOWS_URL = (
    "https://files.pythonhosted.org/packages/05/40/"
    "c948c0ee42b7b6297dd45956092f5a53a6954610c3911a5847c7555b4930/"
    f"onnxruntime_directml-{VERSION}-cp312-cp312-win_amd64.whl"
)
WINDOWS_SHA256 = "d889010e6ed2f30026522308173d295bcfdaf6f28d1df6054c748ffa750a7ad5"
WINDOWS_FILES = ("onnxruntime.dll", "DirectML.dll", "onnxruntime_providers_shared.dll")
WINDOWS_NOTICES = {
    "onnxruntime-LICENSE.txt": "onnxruntime/LICENSE",
    "onnxruntime-ThirdPartyNotices.txt": "onnxruntime/ThirdPartyNotices.txt",
}
ANDROID_URL = (
    "https://repo.maven.apache.org/maven2/com/microsoft/onnxruntime/onnxruntime-android/"
    f"{VERSION}/onnxruntime-android-{VERSION}.aar"
)
ANDROID_MEMBER = "jni/arm64-v8a/libonnxruntime.so"
APK_MEMBER = "lib/arm64-v8a/libonnxruntime.so"


def verify_x64_dll(data):
    if len(data) >= 64 and data[:2] == b"MZ":
        offset = struct.unpack_from("<I", data, 60)[0]
        if data[offset : offset + 6] == b"PE\0\0\x64\x86":
            return
    raise ValueError("expected a Windows x64 PE binary")


def verify_arm64_so(data):
    if (
        len(data) < 64
        or data[:7] != b"\x7fELF\x02\x01\x01"
        or struct.unpack_from("<HH", data, 16) != (3, 183)  # ET_DYN, EM_AARCH64
    ):
        raise ValueError("expected an Android arm64 ELF shared library")


def file_sha256(path):
    with path.open("rb") as file:
        return hashlib.file_digest(file, "sha256").hexdigest()


def expected_sha256(value):
    if not re.fullmatch(r"[0-9a-fA-F]{64}", value):
        raise ValueError("expected the reviewed 64-digit hexadecimal SHA-256 pin")
    return value.lower()


def stage_windows(archive, dest):
    if file_sha256(archive) != WINDOWS_SHA256:
        raise ValueError("the Windows runtime archive failed its SHA-256 check")
    with zipfile.ZipFile(archive) as wheel:
        files = {}
        for name in WINDOWS_FILES:
            files[name] = wheel.read(f"onnxruntime/capi/{name}")
            verify_x64_dll(files[name])
        for name, source in WINDOWS_NOTICES.items():
            files[name] = wheel.read(source)
            if not files[name].strip():
                raise ValueError(f"the Windows distribution notice is empty: {source}")
    dest.mkdir(parents=True, exist_ok=True)
    for name, data in files.items():
        (dest / name).write_bytes(data)


def read_android_library(archive):
    with zipfile.ZipFile(archive) as aar:
        matches = [entry for entry in aar.infolist() if entry.filename == ANDROID_MEMBER]
        if len(matches) != 1:
            raise ValueError(f"expected exactly one {ANDROID_MEMBER} in the AAR, got {len(matches)}")
        data = aar.read(matches[0])
    verify_arm64_so(data)
    return data


def inspect_android(archive):
    data = read_android_library(archive)
    with zipfile.ZipFile(archive) as aar:
        notices = [
            {"path": entry.filename, "sha256": hashlib.sha256(aar.read(entry)).hexdigest()}
            for entry in aar.infolist()
            if not entry.is_dir() and re.search(r"(?:license|notice|copying)", Path(entry.filename).name, re.I)
        ]
    return {
        "version": VERSION,
        "source": ANDROID_URL,
        "archiveBytes": archive.stat().st_size,
        "archiveSha256": file_sha256(archive),
        "libraryPath": ANDROID_MEMBER,
        "libraryBytes": len(data),
        "librarySha256": hashlib.sha256(data).hexdigest(),
        "notices": notices,
    }


def stage_android(archive, dest, sha256):
    if file_sha256(archive) != expected_sha256(sha256):
        raise ValueError("the Android runtime archive failed its SHA-256 check")
    data = read_android_library(archive)
    dest.mkdir(parents=True, exist_ok=True)
    (dest / "libonnxruntime.so").write_bytes(data)


def verify_apk(path):
    with zipfile.ZipFile(path) as apk:
        runtime_files = [
            name for name in apk.namelist()
            if name.startswith("lib/") and name.endswith("/libonnxruntime.so")
        ]
        if runtime_files != [APK_MEMBER]:
            raise ValueError(f"expected exactly {APK_MEMBER} in {path}, got {runtime_files}")
        with apk.open(APK_MEMBER) as entry:
            verify_arm64_so(entry.read(64))


def download(url, archive):
    with urllib.request.urlopen(url, timeout=60) as response, archive.open("wb") as file:
        shutil.copyfileobj(response, file)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="action", required=True)
    commands.add_parser("verify-apk").add_argument("path", type=Path)
    commands.add_parser("windows")
    commands.add_parser("android").add_argument("--sha256", required=True)
    commands.add_parser("inspect-android").add_argument("--report", required=True, type=Path)
    args = parser.parse_args()
    try:
        if args.action == "verify-apk":
            verify_apk(args.path)
            print(f"Verified Android runtime in {args.path}")
            return
        if args.action == "inspect-android":
            with tempfile.TemporaryDirectory() as temp:
                archive = Path(temp) / "runtime.aar"
                download(ANDROID_URL, archive)
                report = inspect_android(archive)
            args.report.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
            print(f"Inspected the Android AAR without building an APK: {args.report}")
            return
        if args.action == "windows":
            url = WINDOWS_URL
            dest = ROOT / "src-tauri/runtime/windows-x64"
        else:
            pin = expected_sha256(args.sha256)
            url = ANDROID_URL
            app = ROOT / "src-tauri/gen/android/app"
            if not (app / "build.gradle.kts").is_file():
                raise ValueError("initialize the Tauri Android project before staging the runtime")
            dest = app / "src/main/jniLibs/arm64-v8a"
        with tempfile.TemporaryDirectory() as temp:
            archive = Path(temp) / "runtime"
            download(url, archive)
            if args.action == "windows":
                stage_windows(archive, dest)
            else:
                stage_android(archive, dest, pin)
        print(f"Staged ONNX Runtime {VERSION} under {dest.relative_to(ROOT)}")
    except (OSError, KeyError, ValueError, zipfile.BadZipFile) as error:
        parser.exit(1, f"ONNX Runtime: {error}\n")


if __name__ == "__main__":
    main()
