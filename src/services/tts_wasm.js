import { applyPatoisProsody, buildPatoisPhrase } from './patois_g2p.js';

export {
  applyPatoisProsody,
  buildPatoisPhrase,
  createPatoisEmotionVector,
  createPatoisProsodyVector,
  normalizePatoisEmotion,
  transformPatoisText,
} from './patois_g2p.js';

let defaultPhonemizerPromise = null;
async function defaultPhonemizer() {
  defaultPhonemizerPromise ||= import('phonemize').then((module) => module.phonemize);
  return defaultPhonemizerPromise;
}

export const KOKORO_SAMPLE_RATE = 24_000;
export const KOKORO_MAX_PHONEMES = 510;
export const KOKORO_MAX_AUDIO_SECONDS = 120;
const MODEL_IPC_LIMIT_BYTES = 512 * 1024 * 1024;
const TTS_PCM_LIMIT_BYTES = 64 * 1024 * 1024;
const VOICE_VECTOR_WIDTH = 256;
const MODEL_REVISION = '468588286ebb2dd77c25b9771e5d165896538cce';

// Character vocabulary from the pinned Kokoro tokenizer.json. This keeps tokenization local and
// avoids Kokoro-JS's eSpeak-based phonemizer dependency; the MIT `phonemize` G2P runs locally.
// Repository: onnx-community/Kokoro-82M-v1.0-ONNX, revision above, tokenizer.json.
const PREFIX_TOKENS = [
  '$', ';', ':', ',', '.', '!', '?', '¡', '¿', '—', '…', '"', '«', '»', '“', '”', ' ',
  ...'ABCDEFGHIJKLMNOPQRSTUVWXYZ',
  ...'abcdefghijklmnopqrstuvwxyz',
];
const IPA_CODEPOINTS = [
  0x0251, 0x0250, 0x0252, 0x00e6, 0x0253, 0x0299, 0x03b2, 0x0254, 0x0255, 0x00e7,
  0x0257, 0x0256, 0x00f0, 0x02a4, 0x0259, 0x0258, 0x025a, 0x025b, 0x025c, 0x025d,
  0x025e, 0x025f, 0x0284, 0x0261, 0x0260, 0x0262, 0x029b, 0x0266, 0x0267, 0x0127,
  0x0265, 0x029c, 0x0268, 0x026a, 0x029d, 0x026d, 0x026c, 0x026b, 0x026e, 0x029f,
  0x0271, 0x026f, 0x0270, 0x014b, 0x0273, 0x0272, 0x0274, 0x00f8, 0x0275, 0x0278,
  0x03b8, 0x0153, 0x0276, 0x0298, 0x0279, 0x027a, 0x027e, 0x027b, 0x0280, 0x0281,
  0x027d, 0x0282, 0x0283, 0x0288, 0x02a7, 0x0289, 0x028a, 0x028b, 0x2c71, 0x028c,
  0x0263, 0x0264, 0x028d, 0x03c7, 0x028e, 0x028f, 0x0291, 0x0290, 0x0292, 0x0294,
  0x02a1, 0x0295, 0x02a2, 0x01c0, 0x01c1, 0x01c2, 0x01c3, 0x02c8, 0x02cc, 0x02d0,
  0x02d1, 0x02bc, 0x02b4, 0x02b0, 0x02b1, 0x02b2, 0x02b7, 0x02e0, 0x02e4, 0x02de,
  0x2193, 0x2191, 0x2192, 0x2197, 0x2198,
];
const KOKORO_VOCAB = new Map();
PREFIX_TOKENS.forEach((token, index) => KOKORO_VOCAB.set(token, index));
IPA_CODEPOINTS.forEach((codepoint, index) => KOKORO_VOCAB.set(String.fromCodePoint(codepoint), 69 + index));
KOKORO_VOCAB.set(String.fromCodePoint(0x0329), 175);
KOKORO_VOCAB.set("'", 176);
KOKORO_VOCAB.set(String.fromCodePoint(0x1d7b), 177);

export function tokenizeKokoroPhonemes(phonemes) {
  const normalized = String(phonemes ?? '').replace(/\s+/gu, ' ').trim();
  const tokenIds = [];
  for (const character of normalized) {
    // Matches tokenizer.json's normalization rule: unsupported symbols are removed.
    const tokenId = KOKORO_VOCAB.get(character);
    if (tokenId !== undefined) tokenIds.push(tokenId);
  }
  if (!tokenIds.length) throw new Error('The local phonemizer returned no Kokoro-compatible tokens.');
  if (tokenIds.length > KOKORO_MAX_PHONEMES) {
    throw new Error(`Shoutout exceeds Kokoro’s ${KOKORO_MAX_PHONEMES}-phoneme limit.`);
  }
  return tokenIds;
}

