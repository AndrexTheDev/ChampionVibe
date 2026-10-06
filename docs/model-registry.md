# Local model registry and separator contracts

ChampionVibe stores optional weights under the app-data `models/` directory. Nothing in this registry is bundled with the app. A download is an explicit user action over HTTPS; the Rust manager limits its size, follows only a final HTTPS Hugging Face/Xet URL, computes SHA-256 while the bytes are on disk, validates the artifact contract, and only then installs it. Model inference and audio processing remain local.

## Pinned registry

All revisions below are immutable repository commits. The SHA-256 pins are content hashes, not license grants.

| Registry ID / file | Repository revision and path | SHA-256 | Use |
| --- | --- | --- | --- |
| `separator-android` · `kuielab_a_vocals.onnx` | `Politrees/UVR_resources@491e44cfaf909b73ce14f6185298de9c82867de3` · `MDXNet_models/kuielab_a_vocals.onnx` | `daba83c2ee1afee9139766ad64c9b6808d6b6f092fff04bed3338be50baac721` | Two-stem manifest; predicts vocals, then writes instrumental as input minus vocals |
| `separator-four-stem` · `kuielab_a_vocals.onnx` | Same revision · `MDXNet_models/kuielab_a_vocals.onnx` | `daba83c2ee1afee9139766ad64c9b6808d6b6f092fff04bed3338be50baac721` | Four-stem vocals target |
| `separator-four-stem` · `kuielab_a_other.onnx` | Same revision · `MDXNet_models/kuielab_a_other.onnx` | `7b67a1dcb5f232153528c59960b4c7bf8dc736b8114de360af0e719633f53358` | Four-stem `Other` / instrumental target |
| `separator-four-stem` · `kuielab_a_bass.onnx` | Same revision · `MDXNet_models/kuielab_a_bass.onnx` | `0c3e77b9963185b1ea6bb46a4b8924137d9370fc1ccdefec7b1b416ef550dcaa` | Four-stem bass target |
| `separator-four-stem` · `kuielab_a_drums.onnx` | Same revision · `MDXNet_models/kuielab_a_drums.onnx` | `40f586b7091934dd6f5563f0cba8f14bad57ce88440da1098bf388ea716c2901` | Four-stem drums target |
| `whisper-tiny` · `ggml-tiny.bin` | `ggerganov/whisper.cpp@80da2d8bfee42b0e836fc3a9890373e5defc00a6` · `ggml-tiny.bin` | `be07e048e1e599ad46341c8d2a135645097a538221678b7acdd1b1919c6e1b21` | Local Whisper Tiny transcription |
| `kokoro-model` · `model_quantized.onnx` | `onnx-community/Kokoro-82M-v1.0-ONNX@468588286ebb2dd77c25b9771e5d165896538cce` · `onnx/model_quantized.onnx` | `fbae9257e1e05ffc727e951ef9b9c98418e6d79f1c9b6b13bd59f5c9028a1478` | Optional, user-selected local Kokoro WebAssembly TTS model; English G2P plus lightweight spelling cues are not a complete Patois model |
| `kokoro-voice-af-heart` · `af_heart.bin` | Same Kokoro revision · `voices/af_heart.bin` | `d583ccff3cdca2f7fae535cb998ac07e9fcb90f09737b9a41fa2734ec44a8f0b` | Optional voice/style vector for US-English synthesis; not Jamaican Patois |

The four KUIELab files are each about 29.7 MB. Whisper Tiny is about 77.7 MB; the quantized Kokoro ONNX is about 92.4 MB and its selected voice pack about 522 kB. Sizes are informational; the downloader enforces hard byte limits and trusts hashes, not advertised lengths.

## MDX-Net manifest contract

MDX-Net is selected through a ChampionVibe `.cvmodel.json` manifest. The manifest pins every file and declares the target stem, FFT size, 1024-sample hop, 2048 retained frequency bins, 512 STFT frames, and amplitude compensation. The verified KUIELab Track A configuration is:

