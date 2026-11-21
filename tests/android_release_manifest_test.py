"""Regression tests for the no-APK Android release manifest check."""

import importlib.util
from pathlib import Path
import unittest
from xml.etree import ElementTree

SPEC = importlib.util.spec_from_file_location(
    "manifest_verifier", Path(__file__).resolve().parents[1] / "scripts/verify-android-release-manifest.py"
)
verifier = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(verifier)

FIXTURE = '''<manifest xmlns:android="http://schemas.android.com/apk/res/android" package="com.championvibe.app">
    <uses-sdk android:minSdkVersion="26" android:targetSdkVersion="36"/>
    <uses-permission android:name="android.permission.INTERNET"/>
    <application android:usesCleartextTraffic="false">
        <provider android:authorities="com.championvibe.app.fileprovider" android:exported="false" android:grantUriPermissions="true"/>
    </application>
</manifest>'''


class AndroidManifestTests(unittest.TestCase):
    def check(self, xml):
        return verifier.verify(ElementTree.fromstring(xml), "com.championvibe.app", 26, 36)

    def test_expected_release(self):
        self.assertEqual(self.check(FIXTURE), ["android.permission.INTERNET"])

    def test_rejects_release_regressions(self):
        for old, new, error in (
            ('package="com.championvibe.app"', 'package="com.championvibe.app.debug"', "application ID"),
            ('minSdkVersion="26"', 'minSdkVersion="24"', "minSdk"),
            ('targetSdkVersion="36"', 'targetSdkVersion="37"', "targetSdk"),
            ('<application ', '<application android:debuggable="true" ', "production"),
            ('<application ', '<application android:testOnly="true" ', "production"),
            ('usesCleartextTraffic="false"', 'usesCleartextTraffic="true"', "cleartext"),
            ('android:exported="false"', 'android:exported="true"', "private app FileProvider"),
            ('<application ', '<uses-permission-sdk-23 android:name="android.permission.RECORD_AUDIO"/><application ', "RECORD_AUDIO"),
            ('<application ', '<uses-permission android:name="android.permission.READ_MEDIA_AUDIO"/><application ', "READ_MEDIA_AUDIO"),
            ('android.permission.INTERNET', 'android.permission.ACCESS_NETWORK_STATE', "missing Android INTERNET"),
        ):
            with self.subTest(error=error), self.assertRaisesRegex(ValueError, error):
                self.check(FIXTURE.replace(old, new))


if __name__ == "__main__":
    unittest.main()
