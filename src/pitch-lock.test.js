import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

const html = readFileSync(new URL('../index.html', import.meta.url), 'utf8');
const main = readFileSync(new URL('./main.js', import.meta.url), 'utf8');
const audio = readFileSync(new URL('../src-tauri/src/audio/mod.rs', import.meta.url), 'utf8');
const decoder = readFileSync(new URL('../src-tauri/src/audio/decoder.rs', import.meta.url), 'utf8');
const masterBus = readFileSync(new URL('../src-tauri/src/audio/master_bus.rs', import.meta.url), 'utf8');
const wsola = readFileSync(new URL('../src-tauri/src/audio/wsola.rs', import.meta.url), 'utf8');

test('the deck Pitch-Lock toggle persists and reaches the active deck and both mix decks', () => {
  assert.match(html, /id="pitch-lock-enabled" type="checkbox" checked/u);
  assert.match(main, /pitchLockEnabled: state\.pitchLockEnabled/u);
  assert.match(main, /localStorage\.setItem\('championvibe\.pitchLockEnabled'/u);
  assert.match(audio, /pub pitch_lock_enabled: Option<bool>/u);
  assert.match(audio, /self\.pitch_lock_enabled\.store\(value, Ordering::Relaxed\)/u);
});

test('live Pitch-Lock toggles crossfade pre-rendered WSOLA and varispeed branches', () => {
  assert.match(decoder, /let mut stretcher = if tempo_changed/u);
  assert.match(decoder, /let mut varispeed = tempo_changed\.then\(\|\| VarispeedProcessor::new\(playback_rate\)\)/u);
  assert.match(decoder, /\(Some\(wsola\), Some\(varispeed\)\) => Ok\(Some\(PitchFrame \{ wsola, varispeed \}\)\)/u);
  assert.match(decoder, /fn disabled_pitch_lock_uses_bounded_streaming_varispeed/u);
  assert.match(masterBus, /self\.pitch_transitions\[0\]\.mix\([\s\S]*?self\.pitch_lock_enabled\[0\]\.load/u);
  assert.match(wsola, /step_per_frame: 1\.0 \/ \(sample_rate\.max\(1\) as f32 \* 0\.010\)/u);
  assert.match(wsola, /fn pitch_lock_mode_changes_use_a_ten_millisecond_ramp/u);
  assert.match(html, /WSOLA preserves pitch for tempo changes; Varispeed changes pitch with tempo/u);
});