| Target | `n_fft` | ChampionVibe lane |
| --- | ---: | --- |
| vocals | 6,144 | acapella |
| other | 8,192 | instrumental |
| bass | 16,384 | bass |
| drums | 4,096 | drums |

The ONNX graph must accept and emit float32 `[1, 4, 2048, 512]`. The four input channels are left-real, left-imaginary, right-real, right-imaginary. The Rust path reproduces the periodic Hann, centered STFT/iSTFT, source-specific frequency cut, and overlapping chunks from the KUIELab reference. Each file is inferred independently. The two-stem profile uses only the vocals predictor and builds the aligned instrumental as mixture minus predicted vocals. The four-stem profile predicts vocals, Other, bass and drums independently; `Other` maps to the app's instrumental lane.

The existing `.onnx` HT-Demucs path is retained. A plain `.onnx` selection uses the HT-Demucs tensor contract; user MDX files must first be imported into a managed manifest. Two- and four-stem tracks are stored with only their actual output paths. Playback chooses the matching active two/four-stem profile, including when a Pro desktop or Android player loads a two-stem track.

The app runs MDX on CPU by default. On Windows only, the existing opt-in DirectML control may request DirectML; it is never silently enabled. CPU inference holds at most one MDX target session at a time so the four-stem set does not require four resident inference sessions on Android.

User-selected MDX files are checked for ONNX validity, float32 element types, rank/channel/frequency/frame dimensions, exact target names (for four-stem sets), and valid local paths before copying. Their source hashes are stored in the managed manifest and checked again before inference. A user-provided hash establishes post-import integrity, not trusted provenance or rights.

## Licensing gates

- The KUIELab reference code repository declares MIT. Its Track A material references MUSDB18-HQ and uses a mirrored weights repository; the source-code license does **not** establish commercial permission for those weight files, the underlying training data, or the mirror. The KUIELab model set remains an explicit rights-review item; do not advertise commercial clearance.
- OpenAI Whisper / `whisper.cpp` source repositories declare MIT. Retain their notices if redistributing code or the pinned weights; the model pin does not independently resolve every upstream-data question.
- The pinned Kokoro model repository declares Apache-2.0, but runtime, ONNX conversion, voice pack, tokenizer, phonemizer, G2P and any pronunciation dictionary must each be reviewed separately. Misaki/CMUdict data terms and provenance are not bundled into a blanket code license. A local WebAssembly synthesis path and approximate word-level Patois spelling substitutions before US-English G2P are implemented; tests cover the transformations and a mocked model path, not an authentic Patois voice or a fully validated real-model/device workflow. The substitutions are not a complete or authoritative Jamaican Creole G2P, and `af_heart` is US English, not Jamaican Patois. Hype/aggressive prosody uses Kokoro's speed input plus local post-synthesis varispeed pitch/intonation; it is not model-native emotion control and may alter timbre. Vet the model, conversion, voice and dependencies independently before redistribution.
- The opt-in Piper feature remains gated pending its runtime/eSpeak and per-voice redistribution review. Do not enable it for a release without completing that review.

## External references

- [KUIELab MDX-Net submission and reference preprocessing](https://github.com/kuielab/mdx-net-submission/tree/leaderboard_A)
- [Pinned KUIELab resource tree](https://huggingface.co/Politrees/UVR_resources/tree/491e44cfaf909b73ce14f6185298de9c82867de3)
- [Pinned Whisper Tiny model commit](https://huggingface.co/ggerganov/whisper.cpp/commit/80da2d8bfee42b0e836fc3a9890373e5defc00a6)
- [Pinned Kokoro model commit](https://huggingface.co/onnx-community/Kokoro-82M-v1.0-ONNX/commit/468588286ebb2dd77c25b9771e5d165896538cce)
- [KUIELab-MDX-Net paper](https://mdx-workshop.github.io/proceedings/mkim.pdf)
