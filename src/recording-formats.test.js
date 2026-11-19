import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

const html = readFileSync(new URL('../index.html', import.meta.url), 'utf8');
const rust = readFileSync(new URL('../src-tauri/src/audio/recording.rs', import.meta.url), 'utf8');
const formatIpc = readFileSync(new URL('../src-tauri/src/audio/recording_formats.rs', import.meta.url), 'utf8');
const commands = readFileSync(new URL('../src-tauri/src/commands.rs', import.meta.url), 'utf8');
const commandRegistry = readFileSync(new URL('../src-tauri/src/lib.rs', import.meta.url), 'utf8');
const main = readFileSync(new URL('../src/main.js', import.meta.url), 'utf8');
const options = [...html.matchAll(/<option\b([^>]*)>([\s\S]*?)<\/option>/gu)]
  .map(([, attributes, label]) => ({ attributes, label }))
  .filter(({ attributes }) => /\bvalue="(?:wavPcm16|wavPcm24|flac|mp3|oggVorbis|m4aAac)"/u.test(attributes));

function valueOf(attributes) {
  return attributes.match(/\bvalue="([^"]+)"/u)?.[1];
}

test('enabled recording-format choices map to formats accepted by the Rust writer', () => {
  const enabled = options.filter(({ attributes }) => !/\bdisabled\b/u.test(attributes));
  assert.deepEqual(enabled.map(({ attributes }) => valueOf(attributes)), [
    'wavPcm16', 'wavPcm24', 'flac', 'mp3', 'oggVorbis', 'm4aAac',
  ]);
  for (const { attributes } of enabled) {
    const value = valueOf(attributes);
    assert.match(rust, new RegExp(`"${value}"\\s*(?:\\||=>)`, 'u'), `${value} must be accepted by RecordingFormat::parse`);
  }
});

test('the recording selector is synchronized from registered Rust encoder availability IPC', () => {
  assert.match(commands, /pub fn get_recording_formats\(\) -> Vec<audio::RecordingFormatInfo>/u);
  assert.match(commandRegistry, /commands::get_recording_formats/u);
  assert.match(main, /invoke\('get_recording_formats'\)/u);
  assert.match(main, /option\.disabled = !availability\.get\(option\.value\)/u);
  for (const id of ['wavPcm16', 'wavPcm24', 'flac', 'mp3', 'oggVorbis', 'm4aAac']) {
    assert.ok(formatIpc.includes(`id: "${id}"`), `${id} must be reported by the Rust format inventory`);
  }
});

test('MP3, OGG/Vorbis, and M4A/AAC are selectable only with implemented encoder paths', () => {
  const unavailable = options.filter(({ attributes }) => /\bdisabled\b/u.test(attributes));
  assert.deepEqual(unavailable, []);
  for (const [id, encoder] of [
    ['mp3', /Mp3RecordingEncoder::new\([\s\S]*?map\(PcmStageWriter::Mp3\)/u],
    ['oggVorbis', /OggRecordingEncoder::new\([\s\S]*?map\(PcmStageWriter::Ogg\)/u],
    ['m4aAac', /AacMp4RecordingEncoder::new\([\s\S]*?map\(PcmStageWriter::M4a\)/u],
  ]) {
    assert.ok(formatIpc.includes(`id: "${id}"`), `${id} must be inventoried`);
    assert.match(formatIpc, new RegExp(`id: "${id}"[\\s\\S]*?available: true`, 'u'));
    assert.match(rust, encoder, `${id} must reach a concrete encoder implementation`);
  }
});
