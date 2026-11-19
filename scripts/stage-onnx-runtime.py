#!/usr/bin/env python3
"""Stage only the native libraries needed by the release and check the APK layout."""

import argparse
import base64
import hashlib
import shutil
import struct
import tarfile
import tempfile
import urllib.request
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
VERSION = "1.24.3"  # ort rc.12 uses native API 24
WINDOWS_URL = f"https://registry.npmjs.org/onnxruntime-node/-/onnxruntime-node-{VERSION}.tgz"
WINDOWS_SHA512 = "JH7+czbc8ALA819vlTgcV+Q214/+VjGeBHDjX81+ZCD0PCVCIFGFNtT0V4sXG/1JXypKPgScQcB3ij/hk3YnTg=="
WINDOWS_PREFIX = "package/bin/napi-v6/win32/x64/"
WINDOWS_FILES = ("onnxruntime.dll", "DirectML.dll", "dxcompiler.dll", "dxil.dll")
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


def stage_windows(archive, dest):
    with archive.open("rb") as file:
        digest = base64.b64encode(hashlib.file_digest(file, "sha512").digest()).decode()
    if digest != WINDOWS_SHA512:
        raise ValueError("the Windows runtime archive failed its SHA-512 check")
    with tarfile.open(archive, "r:gz") as tar:
        libraries = {}
        for name in WINDOWS_FILES:
            member = tar.getmember(WINDOWS_PREFIX + name)
            if not member.isfile():
                raise ValueError(f"unexpected runtime archive entry: {member.name}")
            with tar.extractfile(member) as file:
                libraries[name] = file.read()
            verify_x64_dll(libraries[name])
    dest.mkdir(parents=True, exist_ok=True)
    for name, data in libraries.items():
        (dest / name).write_bytes(data)


def stage_android(archive, dest):
    with zipfile.ZipFile(archive) as aar:
        data = aar.read(ANDROID_MEMBER)
    verify_arm64_so(data)
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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="action", required=True)
    commands.add_parser("verify-apk").add_argument("path", type=Path)
    commands.add_parser("windows")
    commands.add_parser("android")
    args = parser.parse_args()
    try:
        if args.action == "verify-apk":
            verify_apk(args.path)
            print(f"Verified Android runtime in {args.path}")
            return
        if args.action == "windows":
            url, stage = WINDOWS_URL, stage_windows
            dest = ROOT / "src-tauri/runtime/windows-x64"
        else:
            url, stage = ANDROID_URL, stage_android
            app = ROOT / "src-tauri/gen/android/app"
            if not (app / "build.gradle.kts").is_file():
                raise ValueError("initialize the Tauri Android project before staging the runtime")
            dest = app / "src/main/jniLibs/arm64-v8a"
        with tempfile.TemporaryDirectory() as temp:
            archive = Path(temp) / "runtime"
            with urllib.request.urlopen(url, timeout=60) as response, archive.open("wb") as file:
                shutil.copyfileobj(response, file)
            stage(archive, dest)
        print(f"Staged ONNX Runtime {VERSION} under {dest.relative_to(ROOT)}")
    except (OSError, KeyError, ValueError, tarfile.TarError, zipfile.BadZipFile) as error:
        parser.exit(1, f"ONNX Runtime: {error}\n")


if __name__ == "__main__":
    main()
