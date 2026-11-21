"""Test the APK release policy with tool stubs; don't build or sign a real APK."""

import base64
import importlib.util
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location(
    "android_signer", Path(__file__).resolve().parents[1] / "scripts/sign-android-release.py"
)
signer = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(signer)
DIGEST = "12ab" * 16


class SigningTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        self.build_tools = self.root / "build-tools"
        self.source = self.root / "app/build/outputs/apk/universal/release/candidate-unsigned.apk"
        self.source.parent.mkdir(parents=True)
        self.source.write_bytes(b"candidate fixture")
        self.output = self.root / "artifacts/final.apk"
        self.environment = dict(zip(signer.SIGNING_ENV, (
            base64.b64encode(b"fixture keystore").decode(), "store-pw", "release", "key-pw", DIGEST,
        )))
        self.environment["PATH"] = "/bin"
        self.commands = []
        self.verify_stdout = f"Signer #1 certificate SHA-256 digest: {DIGEST}\n"

    def stub_run(self, argv, **kwargs):
        tool = Path(argv[0]).name
        subcommand = argv[1]
        self.commands.append((tool, subcommand))
        self.assertTrue(kwargs['check'])
        self.assertNotIn(self.environment[signer.SIGNING_ENV[1]], argv)
        self.assertNotIn(self.environment[signer.SIGNING_ENV[3]], argv)
        self.assertNotIn(signer.SIGNING_ENV[0], kwargs['env'])
        if tool == "apksigner" and subcommand == "sign":
            self.assertEqual(kwargs['env'][signer.SIGNING_ENV[1]], "store-pw")
            self.assertEqual(kwargs['env'][signer.SIGNING_ENV[3]], "key-pw")
            path = Path(argv[argv.index('--ks') + 1])
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)
            self.assertEqual(path.read_bytes(), b"fixture keystore")
            Path(argv[argv.index('--out') + 1]).write_bytes(b"signed fixture")
        else:
            self.assertEqual(kwargs['env'], {'PATH': '/bin'})
        if tool == "apksigner" and subcommand == "verify":
            return subprocess.CompletedProcess(argv, 0, stdout=self.verify_stdout)
        return subprocess.CompletedProcess(argv, 0)

    def sign_with_stubs(self):
        with patch.object(signer.os, 'access', return_value=True), patch.object(signer.subprocess, 'run', side_effect=self.stub_run):
            signer.sign_apk(self.source, self.output, self.build_tools, self.environment)

    def test_publishes_only_after_alignment_and_pinned_signer_verification(self):
        self.sign_with_stubs()
        self.assertEqual(self.commands, [('zipalign', '-P'), ('apksigner', 'sign'), ('zipalign', '-c'), ('apksigner', 'verify')])
        self.assertEqual(self.output.read_bytes(), b"signed fixture")
        self.assertEqual(list(self.output.parent.iterdir()), [self.output])

    def test_rejects_wrong_and_multiple_signers_without_publishing(self):
        for output in (
            f"Signer #1 certificate SHA-256 digest: {'fb' * 32}\n",
            self.verify_stdout + f"Signer #2 certificate SHA-256 digest: {DIGEST}\n",
            "",
        ):
            with self.subTest(signer_output=output):
                self.verify_stdout = output
                with self.assertRaisesRegex(ValueError, "expected release signer"):
                    self.sign_with_stubs()
                self.assertEqual(list(self.output.parent.iterdir()), [])

    def test_failed_verification_command_leaves_no_output(self):
        run = self.stub_run

        def invalid_signature(argv, **kwargs):
            if argv[1] == 'verify':
                raise subprocess.CalledProcessError(1, argv)
            return run(argv, **kwargs)

        with patch.object(signer.os, 'access', return_value=True), patch.object(signer.subprocess, 'run', side_effect=invalid_signature):
            with self.assertRaises(subprocess.CalledProcessError):
                signer.sign_apk(self.source, self.output, self.build_tools, self.environment)
        self.assertEqual(list(self.output.parent.iterdir()), [])

    def test_fails_closed_when_configuration_is_missing_or_invalid(self):
        for name, value in ((signer.SIGNING_ENV[1], ''), (signer.SIGNING_ENV[0], '!'), (signer.SIGNING_ENV[4], '12')):
            with self.subTest(name=name), self.assertRaises(ValueError):
                signer.load_credentials(self.environment | {name: value})
        formatted = ':'.join(DIGEST[index:index + 2].upper() for index in range(0, 64, 2))
        self.assertEqual(signer.normalize_digest(formatted), DIGEST)

    def test_ignores_debug_apks_and_rejects_ambiguous_release_outputs(self):
        debug = self.source.parent.parent / "debug" / "other.apk"
        debug.parent.mkdir()
        debug.touch()
        self.assertEqual(signer.find_release_apk(self.root), self.source)
        self.source.with_name('another-release.apk').touch()
        with self.assertRaisesRegex(ValueError, 'found 2'):
            signer.find_release_apk(self.root)


if __name__ == '__main__':
    unittest.main()
