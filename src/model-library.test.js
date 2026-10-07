import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

const html = readFileSync(new URL('../index.html', import.meta.url), 'utf8');
const main = readFileSync(new URL('./main.js', import.meta.url), 'utf8');
const indexHtml = readFileSync(new URL('../index.html', import.meta.url), 'utf8');
const manager = readFileSync(new URL('../src-tauri/src/audio/model_manager.rs', import.meta.url), 'utf8');
const commands = readFileSync(new URL('../src-tauri/src/commands.rs', import.meta.url), 'utf8');
const commandRegistry = readFileSync(new URL('../src-tauri/src/lib.rs', import.meta.url), 'utf8');
const mdx = readFileSync(new URL('../src-tauri/src/import/mdx.rs', import.meta.url), 'utf8');
const wasmTts = readFileSync(new URL('./services/tts_wasm.js', import.meta.url), 'utf8');
const ciWorkflow = readFileSync(new URL('../.github/workflows/ci.yml', import.meta.url), 'utf8');
const artifactDownloadScript = readFileSync(new URL('../scripts/download-actions-builds.sh', import.meta.url), 'utf8');
const registry = JSON.parse(readFileSync(new URL('../src-tauri/assets/model-registry.json', import.meta.url), 'utf8'));
const pinScript = readFileSync(new URL('../scripts/verify-model-pins.sh', import.meta.url), 'utf8');
const modelLoader = readFileSync(new URL('../src-tauri/src/audio/model_loader.rs', import.meta.url), 'utf8');
const spleeter = readFileSync(new URL('../src-tauri/src/import/spleeter.rs', import.meta.url), 'utf8');
const samples = readFileSync(new URL('../src-tauri/src/audio/samples.rs', import.meta.url), 'utf8');
const pinWorkflow = readFileSync(new URL('../.github/workflows/model-pins.yml', import.meta.url), 'utf8');
const separatorSource = readFileSync(new URL('../src-tauri/src/import/separator.rs', import.meta.url), 'utf8');
const importSource = readFileSync(new URL('../src-tauri/src/import/mod.rs', import.meta.url), 'utf8');

test('the pin workflow verifies artifacts and locks Cargo.lock into the branch', () => {
  assert.match(pinWorkflow, /cargo generate-lockfile/u);
  assert.match(pinWorkflow, /Cargo\.lock/u);
  assert.match(pinWorkflow, /scripts\/ci-commit-file\.sh/u);
  assert.match(pinWorkflow, /contents: write/u);
  assert.doesNotMatch(readFileSync(new URL('../.gitignore', import.meta.url), 'utf8'), /^Cargo\.lock$/mu);
});

test('GitHub Actions runs on push and exposes downloadable Windows and Android artifacts', () => {
  assert.match(ciWorkflow, /^on:\n  push:\n/m);
  assert.match(ciWorkflow, /championvibe-windows-test-build/u);
  assert.match(ciWorkflow, /championvibe-android-apk/u);
  assert.match(ciWorkflow, /retention-days: 14/u);
  assert.match(artifactDownloadScript, /gh run download/u);
  assert.match(artifactDownloadScript, /gh release download/u);
});

test('the local model library sits after Whisper selection and before runtime options', () => {
  const whisper = html.indexOf('id="whisper-model-path"');
  const library = html.indexOf('class="model-library-disclosure"');
  const runtime = html.indexOf('class="import-runtime-options"');
  assert.ok(whisper >= 0 && library > whisper && runtime > library);
  assert.match(html, /id="managed-model-catalog"/u);
  assert.match(html, /id="refresh-model-catalog"/u);
  assert.match(html, /id="import-custom-mdx"/u);
  assert.match(html, /value="two-stem"/u);
  assert.match(html, /value="four-stem"/u);
  assert.match(html, /KUIELab-Gewichte und Trainingsdaten sind vor kommerziellem Einsatz separat zu klären/u);
  assert.match(html, /Kokoro-TTS läuft lokal mit englischer G2P/u);
  assert.match(html, /Lokale Modelldatei \(\.onnx\)/u);
  assert.match(html, /id="download-default-separator"/u);
  assert.match(html, /id="byom-dropzone"/u);
  assert.match(html, /id="byom-import"/u);
  assert.match(html, /id="byom-model-kind"/u);
  assert.match(html, /id="byom-rights-ack"/u);
  assert.match(html, /id="byom-model-list"/u);
  assert.match(html, /bleibt auf diesem Gerät, wird niemals hochgeladen/u);
  assert.match(html, /Ich bestätige, dass ich die Rechte an diesen Gewichten geprüft habe/u);
  assert.match(html, /Spleeter-2-Stem/u);
});

