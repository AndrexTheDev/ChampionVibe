import test from 'node:test';
import assert from 'node:assert/strict';
import { decodeWaveformPacket } from './waveform.js';

function makePacket({ peaks = [], beats = [], bpm = 0 } = {}) {
  const buffer = new ArrayBuffer(16 + peaks.length * 2 + beats.length * 4);
  const view = new DataView(buffer);
  for (const [index, byte] of [...'CVW1'].entries()) view.setUint8(index, byte.charCodeAt(0));
  view.setUint32(4, peaks.length, true);
  view.setUint32(8, beats.length, true);
  view.setFloat32(12, bpm, true);
  let offset = 16;
  for (const peak of peaks) {
    view.setUint16(offset, peak, true);
    offset += 2;
  }
  for (const beat of beats) {
    view.setUint32(offset, beat, true);
    offset += 4;
  }
  return buffer;
}

test('decodes the CVW1 header and little-endian peak and beat arrays', () => {
  const result = decodeWaveformPacket(makePacket({
    peaks: [0, 16_384, 65_535],
    beats: [0, 500, 1_000],
    bpm: 120.5,
  }));
  assert.deepEqual([...result.peaks], [0, 16_384, 65_535]);
  assert.deepEqual([...result.beats], [0, 500, 1_000]);
  assert.equal(result.bpm, 120.5);
});

test('accepts typed-array views without reading bytes outside the view', () => {
  const packet = new Uint8Array(makePacket({ peaks: [7], beats: [250], bpm: 80 }));
  const padded = new Uint8Array(packet.length + 4).fill(0xff);
  padded.set(packet, 2);
  const result = decodeWaveformPacket(padded.subarray(2, 2 + packet.length));
  assert.deepEqual([...result.peaks], [7]);
  assert.deepEqual([...result.beats], [250]);
  assert.equal(result.bpm, 80);
});

test('rejects JSON number arrays so IPC stays binary-only', () => {
  assert.throws(() => decodeWaveformPacket([67, 86, 87, 49]), /not an ArrayBuffer/);
  assert.throws(() => decodeWaveformPacket({ peaks: [1, 2, 3] }), /not an ArrayBuffer/);
});

test('rejects truncated, unknown-version, inconsistent, and over-limit packets', () => {
  assert.throws(() => decodeWaveformPacket(new Uint8Array(15)), /header is incomplete/);
  const wrongVersion = new Uint8Array(makePacket());
  wrongVersion[3] = '2'.charCodeAt(0);
  assert.throws(() => decodeWaveformPacket(wrongVersion), /Unsupported waveform packet version/);

  const inconsistent = new DataView(makePacket());
  inconsistent.setUint32(4, 1, true);
  assert.throws(() => decodeWaveformPacket(inconsistent.buffer), /packet length is invalid/);

  const tooManyPeaks = new DataView(makePacket());
  tooManyPeaks.setUint32(4, 2_049, true);
  assert.throws(() => decodeWaveformPacket(tooManyPeaks.buffer), /packet length is invalid/);
});
