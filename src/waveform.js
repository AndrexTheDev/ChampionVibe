export function decodeWaveformPacket(value) {
  let buffer;
  if (value instanceof ArrayBuffer) buffer = value;
  else if (ArrayBuffer.isView(value)) {
    buffer = value.buffer.slice(value.byteOffset, value.byteOffset + value.byteLength);
  } else {
    throw new Error('Binary IPC response was not an ArrayBuffer');
  }

  if (buffer.byteLength < 16) throw new Error('Waveform packet header is incomplete');
  const view = new DataView(buffer);
  const magic = String.fromCharCode(view.getUint8(0), view.getUint8(1), view.getUint8(2), view.getUint8(3));
  if (magic !== 'CVW1') throw new Error('Unsupported waveform packet version');
  const peakCount = view.getUint32(4, true);
  const beatCount = view.getUint32(8, true);
  const bpm = view.getFloat32(12, true);
  const expectedBytes = 16 + peakCount * 2 + beatCount * 4;
  if (peakCount > 2048 || beatCount > 200_000 || expectedBytes !== buffer.byteLength) {
    throw new Error('Waveform packet length is invalid');
  }

  const peaks = new Uint16Array(peakCount);
  const beats = new Uint32Array(beatCount);
  let offset = 16;
  for (let index = 0; index < peakCount; index += 1, offset += 2) {
    peaks[index] = view.getUint16(offset, true);
  }
  for (let index = 0; index < beatCount; index += 1, offset += 4) {
    beats[index] = view.getUint32(offset, true);
  }
  return { peaks, beats, bpm };
}
