import test from 'node:test';
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { Buffer } from 'node:buffer';
import {
  applyPatoisProsody,
  buildPatoisPhrase,
  createPatoisEmotionVector,
  createPatoisProsodyVector,
  normalizePatoisEmotion,
  transformPatoisText,
} from './patois_g2p.js';
import {
  createKokoroWasmService,
  KOKORO_SAMPLE_RATE,
  pushWasmTtsAudioBuffer,
  tokenizeKokoroPhonemes,
} from './tts_wasm.js';

const require = createRequire(import.meta.url);
const { phonemize } = require('phonemize');

test('Kokoro phoneme tokenizer maps IPA into the pinned model vocabulary', () => {
  assert.deepEqual(tokenizeKokoroPhonemes('həˈloʊ'), [50, 83, 156, 54, 57, 135]);
  assert.throws(() => tokenizeKokoroPhonemes(''), /no Kokoro-compatible tokens/u);
});

test('Patois G2P applies the requested word rules with case and word-boundary preservation', () => {
  assert.equal(
    transformPatoisText('THING them Brother THREE girl Boy Make Little'),
    'TING dem Bredda TREE gyal Bwoy Mek Likkle',
  );
  assert.equal(transformPatoisText('something, them! brothers and boyish'), 'something, dem! brothers and boyish');
  assert.throws(() => transformPatoisText(null), /input must be a string/u);
});

test('local English G2P output maps the generated IPA into Kokoro token IDs', () => {
  const phrase = buildPatoisPhrase({ text: 'Big tune tonight', preset: 'selector', djName: 'Andrex', emotion: 'energetic' });
  const ipa = phonemize(phrase.text, { language: 'en-US', format: 'ipa', stripStress: false });
  const tokenIds = tokenizeKokoroPhonemes(ipa);
  assert.match(ipa, /ˈ/u);
  assert.equal(tokenIds.length, [...ipa.replace(/\s+/gu, ' ').trim()].length);
  assert.ok(tokenIds.length > 0);
  assert.ok(tokenIds.length <= 510);
  assert.ok(tokenIds.every(Number.isInteger));
});

test('Patois phrase templates return stable one-hot emotion vectors and model cues', () => {
  const hype = buildPatoisPhrase({
    text: 'Big thing tonight',
    preset: 'selector',
    djName: 'Andrex',
    emotion: 'hype',
  });
  assert.match(hype.text, /Yo! Forward, selecta! Big up Andrex!/u);
  assert.match(hype.text, /Big ting tonight/u);
  assert.equal(hype.emotion, 'energetic');
  assert.deepEqual(Array.from(hype.emotionVector), [1, 0, 0, 0, 0, 0]);
  assert.deepEqual(Array.from(createPatoisEmotionVector('calm')), [0, 1, 0, 0, 0, 0]);
  assert.equal(normalizePatoisEmotion('hype'), 'energetic');
  assert.equal(normalizePatoisEmotion('aggressive'), 'aggressive');
  assert.throws(() => createPatoisEmotionVector('angry'), /Emotion must be/u);
});

test('hype and aggressive prosody vectors vary rate, pitch, and the local pitch contour', () => {
  const neutral = createPatoisProsodyVector('confident');
  const hype = createPatoisProsodyVector('hype');
  const aggressive = createPatoisProsodyVector('aggressive');
  assert.equal(neutral[0], 1);
  assert.equal(neutral[1], 0);
  assert.ok(hype[0] > neutral[0] && hype[1] > neutral[1] && hype[2] > 0);
  assert.ok(aggressive[0] > hype[0] && aggressive[1] > hype[1] && aggressive[2] > hype[2]);

  const samples = Float32Array.from({ length: 960 }, (_, index) => Math.sin((2 * Math.PI * 220 * index) / KOKORO_SAMPLE_RATE));
  assert.equal(applyPatoisProsody(samples, neutral), samples);
  const pitched = applyPatoisProsody(samples, aggressive);
  const pitchScale = 2 ** (aggressive[1] / 12);
  assert.equal(pitched.length, Math.round(samples.length / pitchScale));
  assert.notEqual(pitched, samples);
  assert.ok(pitched.every(Number.isFinite));
  assert.notDeepEqual(pitched, samples);
});

