"""Offline checks for the release runtime stager; no native build is required."""

import hashlib
import importlib.util
import json
from pathlib import Path
import struct
import tempfile
import unittest
from unittest.mock import patch
import zipfile

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("runtime", ROOT / "scripts/stage-onnx-runtime.py")
runtime = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(runtime)


def pe(machine=0x8664):
    result = bytearray(72)
    result[:2] = b"MZ"
    struct.pack_into("<I4sH", result, 60, 64, b"PE\0\0", machine)
    return bytes(result)


def elf(machine=183, alignment=16384, virtual_address=0, build_id=None):
    result = bytearray(64 + 56 * (1 if build_id is None else 2))
    result[:7] = b"\x7fELF\x02\x01\x01"
    struct.pack_into("<HH", result, 16, 3, machine)
    struct.pack_into("<Q", result, 32, 64)  # e_phoff
    struct.pack_into("<HH", result, 54, 56, 1 if build_id is None else 2)  # e_phentsize and e_phnum
    struct.pack_into("<I", result, 64, 1)  # PT_LOAD
    struct.pack_into("<Q", result, 64 + 16, virtual_address)
    struct.pack_into("<Q", result, 64 + 48, alignment)
    if build_id is not None:
        note = struct.pack("<III", 4, len(build_id), 3) + b"GNU\0" + build_id
        note += b"\0" * (-len(note) % 4)
        struct.pack_into("<I", result, 120, 4)  # PT_NOTE
        struct.pack_into("<Q", result, 120 + 8, len(result))
        struct.pack_into("<Q", result, 120 + 32, len(note))
        result.extend(note)
    return bytes(result)


class RuntimePackagingTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        self.dest = self.root / "stage"
        self.notice_dest = self.root / "notices"

    def write_zip(self, entries):
        path = self.root / "archive.zip"
        with zipfile.ZipFile(path, "w") as archive:
            for name, data in entries.items():
                archive.writestr(name, data)
        return path

    def test_windows_checks_archive_pin_before_writing(self):
        path = self.root / "untrusted.whl"
        path.write_bytes(b"not the expected archive")
        with self.assertRaisesRegex(ValueError, "SHA-256"):
            runtime.stage_windows(path, self.dest)
        self.assertFalse(self.dest.exists())

    def test_windows_stages_only_native_dlls_and_the_vendor_notices(self):
        entries = {f"onnxruntime/capi/{name}": pe() for name in runtime.WINDOWS_FILES}
        entries["onnxruntime/capi/python-only.pyd"] = pe()
        notices = {source: f"notice for {source}".encode() for source in runtime.WINDOWS_NOTICES.values()}
        path = self.write_zip(entries | notices)
        with patch.object(runtime, "WINDOWS_SHA256", runtime.file_sha256(path)):
            runtime.stage_windows(path, self.dest)
        self.assertEqual(set(runtime.WINDOWS_FILES) | set(runtime.WINDOWS_NOTICES), {file.name for file in self.dest.iterdir()})
        for name, source in runtime.WINDOWS_NOTICES.items():
            self.assertEqual(notices[source], (self.dest / name).read_bytes())
        config = json.loads((ROOT / "src-tauri/tauri.runtime.windows.json").read_text())
        self.assertEqual(
            {f"runtime/windows-x64/{name}": name for name in runtime.WINDOWS_FILES}
            | {f"runtime/windows-x64/{name}": f"licenses/{name}" for name in runtime.WINDOWS_NOTICES},
            config["bundle"]["resources"],
        )
        with self.assertRaisesRegex(ValueError, "x64"):
            runtime.verify_x64_dll(pe(0xAA64))

    def test_windows_rejects_missing_notices_without_staging(self):
        path = self.write_zip({f"onnxruntime/capi/{name}": pe() for name in runtime.WINDOWS_FILES})
        with patch.object(runtime, "WINDOWS_SHA256", runtime.file_sha256(path)), self.assertRaises(KeyError):
            runtime.stage_windows(path, self.dest)
        self.assertFalse(self.dest.exists())

    def test_android_stages_arm64_without_other_abis(self):
        archive = self.write_zip({
            runtime.ANDROID_MEMBER: elf(),
            "jni/x86_64/libonnxruntime.so": elf(62),
        })
        pin = runtime.inspect_android(archive)
        with patch.object(runtime, "load_android_pin", return_value=pin):
            runtime.stage_android(archive, self.dest, self.notice_dest)
        self.assertEqual({"libonnxruntime.so"}, {file.name for file in self.dest.iterdir()})
        self.assertEqual(elf(), (self.dest / "libonnxruntime.so").read_bytes())
        for name, digest in runtime.NOTICE_DIGESTS.items():
            self.assertEqual(runtime.file_sha256(self.notice_dest / name), digest)

    def test_android_inspection_reports_observed_archive_and_library_pins(self):
        archive = self.write_zip({runtime.ANDROID_MEMBER: elf(), "META-INF/LICENSE": b"notice"})
        report = runtime.inspect_android(archive)
        self.assertEqual(runtime.ANDROID_URL, report["source"])
        self.assertEqual(report["minimumElfLoadAlignment"], 16384)
        self.assertEqual(hashlib.sha256(archive.read_bytes()).hexdigest(), report["archiveSha256"])
        self.assertEqual(hashlib.sha256(elf()).hexdigest(), report["librarySha256"])
        self.assertEqual(report["notices"], [{"path": "META-INF/LICENSE", "sha256": hashlib.sha256(b"notice").hexdigest()}])
        self.assertFalse(self.dest.exists())

    def test_android_rejects_archive_or_library_mismatch_without_staging(self):
        archive = self.write_zip({runtime.ANDROID_MEMBER: elf()})
        pin = runtime.inspect_android(archive)
        for key in ("archiveSha256", "librarySha256"):
            with self.subTest(key=key), patch.object(runtime, "load_android_pin", return_value=pin | {key: "0" * 64}):
                with self.assertRaisesRegex(ValueError, "SHA-256"):
                    runtime.stage_android(archive, self.dest, self.notice_dest)
                self.assertFalse(self.dest.exists())
                self.assertFalse(self.notice_dest.exists())

    def test_android_rejects_wrong_architecture_and_incompatible_elf_alignment(self):
        for data, error in [(elf(62), "arm64 ELF"), (elf(alignment=4096), "16 KiB"), (elf(virtual_address=4096), "16 KiB"), (elf()[:64], "header table")]:
            with self.subTest(error=error), self.assertRaisesRegex(ValueError, error):
                runtime.verify_elf_load_alignment(data)

    def make_ndk(self):
        ndk = self.root / "ndk"
        lib = ndk / runtime.NDK_SYSROOT_LIB / runtime.CXX_NAME
        lib.parent.mkdir(parents=True)
        lib.write_bytes(elf(build_id=b"NDK fixture"))
        for name in runtime.NDK_NOTICES:
            (ndk / name).write_text(f"Fixture for {name}")
        return ndk

    def test_stages_cxx_and_notices_from_selected_ndk(self):
        ndk = self.make_ndk()
        build_id = runtime.stage_android_cxx(ndk, self.dest)
        self.assertEqual(build_id, b"NDK fixture".hex())
        self.assertEqual((self.dest / "jniLibs/arm64-v8a" / runtime.CXX_NAME).read_bytes(), elf(build_id=b"NDK fixture"))
        for name in runtime.NDK_NOTICES:
            self.assertEqual((self.dest / runtime.APK_NDK_NOTICES / name).read_bytes(), (ndk / name).read_bytes())

    def test_cxx_rejects_invalid_elf_or_missing_build_id(self):
        ndk = self.make_ndk()
        lib = ndk / runtime.NDK_SYSROOT_LIB / runtime.CXX_NAME
        for payload, error in ((elf(alignment=4096), "16 KiB"), (elf(), "no GNU build ID")):
            with self.subTest(error=error):
                lib.write_bytes(payload)
                with self.assertRaisesRegex(ValueError, error):
                    runtime.stage_android_cxx(ndk, self.dest)
                self.assertFalse(self.dest.exists())

    def test_apk_requires_arm64_dependencies_and_matching_notices(self):
        ndk = self.make_ndk()
        notices = {f"{runtime.APK_NOTICE_DIR}/{name}": (runtime.NOTICE_DIR / name).read_bytes() for name in runtime.NOTICE_DIGESTS}
        notices |= {f"{runtime.APK_NDK_NOTICES}/{name}": (ndk / name).read_bytes() for name in runtime.NDK_NOTICES}
        libs = {runtime.APK_APP_MEMBER: elf(), runtime.APK_MEMBER: elf(), runtime.APK_CXX_MEMBER: elf(build_id=b"NDK fixture")}
        apk = libs | notices
        runtime.verify_apk(self.write_zip(apk), ndk)
        # Stripping elsewhere in the file must not invalidate the original build ID.
        runtime.verify_apk(self.write_zip(apk | {runtime.APK_CXX_MEMBER: libs[runtime.APK_CXX_MEMBER] + b"other file data"}), ndk)
        for entries in (
            {}, libs,
            apk | {runtime.APK_APP_MEMBER: elf(alignment=4096)},
            apk | {"lib/x86_64/libextra.so": elf(62)},
            apk | {runtime.APK_CXX_MEMBER: elf(build_id=b"wrong NDK")},
            apk | {f"{runtime.APK_NDK_NOTICES}/NOTICE": b"tampered"},
            {name: value for name, value in apk.items() if name != runtime.APK_CXX_MEMBER},
        ):
            with self.subTest(entries=list(entries)), self.assertRaises(ValueError):
                runtime.verify_apk(self.write_zip(entries), ndk)

    def test_checked_in_pin_matches_current_maven_coordinates(self):
        pin = runtime.load_android_pin()
        self.assertEqual(pin["version"], runtime.VERSION)
        self.assertEqual(pin["source"], runtime.ANDROID_URL)
        runtime.check_android_pin(pin)


if __name__ == "__main__":
    unittest.main()
