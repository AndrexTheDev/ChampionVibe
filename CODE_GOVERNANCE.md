# Model and licence governance

This document is the policy that keeps ChampionVibe commercially shippable. It is enforced by code and
CI, not by good intentions.

## Rules

1. **No unclear-rights download path.** Every artifact that the app can download is listed in
   `src-tauri/assets/model-registry.json` under `artifacts`. An artifact is admissible only if its code
   *and* its weights are usable commercially and a SHA-256 pin exists. HT-Demucs, KUIELab/UVR MDX
   mirrors and Open-Unmix are in `policy.forbiddenDownloadSources` and must never be added as a
   download target.
2. **Only cleared tier is installable.** `policy.clearedTierOnly: true`; a `tier` other than `cleared`
   is rejected by the loader. A pin of `PENDING_VERIFICATION` makes the artifact non-installable until
   CI has verified the bytes.
3. **Pins are evidence, not claims.** `scripts/verify-model-pins.sh` downloads each artifact, checks
   byte size and SHA-256, cross-checks the upstream checksum file when it lists the artifact, and
   writes `reports/model-pin-verification.json`. `.github/workflows/model-pins.yml` runs this on every
   change to the registry, commits the report back to the branch, and fails the run when a pin does not
   verify.
4. **BYOM never redistributes.** User-supplied separators are validated for structure and tensor shapes
   only; the app stores the local path plus the file hash in `byom-models.json` and requires an explicit
   acknowledgement for kinds whose rights are unclear. If a BYOM model later becomes a *default*, it
   must first be re-admitted through rule 1 (cleared licence + verified pin).
5. **Licence text travels with the feature.** Every catalogue entry carries a `rights_note`, the UI
   shows it next to the download button, and `THIRD-PARTY-NOTICES.md` plus `docs/model-registry.md` are
   updated in the same change as any model change.
6. **Transparent TTS naming.** The local TTS engine must always be presented as a US English AI voice
   with a Patois pronunciation approximation in all four locales (`de`, `en`, `es`, `fr`) and in the UI.
   Never claim an authentic Patois voice.
7. **Locked builds.** `Cargo.lock` is checked in and regenerated only by the `Model pins and lockfile`
   workflow so CI and release builds resolve identical dependency versions.

## Change checklist

- [ ] New artifact has a commercial-clearance statement and an upstream `checksumSource` where one exists
- [ ] `src-tauri/assets/model-registry.json` updated (URL, asset, members, size, `maxBytes`, licence summary, notices)
- [ ] `scripts/verify-model-pins.sh` produces a clean `reports/model-pin-verification.json` for the new pin
- [ ] `docs/model-registry.md` and `THIRD-PARTY-NOTICES.md` updated
- [ ] Frontend catalogue copy and `src/model-library.test.js` updated
- [ ] `npm test` and the Rust workspace tests pass

## Review and removal

Any artifact whose rights statement cannot be reproduced from a primary source is removed from the
registry in the same change that adds the finding. Removing a download path is never a breaking change
for user data: BYOM records keep working because they only store local paths.
