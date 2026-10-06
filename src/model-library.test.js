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
});

test('managed artifacts use unique hard-coded SHA-256 pins and verify before install', () => {
  const pins = [...manager.matchAll(/sha256:\s*"([a-f0-9]{64})"/gu)].map(([, pin]) => pin);
  assert.equal(pins.length, 7, 'four MDX, Whisper, Kokoro ONNX, and voice artifacts each need a pin');
  assert.equal(new Set(pins).size, pins.length, 'artifact pins must not be accidentally reused');
  for (const id of [
    'separator-android', 'separator-four-stem', 'whisper-tiny', 'kokoro-model', 'kokoro-voice-af-heart',
  ]) {
    assert.ok(manager.includes(`"${id}"`), `${id} must be present in the built-in catalog`);
  }
  assert.match(manager, /actual_hash\.eq_ignore_ascii_case\(artifact\.sha256\)/u);
  assert.match(manager, /validate_artifact\(artifact, &temporary\)[\s\S]*?fs::rename\(&temporary, &final_path\)/u);
  assert.match(manager, /validate_manifest_models\(&manifest, &self\.root, true\)/u);
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