test('WASM engine synthesizes finite Float32 PCM at the Kokoro sample rate via binary local IPC', async () => {
  const voiceBytes = new Float32Array(510 * 256);
  voiceBytes.fill(0.125);
  const calls = [];
  let runtimeOptions;
  let capturedFeeds;
  let phonemizedText;
  const runtime = {
    env: { wasm: {} },
    Tensor: class {
      constructor(type, data, dims) {
        this.type = type;
        this.data = data;
        this.dims = dims;
      }
    },
    InferenceSession: {
      async create(model, options) {
        runtimeOptions = options;
        assert.ok(model instanceof ArrayBuffer);
        return {
          outputNames: ['audio'],
          async run(feeds) {
            capturedFeeds = feeds;
            return { audio: { data: new Float32Array([0.1, -0.2, 0.3, 0]) } };
          },
          release() {},
        };
      },
    },
  };
  const service = createKokoroWasmService({
    runtime,
    g2p: (text) => {
      phonemizedText = text;
      return 'həˈloʊ';
    },
    invoke: async (command, args) => {
      calls.push([command, args]);
      return args.asset === 'model' ? new Uint8Array([1, 2, 3]).buffer : voiceBytes.buffer;
    },
  });

  await service.load();
  const result = await service.synthesize({ text: 'The thing, brother', emotion: 'confident' });
  assert.equal(service.ready, true);
  assert.deepEqual(runtimeOptions.executionProviders, ['wasm']);
  assert.equal(runtime.env.wasm.numThreads, 1);
  assert.deepEqual(calls, [
    ['read_managed_kokoro_asset', { asset: 'model' }],
    ['read_managed_kokoro_asset', { asset: 'voice' }],
  ]);
  assert.equal(capturedFeeds.input_ids.type, 'int64');
  assert.deepEqual(capturedFeeds.input_ids.dims, [1, 8]);
  assert.deepEqual(capturedFeeds.style.dims, [1, 256]);
  assert.equal(phonemizedText, 'The ting, bredda');
  assert.equal(capturedFeeds.speed.data[0], 1);
  assert.equal(result.sampleRate, KOKORO_SAMPLE_RATE);
  assert.equal(result.samples instanceof Float32Array, true);
  assert.deepEqual(Array.from(result.samples).map((sample) => Math.round(sample * 10) / 10), [0.1, -0.2, 0.3, 0]);
  assert.deepEqual(Array.from(result.emotionVector), [0, 0, 0, 0, 1, 0]);
  assert.deepEqual(Array.from(result.prosodyVector), [1, 0, 0]);
  assert.equal(result.prosody.pitchSemitones, 0);
  assert.equal(result.authenticPatoisVoice, false);
});

test('aggressive prosody reaches the Kokoro speed input and transforms the synthesized pitch contour', async () => {
  const voiceBytes = new Float32Array(256);
  let capturedSpeed;
  let phonemizedText;
  const runtime = {
    env: { wasm: {} },
    Tensor: class {
      constructor(type, data) { this.type = type; this.data = data; }
    },
    InferenceSession: {
      async create() {
        return {
          outputNames: ['audio'],
          async run(feeds) {
            capturedSpeed = feeds.speed.data[0];
            return { audio: { data: Float32Array.from({ length: 960 }, (_, index) => Math.sin(index / 5)) } };
          },
          release() {},
        };
      },
    },
  };
  const service = createKokoroWasmService({
    runtime,
    g2p: (text) => {
      phonemizedText = text;
      return 'ˈtɪŋ';
    },
    invoke: async (_command, args) => args.asset === 'model'
      ? new Uint8Array([1]).buffer
      : voiceBytes.buffer,
  });

  await service.load();
  const result = await service.synthesize({ text: 'Girl and boy', emotion: 'aggressive' });
  const expectedModelSpeed = result.prosody.targetRate / result.prosody.pitchScale;
  assert.equal(phonemizedText, 'Stand firm! Gyal and bwoy!');
  assert.ok(Math.abs(capturedSpeed - expectedModelSpeed) < 1e-6);
  assert.ok(result.prosody.pitchSemitones > 0);
  assert.ok(result.samples.length < 960);
  assert.ok(result.samples.every(Number.isFinite));
});