test('managed artifacts use unique hard-coded SHA-256 pins and verify before install', () => {
  const pins = [...manager.matchAll(/sha256:\s*"([a-f0-9]{64})"/gu)].map(([, pin]) => pin);
  assert.equal(pins.length, 3, 'Whisper, Kokoro ONNX, and Kokoro voice each keep a hard pin');
  assert.equal(new Set(pins).size, pins.length, 'artifact pins must not be accidentally reused');
  for (const id of ['whisper-tiny', 'kokoro-model', 'kokoro-voice-af-heart']) {
    assert.ok(manager.includes(`"${id}"`), `${id} must be present in the built-in catalog`);
  }
  for (const id of ['separator-android', 'separator-four-stem']) {
    assert.ok(
      !manager.includes(id),
      `${id} must be gone: separator weights are no longer downloadable from the managed catalog`,
    );
  }
  assert.match(manager, /actual_hash\.eq_ignore_ascii_case\(artifact\.sha256\)/u);
  assert.match(manager, /validate_artifact\(artifact, &temporary\)[\s\S]*?fs::rename\(&temporary, &final_path\)/u);
  assert.match(manager, /fn import_user_separator_models/u);
  assert.match(manager, /validate_manifest_models\(/u);
});

test('the standard separator is the cleared, SHA-256 pinned Spleeter 2-stem ONNX profile', () => {
  assert.equal(registry.format, 'championvibe-model-registry-v2');
  assert.equal(registry.policy.defaultSeparatorId, 'spleeter-2stems-onnx');
  assert.equal(registry.policy.clearedTierOnly, true);
  for (const forbidden of [
    'facebookresearch/demucs',
    'adefossez/demucs',
    'sigsep/open-unmix-pytorch',
    'Politrees/UVR_resources',
  ]) {
    assert.ok(
      registry.policy.forbiddenDownloadSources.includes(forbidden),
      `${forbidden} must stay on the forbidden download list`,
    );
  }
  const ids = registry.artifacts.map((artifact) => artifact.id);
  assert.deepEqual(ids, ['spleeter-2stems-onnx', 'spleeter-2stems-onnx-fp16']);
  for (const artifact of registry.artifacts) {
    assert.equal(artifact.tier, 'cleared');
    assert.match(artifact.sha256, /^[a-f0-9]{64}$/u, `${artifact.id} needs a full SHA-256 pin`);
    assert.ok(artifact.sizeBytes > 1_000_000, `${artifact.id} needs a verified byte size`);
    assert.match(artifact.url, /^https:\/\/github\.com\/k2-fsa\/sherpa-onnx\/releases\/download\//u);
    assert.match(artifact.licenseSummary, /MIT/u);
  }
  assert.match(pinScript, /forbiddenDownloadSources/u);
  assert.match(pinScript, /--allow-pending/u);
  assert.match(pinScript, /championvibe-model-pin-verification-v1/u);
});

test('the two-tier loader keeps cleared downloads separate from BYOM imports', () => {
  assert.match(modelLoader, /championvibe-byom-v1/u);
  assert.match(modelLoader, /byom-models\.json/u);
  assert.match(modelLoader, /fn import_byom\(/u);
  assert.match(modelLoader, /fn validate_byom_file\(/u);
  assert.match(modelLoader, /fn download_separator\(/u);
  assert.match(modelLoader, /never downloads, copies or redistributes/u);
  assert.match(modelLoader, /PENDING_VERIFICATION/u);
  assert.match(main, /invoke\('validate_byom_model', \{ path, kind \}\)/u);
  assert.match(main, /invoke\('import_byom_model', \{ path, kind, acknowledged, displayName: null \}\)/u);
  assert.match(main, /invoke\('download_default_separator', \{\}\)/u);
  assert.match(main, /invoke\('list_byom_models'\)/u);
  assert.match(main, /invoke\('remove_byom_model', \{ id \}\)/u);
  assert.match(commands, /pub fn get_separator_catalog/u);
  assert.match(commands, /pub async fn download_default_separator/u);
  assert.match(commands, /pub fn import_byom_model/u);
  assert.match(commands, /pub fn validate_byom_model/u);
  for (const registered of [
    'commands::download_default_separator',
    'commands::get_separator_catalog',
    'commands::validate_byom_model',
    'commands::import_byom_model',
    'commands::remove_byom_model',
  ]) {
    assert.ok(commandRegistry.includes(registered), `${registered} must be registered in lib.rs`);
  }
});

test('the Spleeter backend follows the verified sherpa-onnx runtime contract', () => {
  assert.match(modelLoader, /SPLEETER_N_FFT: usize = 4_096/u);
  assert.match(modelLoader, /SPLEETER_HOP_LENGTH: usize = 1_024/u);
  assert.match(modelLoader, /SPLEETER_BINS: usize = 1_024/u);
  assert.match(modelLoader, /SPLEETER_FRAMES: usize = 512/u);
  assert.match(spleeter, /SPLEETER_EPSILON: f32 = 1\.0e-10/u);
  assert.match(spleeter, /\[2_usize, 1_usize, SPLEETER_FRAMES, SPLEETER_BINS\]/u);
  assert.match(spleeter, /SPLEETER_STEM_NAMES: \[&str; 2\] = \["acapella", "instrumental"\]/u);
  assert.match(spleeter, /descriptor\.kind == "spleeter-2stems"/u);
  assert.match(separatorSource, /Spleeter\(SpleeterSeparator\)/u);
  assert.match(separatorSource, /championvibe-spleeter-onnx-v1|SPLEETER_DESCRIPTOR_FORMAT/u);
  assert.match(importSource, /let stem_names: &\[&str\] = if stem_paths\.len\(\) == 2/u);
  assert.match(importSource, /installed_separator_descriptor/u);
});

test('the Patois drop sampleboard ships as local CC0 WAV assets', () => {
  assert.match(samples, /BUNDLED_DROPS/u);
  assert.match(samples, /CC0-1\.0 · ChampionVibe original synthesis/u);
  assert.match(samples, /include_bytes!/u);
  assert.match(
    readFileSync(new URL('../scripts/generate-cc0-reggae-drops.py', import.meta.url), 'utf8'),
    /--check/u,
  );
});

test('custom MDX imports validate ONNX contracts before copying named two- or four-stem sets', () => {
  assert.match(main, /invoke\('validate_local_model', \{ path, kind: 'separator' \}\)/u);
  assert.match(main, /invoke\('import_custom_separator_models', \{ paths, mode \}\)/u);
  assert.match(commands, /pub fn validate_local_model/u);
  assert.match(commands, /pub fn import_custom_separator_models/u);
  assert.match(commandRegistry, /commands::validate_local_model/u);
  assert.match(commandRegistry, /commands::import_custom_separator_models/u);
  assert.match(manager, /"two-stem" => 1,[\s\S]*?"four-stem" => 4/u);
  assert.match(manager, /inspect_mdx_signature\(&source\)/u);
  assert.match(manager, /\["vocals", "other", "bass", "drums"\]/u);
  assert.match(manager, /sha256: Some\(source_hash\)/u);
  assert.match(mdx, /MDX-Net requires a ChampionVibe \.cvmodel\.json model manifest/u);
  assert.match(manager, /target-specific n_fft/u);
});

test('Kokoro uses opt-in local WASM inference and routes PCM through the native master bus', () => {
  assert.match(main, /createKokoroWasmService\(\{ invoke \}\)/u);
  assert.match(main, /state\.kokoroWasmService\.synthesize\(/u);
  assert.match(main, /pushWasmTtsAudioBuffer\(audio/u);
  assert.match(main, /android: isAndroid/u);
  assert.match(main, /#load-kokoro-wasm/u);
  assert.match(wasmTts, /read_managed_kokoro_asset/u);
  assert.match(wasmTts, /executionProviders:\s*\['wasm'\]/u);
  assert.match(wasmTts, /SAMPLE_RATE\s*=\s*24_000/u);
  assert.match(wasmTts, /headers:\s*\{/u);
  assert.match(wasmTts, /pcmBase64:/u);
  assert.match(wasmTts, /applyPatoisProsody/u);
  assert.match(main, /if \(\/aggressive\/u\.test\(label\)\) return 'aggressive';/u);
  assert.doesNotMatch(wasmTts, /https?:\/\//u);
  assert.match(commands, /pub fn push_tts_audio_buffer/u);
  assert.match(commands, /raw_tts_ipc_body/u);
  assert.match(commands, /parse_tts_android_base64_payload/u);
  assert.match(commands, /JSON number arrays are not accepted for TTS PCM/u);
  assert.match(commandRegistry, /commands::push_tts_audio_buffer/u);
  assert.match(main, /keine authentische Patois-Stimme/u);
  assert.match(indexHtml, /nativen Master-Ausgang zum nächsten Beat oder Takt/u);
  assert.match(indexHtml, /nicht an den Track-Key angepasst/u);
});