function binaryView(value, label) {
  if (value instanceof ArrayBuffer) return new Uint8Array(value);
  if (ArrayBuffer.isView(value) && value.BYTES_PER_ELEMENT === 1) {
    return new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
  }
  throw new Error(`${label} must arrive as a binary ArrayBuffer from Tauri IPC.`);
}

function ownedArrayBuffer(bytes) {
  return bytes.byteOffset === 0 && bytes.byteLength === bytes.buffer.byteLength
    ? bytes.buffer
    : bytes.slice().buffer;
}

function readVoiceMatrix(value) {
  const bytes = binaryView(value, 'Kokoro voice pack');
  if (bytes.byteLength === 0 || bytes.byteLength % (VOICE_VECTOR_WIDTH * Float32Array.BYTES_PER_ELEMENT) !== 0) {
    throw new Error('Kokoro voice pack does not contain complete 256-float style vectors.');
  }
  const aligned = bytes.byteOffset % Float32Array.BYTES_PER_ELEMENT === 0
    ? bytes
    : bytes.slice();
  const floats = new Float32Array(aligned.buffer, aligned.byteOffset, aligned.byteLength / 4);
  if (!floats.length || !floats.every(Number.isFinite)) throw new Error('Kokoro voice pack contains invalid values.');
  return floats;
}

function sampleRateFromOutput(output) {
  const samples = output?.data;
  if (!samples || typeof samples.length !== 'number') throw new Error('Kokoro ONNX returned no audio tensor.');
  if (samples.length === 0 || samples.length > KOKORO_SAMPLE_RATE * KOKORO_MAX_AUDIO_SECONDS) {
    throw new Error('Kokoro ONNX audio output is empty or exceeds the local duration limit.');
  }
  const pcm = Float32Array.from(samples);
  if (!pcm.every(Number.isFinite)) throw new Error('Kokoro ONNX returned non-finite PCM samples.');
  return pcm;
}

export function createKokoroWasmService({ runtime: injectedRuntime = null, g2p = null, invoke } = {}) {
  if (typeof invoke !== 'function') throw new TypeError('A Tauri invoke function is required to read verified local model files.');
  let runtime = injectedRuntime;
  let session = null;
  let voice = null;
  let loadPromise = null;

  async function load() {
    if (session && voice) return { ready: true, runtime: 'WebAssembly', revision: MODEL_REVISION };
    if (loadPromise) return loadPromise;
    loadPromise = (async () => {
      const runtimePromise = runtime ? Promise.resolve(runtime) : import('onnxruntime-web/wasm');
      const [modelPayload, voicePayload, loadedRuntime] = await Promise.all([
        invoke('read_managed_kokoro_asset', { asset: 'model' }),
        invoke('read_managed_kokoro_asset', { asset: 'voice' }),
        runtimePromise,
      ]);
      runtime ||= loadedRuntime;
      const modelBytes = binaryView(modelPayload, 'Kokoro ONNX model');
      if (!modelBytes.byteLength || modelBytes.byteLength > MODEL_IPC_LIMIT_BYTES) {
        throw new Error('Kokoro ONNX model exceeds the WebView loading limit.');
      }
      voice = readVoiceMatrix(voicePayload);
      runtime.env.wasm.numThreads = 1;
      runtime.env.wasm.proxy = false;
      session = await runtime.InferenceSession.create(ownedArrayBuffer(modelBytes), {
        executionProviders: ['wasm'],
        graphOptimizationLevel: 'all',
      });
      return { ready: true, runtime: 'WebAssembly', revision: MODEL_REVISION };
    })();
    try {
      return await loadPromise;
    } catch (error) {
      session?.release?.();
      session = null;
      voice = null;
      throw error;
    } finally {
      loadPromise = null;
    }
  }

  async function synthesize(options) {
    if (!session || !voice) throw new Error('Load the verified Kokoro WASM model and voice first.');
    const { text: phrase, emotion, emotionVector, prosodyVector } = buildPatoisPhrase(options);
    const phonemizer = g2p || await defaultPhonemizer();
    const phonemes = phonemizer(phrase, { language: 'en-US', format: 'ipa', stripStress: false });
    const phoneIds = tokenizeKokoroPhonemes(phonemes);
    const tokenIds = BigInt64Array.from([0, ...phoneIds, 0], BigInt);
    const styleIndex = Math.min(phoneIds.length, Math.floor(voice.length / VOICE_VECTOR_WIDTH) - 1);
    const style = voice.slice(styleIndex * VOICE_VECTOR_WIDTH, (styleIndex + 1) * VOICE_VECTOR_WIDTH);
    const targetRate = prosodyVector[0];
    const pitchSemitones = prosodyVector[1];
    const pitchScale = 2 ** (pitchSemitones / 12);
    // Kokoro's speed input controls duration. Compensate for the later local pitch varispeed so
    // the requested rate remains approximately independent of the global pitch shift.
    const modelSpeed = targetRate / pitchScale;
    const feeds = {
      input_ids: new runtime.Tensor('int64', tokenIds, [1, tokenIds.length]),
      style: new runtime.Tensor('float32', style, [1, VOICE_VECTOR_WIDTH]),
      speed: new runtime.Tensor('float32', new Float32Array([modelSpeed]), [1]),
    };
    const outputs = await session.run(feeds);
    const outputName = session.outputNames?.[0] || Object.keys(outputs)[0];
    const nativeSamples = sampleRateFromOutput(outputs[outputName]);
    const samples = applyPatoisProsody(nativeSamples, prosodyVector);
    return {
      samples,
      sampleRate: KOKORO_SAMPLE_RATE,
      text: phrase,
      phonemes,
      emotion,
      emotionVector,
      prosodyVector,
      prosody: { targetRate, modelSpeed, pitchSemitones, pitchScale },
      voice: 'af_heart',
      language: 'en-US',
      authenticPatoisVoice: false,
    };
  }

  function dispose() {
    session?.release?.();
    session = null;
    voice = null;
  }

  return { load, synthesize, dispose, get ready() { return Boolean(session && voice); } };
}

