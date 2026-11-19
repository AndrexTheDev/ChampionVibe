import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

const html = readFileSync(new URL('../index.html', import.meta.url), 'utf8');
const main = readFileSync(new URL('./main.js', import.meta.url), 'utf8');
const audio = readFileSync(new URL('../src-tauri/src/audio/mod.rs', import.meta.url), 'utf8');
const ramp = readFileSync(new URL('../src-tauri/src/audio/auto_gain.rs', import.meta.url), 'utf8');
const commands = readFileSync(new URL('../src-tauri/src/commands.rs', import.meta.url), 'utf8');
const mixEngine = readFileSync(new URL('../src-tauri/src/mix_engine.rs', import.meta.url), 'utf8');
const registry = readFileSync(new URL('../src-tauri/src/lib.rs', import.meta.url), 'utf8');

test('Auto-Gain exposes enable, -14/-16 LUFS targets, and Off', () => {
  assert.match(html, /id="auto-gain-enabled" type="checkbox"/u);
  const selector = html.match(/<select id="auto-gain-target">([\s\S]*?)<\/select>/u)?.[1] || '';
  assert.deepEqual([...selector.matchAll(/<option value="([^"]+)"/gu)].map(([, value]) => value), ['off', '-14', '-16']);
  assert.match(main, /autoGainEnabled: state\.autoGainEnabled, autoGainTargetLufs: targetLufs/u);
  assert.match(main, /localStorage\.setItem\('championvibe\.autoGainEnabled'/u);
});

test('Auto-Gain IPC updates the active engine and both native mix decks', () => {
  assert.match(main, /invoke\('set_audio_controls', \{ update \}\)/u);
  assert.match(main, /\[0, 1\]\.map\(\(deck\) => invoke\('set_deck_audio_controls'/u);
  assert.match(commands, /pub fn set_audio_controls/u);
  assert.match(commands, /pub fn set_deck_audio_controls/u);
  assert.match(mixEngine, /pub fn set_deck_controls/u);
  assert.match(registry, /commands::set_deck_audio_controls/u);
  assert.match(audio, /auto_gain_enabled: Option<bool>/u);
  assert.match(audio, /auto_gain_target_lufs: Option<f32>/u);
  assert.match(audio, /check\("autoGainTargetLufs", value, -36\.0, -6\.0\)/u);
  assert.match(ramp, /const DEFAULT_RAMP_MS: f32 = 8\.0/u);
  assert.match(ramp, /smoothstep/iu);
});

test('deck UI reports the per-track applied R128 gain offset only when LUFS is available', () => {
  assert.match(html, /id="auto-gain-applied" aria-live="polite"/u);
  assert.match(main, /state\.autoGainTargetLufs - integratedLufs/u);
  assert.match(main, /Auto-Gain \{gain\} dB/u);
  assert.match(main, /integratedLufs < -70 \|\| integratedLufs > 10/u);
  assert.match(main, /No LUFS analysis is available; unity gain is used\./u);
});
