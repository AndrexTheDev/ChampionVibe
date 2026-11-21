# Android release signing

The manual **Final packaging** workflow builds the Android release candidate, aligns it, signs it, and verifies that its only signer matches the expected SHA-256 certificate fingerprint. It uploads only the verified APK, never the Gradle intermediate or the keystore. It does not publish a GitHub Release or upload to Google Play.

## Configure GitHub once

Create an Actions environment named `android-release`. Restrict deployments to trusted release refs (for example, protected `main` or release tags) and configure approval where available. Keep the long-term release key backed up securely. Android updates must use the same signing identity.

| Type | Name | Value |
| --- | --- | --- |
| Secret | `ANDROID_RELEASE_KEYSTORE_BASE64` | Base64 of the release keystore file (JKS or PKCS#12). Base64 is not encryption. |
| Secret | `ANDROID_RELEASE_STORE_PASSWORD` | Password for the keystore. |
| Secret | `ANDROID_RELEASE_KEY_PASSWORD` | Password for the signing key entry. |
| Variable | `ANDROID_RELEASE_KEY_ALIAS` | Alias of the release key in the keystore. |
| Variable | `ANDROID_RELEASE_CERT_SHA256` | SHA-256 fingerprint of the intended signing certificate. Colons and letter case are accepted. |

Obtain the public fingerprint from a copy of the trusted keystore with `keytool -list -v -keystore <file> -alias <alias>`. Enter the password at the prompt. Do not commit the keystore, its base64 representation, or passwords. Do not paste them into an issue, comment, or chat. If using Google Play App Signing later, a Play upload key can be different from the Play app-signing key; plan the signing identity before distributing sideloaded APKs that users will need to update.

## What the workflow verifies

- Exactly one Gradle **release** APK exists. Debug builds and ambiguous output are not accepted.
- `zipalign` aligns the APK before signing and checks the finished archive with the 16 KB page-alignment option. This does not prove that each ELF library or the app works on a 16 KB device.
- Android SDK `apksigner` verifies the APK and its certificate fingerprint; the workflow also verifies the arm64 ONNX Runtime entry and bundled ONNX notices and writes `SHA256SUMS.txt` for the finished APK.
- Ordinary CI runs `python3 tests/android_release_signing_test.py` with simulated tools. It does not build an APK, read any release secrets, or substitute for the final signature check.

Windows installers have a separate `windows-packaging` environment and per-file SHA-256 checksums. This workflow does not configure a Windows Authenticode signing identity; a checksum is not a code signature.
