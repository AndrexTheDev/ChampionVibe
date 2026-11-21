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


def elf(machine=183):
    result = bytearray(64)
    result[:7] = b"\x7fELF\x02\x01\x01"
    struct.pack_into("<HH", result, 16, 3, machine)
    return bytes(result)


class RuntimePackagingTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        self.dest = self.root / "stage"

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
        runtime.stage_android(archive, self.dest, runtime.file_sha256(archive))
        self.assertEqual({"libonnxruntime.so"}, {file.name for file in self.dest.iterdir()})
        self.assertEqual(elf(), (self.dest / "libonnxruntime.so").read_bytes())

    def test_android_inspection_reports_observed_archive_and_library_pins(self):
        archive = self.write_zip({runtime.ANDROID_MEMBER: elf(), "META-INF/LICENSE": b"notice"})
        report = runtime.inspect_android(archive)
        self.assertEqual(runtime.ANDROID_URL, report["source"])
        self.assertEqual(hashlib.sha256(archive.read_bytes()).hexdigest(), report["archiveSha256"])
        self.assertEqual(hashlib.sha256(elf()).hexdigest(), report["librarySha256"])
        self.assertEqual(report["notices"], [{"path": "META-INF/LICENSE", "sha256": hashlib.sha256(b"notice").hexdigest()}])
        self.assertFalse(self.dest.exists())

    def test_android_rejects_missing_or_incorrect_pin_before_staging(self):
        archive = self.write_zip({runtime.ANDROID_MEMBER: elf()})
        for pin in ("", "not-a-digest", "0" * 64):
            with self.subTest(pin=pin), self.assertRaisesRegex(ValueError, "SHA-256"):
                runtime.stage_android(archive, self.dest, pin)
            self.assertFalse(self.dest.exists())

    def test_android_rejects_wrong_library_before_staging(self):
        archive = self.write_zip({runtime.ANDROID_MEMBER: elf(62)})
        with self.assertRaisesRegex(ValueError, "arm64 ELF"):
            runtime.stage_android(archive, self.dest, runtime.file_sha256(archive))
        self.assertFalse(self.dest.exists())

    def test_apk_requires_one_correctly_named_arm64_library(self):
        runtime.verify_apk(self.write_zip({runtime.APK_MEMBER: elf()}))
        for entries in (
            {},
            {runtime.APK_MEMBER: elf(62)},
            {runtime.APK_MEMBER: elf(), "lib/x86_64/libonnxruntime.so": elf(62)},
        ):
            with self.subTest(entries=list(entries)), self.assertRaises(ValueError):
                runtime.verify_apk(self.write_zip(entries))


if __name__ == "__main__":
    unittest.main()
