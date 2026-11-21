"""Exercise selected-platform artifact downloads against an offline gh stub."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "scripts/download-actions-builds.sh"
STUB = '''#!/usr/bin/env bash
set -eu
case "$1 $2" in
  'run view') echo "$RUN_CONCLUSION" ;;
  'api --paginate') printf '%s\\n' "$AVAILABLE_ARTIFACTS" ;;
  'run download')
    artifact="$7"
    dest="$9"
    printf '%s\\n' "$artifact" >> "$CALLS"
    printf 'fixture\\n' > "$dest/SHA256SUMS.txt"
    case "$artifact" in
      championvibe-android-apk|championvibe-android-test-apk) touch "$dest/app.apk" ;;
      championvibe-windows-test-build) touch "$dest/app.msi" "$dest/setup.exe" ;;
      *) exit 2 ;;
    esac ;;
  *) exit 2 ;;
esac
'''


class DownloadTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        tool = self.root / "gh"
        tool.write_text(STUB)
        tool.chmod(0o755)
        self.calls = self.root / "calls"
        self.dest = self.root / "artifacts"

    def download(self, names, conclusion="success"):
        return subprocess.run(["bash", str(SCRIPT), "12345", str(self.dest)], env=os.environ | {
            "PATH": f"{self.root}:{os.environ['PATH']}",
            "GH_REPO": "example/fixture",
            "RUN_CONCLUSION": conclusion,
            "AVAILABLE_ARTIFACTS": "\n".join(names),
            "CALLS": str(self.calls),
        }, capture_output=True, text=True)

    def test_android_only(self):
        result = self.download(["android-gradle-runtime-licenses", "championvibe-android-apk"])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.calls.read_text().splitlines(), ["championvibe-android-apk"])
        self.assertTrue((self.dest / "android/app.apk").is_file())
        self.assertFalse((self.dest / "windows").exists())

    def test_disposable_test_apk_stays_separate_from_release(self):
        result = self.download(["championvibe-android-test-apk"])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.calls.read_text().splitlines(), ["championvibe-android-test-apk"])
        self.assertTrue((self.dest / "android-test/app.apk").is_file())
        self.assertFalse((self.dest / "android").exists())

    def test_windows_only(self):
        result = self.download(["championvibe-windows-test-build"])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.calls.read_text().splitlines(), ["championvibe-windows-test-build"])
        self.assertFalse((self.dest / "android").exists())

    def test_both(self):
        result = self.download(["championvibe-android-apk", "championvibe-windows-test-build"])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((self.dest / "android/app.apk").is_file())
        self.assertTrue((self.dest / "windows/setup.exe").is_file())

    def test_missing_or_failed_run(self):
        self.assertIn("no unexpired", self.download(["unrelated"]).stderr)
        self.assertIn("did not finish successfully", self.download(["championvibe-android-apk"], "failure").stderr)
        self.assertFalse(self.calls.exists())


if __name__ == '__main__':
    unittest.main()
