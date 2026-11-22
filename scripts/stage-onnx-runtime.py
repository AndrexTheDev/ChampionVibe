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
APK_LIB_DIR = "lib/arm64-v8a"
APK_MEMBER = f"{APK_LIB_DIR}/libonnxruntime.so"
APK_APP_MEMBER = f"{APK_LIB_DIR}/libchampionvibe_lib.so"
CXX_NAME = "libc++_shared.so"
APK_CXX_MEMBER = f"{APK_LIB_DIR}/{CXX_NAME}"
APK_NDK_NOTICES = "assets/championvibe-licenses/android-ndk"
NDK_NOTICES = ("NOTICE", "NOTICE.toolchain")
GUAVA_NOTICES = ROOT / "legal/android/listenablefuture-1.0"
APK_GUAVA_NOTICES = "assets/championvibe-licenses/listenablefuture-1.0"
NDK_SYSROOT_LIB = Path("toolchains/llvm/prebuilt/linux-x86_64/sysroot/usr/lib/aarch64-linux-android")
ANDROID_PIN = ROOT / f"reports/android-onnx-runtime-{VERSION}.json"
NOTICE_DIR = ROOT / f"legal/onnxruntime/{VERSION}"
APK_NOTICE_DIR = "assets/championvibe-licenses/onnxruntime"
NOTICE_DIGESTS = {
    "LICENSE": "c250d6278f0b47a6439fb7592b08b58a55eb9f535aa49a1db63211c3f982b674",
    "ThirdPartyNotices.txt": "fb0af774b4d7cffc5b9d046f2aaeade2f37df2f80abf8033c95dfffcc77a8866",
}


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


def elf_program_headers(data):
    verify_arm64_so(data)
    phoff = struct.unpack_from("<Q", data, 32)[0]
    phentsize, phnum = struct.unpack_from("<HH", data, 54)
    if phoff < 64 or phentsize != 56 or not phnum or phoff + phentsize * phnum > len(data):
        raise ValueError("invalid Android ELF program header table")
    return range(phoff, phoff + phentsize * phnum, phentsize)


def verify_elf_load_alignment(data):
    alignments = []
    for offset in elf_program_headers(data):
        if struct.unpack_from("<I", data, offset)[0] != 1:  # PT_LOAD
            continue
        file_offset, virtual_address = struct.unpack_from("<QQ", data, offset + 8)
        alignment = struct.unpack_from("<Q", data, offset + 48)[0]
        if alignment < 16384 or (virtual_address - file_offset) % 16384:
            raise ValueError("Android shared library has a load segment incompatible with 16 KiB pages")
        alignments.append(alignment)
    if not alignments:
        raise ValueError("Android shared library has no ELF load segments")
    return min(alignments)


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
        "minimumElfLoadAlignment": verify_elf_load_alignment(data),
        "notices": notices,
    }


def load_android_pin():
    pin = json.loads(ANDROID_PIN.read_text(encoding="utf-8"))
    if (pin.get("version"), pin.get("source"), pin.get("libraryPath")) != (VERSION, ANDROID_URL, ANDROID_MEMBER):
        raise ValueError("the Android archive pin does not match the release version and source")
    for key in ("archiveSha256", "librarySha256"):
        if not re.fullmatch(r"[0-9a-f]{64}", pin.get(key, "")):
            raise ValueError(f"missing or invalid Android SHA-256 pin: {key}")
    return pin


def check_android_pin(report):
    pin = load_android_pin()
    for key in ("archiveSha256", "librarySha256"):
        if report[key] != pin[key]:
            raise ValueError(f"Android runtime does not match the checked-in SHA-256 pin: {key}")


def validate_notice(name, data):
    if hashlib.sha256(data).hexdigest() != NOTICE_DIGESTS[name]:
        raise ValueError(f"Android ONNX Runtime notice is missing or altered: {name}")


def stage_android(archive, dest, notice_dest):
    pin = load_android_pin()
    archive_digest = file_sha256(archive)
    if archive_digest != pin["archiveSha256"]:
        raise ValueError("the Android runtime archive failed its SHA-256 check")
    data = read_android_library(archive)
    if hashlib.sha256(data).hexdigest() != pin["librarySha256"]:
        raise ValueError("the extracted Android runtime library failed its SHA-256 check")
    verify_elf_load_alignment(data)
    notices = {name: (NOTICE_DIR / name).read_bytes() for name in NOTICE_DIGESTS}
    for name, payload in notices.items():
        validate_notice(name, payload)
    dest.mkdir(parents=True, exist_ok=True)
    notice_dest.mkdir(parents=True, exist_ok=True)
    (dest / "libonnxruntime.so").write_bytes(data)
    for name, payload in notices.items():
        (notice_dest / name).write_bytes(payload)


def elf_build_id(data):
    # A GNU build ID survives symbol stripping by Gradle; a whole-file hash may not.
    for header in elf_program_headers(data):
        if struct.unpack_from("<I", data, header)[0] != 4:  # PT_NOTE
            continue
        start = struct.unpack_from("<Q", data, header + 8)[0]
        end = start + struct.unpack_from("<Q", data, header + 32)[0]
        if end > len(data):
            raise ValueError("truncated Android ELF note segment")
        while start + 12 <= end:
            name_size, descriptor_size, note_type = struct.unpack_from("<III", data, start)
            name_start = start + 12
            descriptor_start = name_start + ((name_size + 3) & ~3)
            start = descriptor_start + ((descriptor_size + 3) & ~3)
            if start > end:
                raise ValueError("truncated Android ELF note")
            if note_type == 3 and data[name_start:name_start + name_size] == b"GNU\0" and descriptor_size:
                return data[descriptor_start:descriptor_start + descriptor_size].hex()
    raise ValueError("Android C++ runtime has no GNU build ID")


