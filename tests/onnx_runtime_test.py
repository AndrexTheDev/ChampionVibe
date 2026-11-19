"""Offline checks for the release runtime stager; no native build is required."""

import base64
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import struct
import tarfile
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
        path = self.root / "untrusted.tgz"
        path.write_bytes(b"not the expected archive")
        with self.assertRaisesRegex(ValueError, "SHA-512"):
            runtime.stage_windows(path, self.dest)
        self.assertFalse(self.dest.exists())

    def test_windows_extracts_only_the_four_configured_x64_dlls(self):
        path = self.root / "runtime.tgz"
        with tarfile.open(path, "w:gz") as tar:
            for name in (*runtime.WINDOWS_FILES, "not-for-the-installer.node"):
                header = tarfile.TarInfo(runtime.WINDOWS_PREFIX + name)
                header.size = len(pe())
                tar.addfile(header, io.BytesIO(pe()))
        pin = base64.b64encode(hashlib.sha512(path.read_bytes()).digest()).decode()
        with patch.object(runtime, "WINDOWS_SHA512", pin):
            runtime.stage_windows(path, self.dest)
        self.assertEqual(set(runtime.WINDOWS_FILES), {file.name for file in self.dest.iterdir()})
        config = json.loads((ROOT / "src-tauri/tauri.runtime.windows.json").read_text())
        self.assertEqual(
            {f"runtime/windows-x64/{name}": name for name in runtime.WINDOWS_FILES},
            config["bundle"]["resources"],
        )
        with self.assertRaisesRegex(ValueError, "x64"):
            runtime.verify_x64_dll(pe(0xAA64))

    def test_android_stages_arm64_without_other_abis(self):
        archive = self.write_zip({
            runtime.ANDROID_MEMBER: elf(),
            "jni/x86_64/libonnxruntime.so": elf(62),
        })
        runtime.stage_android(archive, self.dest)
        self.assertEqual({"libonnxruntime.so"}, {file.name for file in self.dest.iterdir()})
        self.assertEqual(elf(), (self.dest / "libonnxruntime.so").read_bytes())

    def test_android_rejects_wrong_library_before_staging(self):
        archive = self.write_zip({runtime.ANDROID_MEMBER: elf(62)})
        with self.assertRaisesRegex(ValueError, "arm64 ELF"):
            runtime.stage_android(archive, self.dest)
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
