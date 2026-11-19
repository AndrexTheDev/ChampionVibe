# CC0 Patois / reggae drop sampleboard

Every `.wav` in this directory is **original synthesis by the ChampionVibe project**, dedicated to the
public domain under [CC0 1.0 Universal](https://creativecommons.org/publicdomain/zero/1.0/). No
third-party master recording, composition, performance or vocal take is involved, so the drops can be
redistributed and used commercially without attribution.

The two `vocal-*` drops are **formant-synthesised shouts**, not recordings of a singer. The UI labels
them accordingly; do not present them as an artist sample or as an authentic Patois voice recording.

| File | Bytes | SHA-256 |
| --- | ---: | --- |
| `airhorn-classic.wav` | 97,064 | `7ef6d994456154e2b68491f498736909c716ce4114924d279feade985888fade` |
| `bass-riddim.wav` | 211,724 | `6144e7d7fc381263ee81475c72ff221f8813e98e7770fef683149895b0c5bc1e` |
| `drum-one-drop.wav` | 176,444 | `95c418bfa86a006dd097b77e8892cac01522c6f9b389c65d455c4f2b164fd3ab` |
| `drum-steppers.wav` | 176,444 | `6ad8d086bf60eae785142b85f30ace8827bedc78126c79a000f977f20f92eb4b` |
| `fx-tape-rewind.wav` | 97,064 | `a7ca0011f23aade82524b8b351d22f4f37bdded93073160a1382c05000deb3aa` |
| `siren-dub-sweep.wav` | 176,444 | `3cf7f2e57fbf3edaeb520f2ab5a4dc8ef1fdac5db7a46e548bd7fcf4c8406bdd` |
| `siren-ragga.wav` | 141,164 | `093e0917c922d9112a5f1672e70848185fe4d4bd6c4e845806a7b16c3da023f1` |
| `skank-chord.wav` | 105,884 | `780b3c85e0765490d3d66ddadea97be96f6308e22fb9aad5b14f3c09b00e472e` |
| `vocal-hey.wav` | 66,194 | `b26702aa44a8bca54df2b34beb3094b16f840be861f1410af2bb3a5e5b4544bb` |
| `vocal-pull-up.wav` | 119,114 | `93c26b638d5f196bf58d3da93afda4f170760fc9368aab979f00476e8eea6a1e` |

Total: 1,367,540 bytes (10 files). Format: 44.1 kHz, mono, 16-bit PCM WAV.

## Reproducing the board

```sh
python3 scripts/generate-cc0-reggae-drops.py          # rewrite the board
python3 scripts/generate-cc0-reggae-drops.py --check   # fail if any byte differs from the committed file
```

The generator is deterministic (seed `0x5EED1975`): identical input always yields identical bytes, so
the hashes above are stable and the files can be regenerated from source at any time. `audio/samples.rs`
embeds the same files with `include_bytes!` and tags each clip with `CC0-1.0 · ChampionVibe original
synthesis` so the licence travels with the sample deck.
