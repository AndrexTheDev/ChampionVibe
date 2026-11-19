import test from 'node:test';
import assert from 'node:assert/strict';
import { getAudioFormat } from './audio-format.js';

test('prefers the locally detected codec over a misleading file extension', () => {
  assert.equal(getAudioFormat({ filePath: 'voice.mp3', codec: 'aac' }), 'AAC');
  assert.equal(getAudioFormat({ file_path: 'mix.wav', codec: 'mp3' }), 'MP3');
  assert.equal(getAudioFormat({ filePath: 'mix.ogg', codec: 'flac' }), 'FLAC');
  assert.equal(getAudioFormat({ filePath: 'mix.mp4', codec: 'opus' }), 'OGG');
  assert.equal(getAudioFormat({ filePath: 'track.wav', codec: 'mpeg layer 3' }), 'MP3');
  assert.equal(getAudioFormat({ filePath: 'misnamed.mp3', codec: 'mp2' }), 'AUDIO');
});

test('maps codec and container combinations to user-facing format badges', () => {
  assert.equal(getAudioFormat({ filePath: 'track.m4a', codec: 'aac' }), 'M4A');
  assert.equal(getAudioFormat({ filePath: 'track.aac', codec: 'aac' }), 'AAC');
  assert.equal(getAudioFormat({ filePath: 'track.aiff', codec: 'pcm_s16le' }), 'PCM');
  assert.equal(getAudioFormat({ filePath: 'track.wav', codec: 'pcm_s16le' }), 'WAV');
  assert.equal(getAudioFormat({ filePath: 'track.m4a', codec: 'alac' }), 'M4A');
});

test('uses recognized extensions only when the decoder did not report a codec', () => {
  assert.equal(getAudioFormat({ filePath: 'track.mp3' }), 'MP3');
  assert.equal(getAudioFormat({ filePath: 'track.flac' }), 'FLAC');
  assert.equal(getAudioFormat({ filePath: 'track.oga' }), 'OGG');
  assert.equal(getAudioFormat({ filePath: 'track.unknown', codec: 'mystery' }), 'AUDIO');
});