test('Kokoro handoff sends little-endian PCM through raw Tauri IPC with beat-grid metadata', async () => {
  const calls = [];
  const audio = { samples: new Float32Array([0.25, -0.5]), sampleRate: KOKORO_SAMPLE_RATE };
  const result = await pushWasmTtsAudioBuffer(audio, {
    invoke: async (...args) => {
      calls.push(args);
      return { targetDeckFrame: 48_000, delayFrames: 24_000, outputSampleRate: 48_000, quantization: 'bar' };
    },
    quantization: 'bar',
  });

  assert.equal(calls.length, 1);
  const [command, body, options] = calls[0];
  assert.equal(command, 'push_tts_audio_buffer');
  assert.ok(body instanceof Uint8Array);
  assert.equal(Array.isArray(body), false);
  assert.equal(body.byteLength, audio.samples.length * 4);
  const view = new DataView(body.buffer, body.byteOffset, body.byteLength);
  assert.equal(view.getFloat32(0, true), 0.25);
  assert.equal(view.getFloat32(4, true), -0.5);
  assert.deepEqual(options.headers, {
    'x-cv-tts-sample-rate': '24000',
    'x-cv-tts-channels': '1',
    'x-cv-tts-encoding': 'f32le',
    'x-cv-tts-quantization': 'bar',
  });
  assert.equal(result.transport, 'raw');
  assert.equal(result.schedule.quantization, 'bar');
  assert.ok(Math.abs(result.durationSeconds - 2 / KOKORO_SAMPLE_RATE) < 1e-9);
});

test('Android TTS fallback carries base64 PCM, not a JSON numeric array', async () => {
  const calls = [];
  const audio = { samples: new Float32Array([0.125, -0.75, 0]), sampleRate: KOKORO_SAMPLE_RATE };
  const result = await pushWasmTtsAudioBuffer(audio, {
    invoke: async (...args) => {
      calls.push(args);
      return { targetDeckFrame: 12, delayFrames: 12, outputSampleRate: 48_000, quantization: 'beat' };
    },
    android: true,
    quantization: 'beat',
  });

  const [command, payload] = calls[0];
  assert.equal(command, 'push_tts_audio_buffer');
  assert.equal(Array.isArray(payload.pcmBase64), false);
  assert.deepEqual(Object.keys(payload).sort(), ['channels', 'encoding', 'pcmBase64', 'quantization', 'sampleRate']);
  const decoded = Buffer.from(payload.pcmBase64, 'base64');
  assert.equal(decoded.length, audio.samples.length * 4);
  const view = new DataView(decoded.buffer, decoded.byteOffset, decoded.byteLength);
  assert.equal(view.getFloat32(0, true), 0.125);
  assert.equal(view.getFloat32(4, true), -0.75);
  assert.equal(payload.encoding, 'f32le');
  assert.equal(payload.quantization, 'beat');
  assert.equal(result.transport, 'base64');
});

test('TTS handoff rejects invalid buffers and quantization values', async () => {
  await assert.rejects(
    pushWasmTtsAudioBuffer({ samples: new Float32Array([Number.NaN]), sampleRate: KOKORO_SAMPLE_RATE }, { invoke() {} }),
    /non-finite/u,
  );
  await assert.rejects(
    pushWasmTtsAudioBuffer({ samples: new Float32Array([0]), sampleRate: KOKORO_SAMPLE_RATE }, {
      invoke() {}, quantization: 'off',
    }),
    /quantization must be beat or bar/u,
  );
});