function encodeBase64InChunks(bytes) {
  if (typeof globalThis.btoa !== 'function') throw new Error('This WebView cannot encode the Android TTS PCM fallback.');
  const chunks = [];
  const chunkSize = 0x6000; // divisible by three, so independently encoded chunks concatenate safely
  for (let offset = 0; offset < bytes.length; offset += chunkSize) {
    const chunk = bytes.subarray(offset, Math.min(offset + chunkSize, bytes.length));
    chunks.push(globalThis.btoa(String.fromCharCode(...chunk)));
  }
  return chunks.join('');
}

/**
 * Hands local Kokoro float32 PCM to Rust's master TTS Aux over Tauri IPC.
 * Desktop sends a raw ArrayBuffer; Tauri Android currently lacks raw request bodies, so Android
 * uses its documented compact base64-string fallback (never a JSON number array).
 */
export async function pushWasmTtsAudioBuffer(audio, {
  invoke,
  quantization = 'bar',
  android = false,
} = {}) {
  if (typeof invoke !== 'function') throw new TypeError('A Tauri invoke function is required to route TTS PCM.');
  const samples = audio?.samples;
  const sampleRate = Number(audio?.sampleRate);
  if (!(samples instanceof Float32Array) || !samples.length || !Number.isInteger(sampleRate)
    || sampleRate < 8_000 || sampleRate > 192_000
    || samples.length > sampleRate * KOKORO_MAX_AUDIO_SECONDS
    || samples.length * Float32Array.BYTES_PER_ELEMENT > TTS_PCM_LIMIT_BYTES) {
    throw new Error('Kokoro returned an invalid or oversized PCM buffer.');
  }
  if (!['beat', 'bar'].includes(quantization)) throw new Error('TTS quantization must be beat or bar.');
  if (!samples.every(Number.isFinite)) throw new Error('Kokoro PCM contains non-finite samples.');

  const pcm = new Uint8Array(samples.length * Float32Array.BYTES_PER_ELEMENT);
  const view = new DataView(pcm.buffer);
  samples.forEach((sample, index) => view.setFloat32(index * 4, sample, true));
  const schedule = android
    ? await invoke('push_tts_audio_buffer', {
      pcmBase64: encodeBase64InChunks(pcm),
      sampleRate,
      channels: 1,
      encoding: 'f32le',
      quantization,
    })
    : await invoke('push_tts_audio_buffer', pcm, {
      headers: {
        'x-cv-tts-sample-rate': String(sampleRate),
        'x-cv-tts-channels': '1',
        'x-cv-tts-encoding': 'f32le',
        'x-cv-tts-quantization': quantization,
      },
    });

  return { durationSeconds: samples.length / sampleRate, sampleRate, schedule, transport: android ? 'base64' : 'raw' };
}