def read_ndk_cxx(ndk):
    library = (ndk / NDK_SYSROOT_LIB / CXX_NAME).read_bytes()
    verify_elf_load_alignment(library)
    build_id = elf_build_id(library)
    notices = {name: (ndk / name).read_bytes() for name in NDK_NOTICES}
    if any(not payload.strip() for payload in notices.values()):
        raise ValueError("an Android NDK notice is empty")
    return library, build_id, notices


def stage_android_cxx(ndk, dest):
    library, build_id, notices = read_ndk_cxx(ndk)
    libs = dest / "jniLibs/arm64-v8a"
    libs.mkdir(parents=True, exist_ok=True)
    (libs / CXX_NAME).write_bytes(library)
    notice_dest = dest / APK_NDK_NOTICES
    notice_dest.mkdir(parents=True, exist_ok=True)
    for name, payload in notices.items():
        (notice_dest / name).write_bytes(payload)
    return build_id


def verify_apk(path, ndk):
    _, build_id, ndk_notices = read_ndk_cxx(ndk)
    with zipfile.ZipFile(path) as apk:
        names = apk.namelist()
        libraries = [name for name in names if name.startswith("lib/") and name.endswith(".so")]
        if any(name.rsplit("/", 1)[0] != APK_LIB_DIR for name in libraries):
            raise ValueError("the release APK contains a non-arm64 native library")
        for member in (APK_APP_MEMBER, APK_MEMBER, APK_CXX_MEMBER):
            if libraries.count(member) != 1:
                raise ValueError(f"expected exactly one {member} in {path}")
        if len(libraries) != len(set(libraries)):
            raise ValueError("the release APK contains duplicate native libraries")
        for member in libraries:
            data = apk.read(member)
            verify_elf_load_alignment(data)
            if member == APK_CXX_MEMBER and elf_build_id(data) != build_id:
                raise ValueError("the APK C++ runtime does not match the selected NDK build ID")
        for name in NOTICE_DIGESTS:
            asset = f"{APK_NOTICE_DIR}/{name}"
            if names.count(asset) != 1:
                raise ValueError(f"expected exactly one runtime notice in the APK: {asset}")
            validate_notice(name, apk.read(asset))
        for name in ("COPYING", "ATTRIBUTION.txt"):
            asset = f"{APK_GUAVA_NOTICES}/{name}"
            if names.count(asset) != 1 or apk.read(asset) != (GUAVA_NOTICES / name).read_bytes():
                raise ValueError(f"Guava ListenableFuture notice missing or altered in the APK: {asset}")
        for name, payload in ndk_notices.items():
            asset = f"{APK_NDK_NOTICES}/{name}"
            if names.count(asset) != 1 or apk.read(asset) != payload:
                raise ValueError(f"Android NDK notice missing or altered in the APK: {asset}")


def download(url, archive):
    with urllib.request.urlopen(url, timeout=60) as response, archive.open("wb") as file:
        shutil.copyfileobj(response, file)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="action", required=True)
    verify = commands.add_parser("verify-apk")
    verify.add_argument("path", type=Path)
    verify.add_argument("--ndk", required=True, type=Path)
    cxx = commands.add_parser("stage-android-cxx")
    cxx.add_argument("--ndk", required=True, type=Path)
    cxx.add_argument("--dest", required=True, type=Path)
    commands.add_parser("verify-android-library").add_argument("path", type=Path)
    commands.add_parser("windows")
    commands.add_parser("android")
    inspect = commands.add_parser("inspect-android")
    inspect.add_argument("--report", required=True, type=Path)
    inspect.add_argument("--check-pin", action="store_true")
    inspect.add_argument("--stage-dir", type=Path, help="stage the pinned runtime and notices for a no-build check")
    args = parser.parse_args()
    try:
        if args.action == "verify-android-library":
            alignment = verify_elf_load_alignment(args.path.read_bytes())
            print(f"Verified Android arm64 library (minimum ELF load alignment: {alignment} bytes): {args.path}")
            return
        if args.action == "stage-android-cxx":
            build_id = stage_android_cxx(args.ndk, args.dest)
            print(f"Staged Android NDK C++ runtime and notices (GNU build ID {build_id})")
            return
        if args.action == "verify-apk":
            verify_apk(args.path, args.ndk)
            print(f"Verified Android APK native libraries and notices: {args.path}")
            return
        if args.action == "inspect-android":
            with tempfile.TemporaryDirectory() as temp:
                archive = Path(temp) / "runtime.aar"
                download(ANDROID_URL, archive)
                report = inspect_android(archive)
                if args.check_pin or args.stage_dir:
                    check_android_pin(report)
                if args.stage_dir:
                    stage_android(archive, args.stage_dir / "jniLibs/arm64-v8a", args.stage_dir / APK_NOTICE_DIR)
            args.report.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
            print(f"Inspected the Android AAR without building an APK: {args.report}")
            return
        if args.action == "windows":
            url = WINDOWS_URL
            dest = ROOT / "src-tauri/runtime/windows-x64"
        else:
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
                stage_android(archive, dest, app / "src/main" / APK_NOTICE_DIR)
        print(f"Staged ONNX Runtime {VERSION} under {dest.relative_to(ROOT)}")
    except (OSError, KeyError, ValueError, zipfile.BadZipFile) as error:
        parser.exit(1, f"Native runtime packaging: {error}\n")


if __name__ == "__main__":
    main()
