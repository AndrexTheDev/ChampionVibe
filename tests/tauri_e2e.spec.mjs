/**
 * Headless native-IPC end-to-end driver for ChampionVibe.
 *
 * The Tauri WebView talks to Rust through one bridge: `window.__TAURI_INTERNALS__`. This driver
 * replaces that bridge with a recording stub, so the four critical flows — import, deck loading,
 * manual beat-grid correction and the recording loop — can be exercised without a GUI, and the
 * native IPC signals (command names, argument spelling, event names and payload shapes) are
 * asserted against the contracts in `src-tauri/src/commands.rs`.
 *
 * Three run modes, all from this one file:
 *
 * 1. **Driver mode** (always available, no dependencies): `node tests/tauri_e2e.spec.mjs`
 *    runs every flow against the stub and exits non-zero on the first contract violation.
 * 2. **Playwright mode** (optional): when `@playwright/test` is installed, the same flows are
 *    registered as browser tests. A build in `dist/` is loaded headlessly with the stub injected,
 *    so the real frontend code is driven through the same bridge. Without `dist/` the browser
 *    tests skip themselves instead of failing a checkout that was never built.
 * 3. **CI**: `npm run e2e:tauri` runs mode 1.
 *
 * `node --test` deliberately ignores this directory, so the spec never breaks `npm test` on a
 * machine without Playwright.
 *
 * The stub factory is self-contained (no module-scope references) because mode 2 injects it into the
 * page with `createTauriIpcStub.toString()`.
 */

import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
export const repositoryRoot = resolve(here, '..');

/** Commands the driver expects the frontend to send, in the order the flows use them. */
export const EXPECTED_COMMANDS = Object.freeze({
  import: ['start_import_job', 'get_import_jobs'],
  deck: [
    'upsert_track',
    'load_track_audio',
    'set_audio_controls',
    'set_deck_audio_controls',
    'play_audio',
    'get_audio_status',
  ],
  grid: ['correct_track_grid', 'search_tracks_page'],
  recording: [
    'get_license_status',
    'authorize_recording',
    'start_master_recording',
    'get_master_recording_status',
    'stop_master_recording',
    'save_recording_metadata',
  ],
});

/**
 * Builds the stub for `window.__TAURI_INTERNALS__`.
 *
 * Everything the stub returns mirrors the serde shape of the matching Rust type, so a mismatch in
 * field naming (for example `statusMessage` vs `status_message`) fails the driver.
 */
export function createTauriIpcStub() {
  // The Playwright test stringifies this factory into the page, so it must not close over a
  // module-scope helper: the two assertions the stub needs are declared right here.
  const assert = {
    ok(value, message) {
      if (!value) {
        throw new Error(message ?? 'IPC contract violation');
      }
    },
    equal(actual, expected, message) {
      if (actual !== expected) {
        throw new Error(
          `${message ?? 'IPC contract violation'}: expected ${String(expected)}, received ${String(actual)}`,
        );
      }
    },
  };
  const localized = (code, de, en, es, fr) => ({ code, de, en, es, fr });
  const calls = [];
  const callbacks = new Map();
  const listeners = new Map();
  const tracks = new Map();
  const jobs = new Map();
  let callbackId = 0;
  let nextId = 1;
  let license = {
    active: false,
    demoMode: true,
    plan: null,
    expiresAt: null,
    secondsRemaining: null,
    message: 'Demo mode: activate a license to unlock recording.',
  };
  let audio = {
    loaded: false,
    playing: false,
    finished: false,
    failed: false,
    backend: 'null',
    sampleRate: 48_000,
    activeStems: 4,
    playbackRate: 1,
    limiterLatencyMs: 0,
    masterGainReductionDb: 0,
    positionFrames: 0,
    underruns: 0,
    deviceErrors: 0,
  };
  let deck = { trackId: null, controls: null };
  let recording = {
    active: false,
    statusCode: 'recording.ready',
    format: null,
    sampleRate: null,
    bitDepth: null,
    path: null,
    framesWritten: 0,
    droppedFrames: 0,
    error: null,
  };

  function requireLicense(feature) {
    if (!license.active) {
      throw new Error(
        `Demo mode: the ${feature} entitlement requires an active license, so the native gate failed closed`,
      );
    }
  }

  function audioStatus() {
    return { ...audio };
  }

  function trackSummary(track) {
    return {
      id: track.id,
      filePath: track.filePath,
      title: track.title,
      artist: track.artist,
      durationMs: track.durationMs,
      codec: track.codec,
      bitrateBps: track.bitrateBps,
      sampleRate: track.sampleRate,
      channelCount: track.channelCount,
      tagsJson: track.tagsJson,
      bpm: track.bpm,
      bpmConfidence: track.bpmConfidence,
      keyConfidence: track.keyConfidence,
      isUserVerified: track.isUserVerified,
      musicalKey: track.musicalKey,
      camelotKey: track.camelotKey,
      playCount: 0,
    };
  }

  const handlers = {
    // ---- Tauri event plugin ------------------------------------------------------------
    'plugin:event|listen': ({ event, handler }) => {
      const id = nextId;
      nextId += 1;
      listeners.set(id, { event, handler });
      return id;
    },
    'plugin:event|unlisten': ({ eventId }) => {
      listeners.delete(eventId);
      return null;
    },

    // ---- licensing ---------------------------------------------------------------------
    get_license_status: () => ({ ...license }),
    get_device_binding: () => 'a'.repeat(64),
    activate_license: ({ plan = 'Monthly' }) => {
      license = {
        active: true,
        demoMode: false,
        plan,
        expiresAt: Math.floor(Date.now() / 1000) + 30 * 24 * 60 * 60,
        secondsRemaining: 30 * 24 * 60 * 60,
        message: 'License active.',
      };
      return { ...license };
    },

    // ---- import pipeline ---------------------------------------------------------------
    start_import_job: ({ request }) => {
      assert.ok(request, 'start_import_job needs the request wrapper');
      assert.ok(Array.isArray(request.paths) && request.paths.length > 0);
      assert.equal(typeof request.separationModelPath, 'string');
      assert.equal(typeof request.whisperModelPath, 'string');
      assert.equal(typeof request.useDirectml, 'boolean');
      assert.ok(Number.isInteger(request.cpuThreads));
      const id = nextId;
      nextId += 1;
      const job = {
        id,
        status: 'queued',
        statusMessage: localized(
          'import.queued',
          'Wartend',
          'Queued',
          'En cola',
          'En attente',
        ),
        totalItems: request.paths.length,
        queuedItems: request.paths.length,
        runningItems: 0,
        successfulItems: 0,
        failedItems: 0,
        cancelledItems: 0,
        currentPath: null,
        currentStage: null,
        currentStageMessage: null,
        currentProgress: 0,
        error: null,
        createdAt: Math.floor(Date.now() / 1000),
        updatedAt: Math.floor(Date.now() / 1000),
      };
      jobs.set(id, job);
      return job;
    },
    get_import_jobs: ({ limit = 20 } = {}) =>
      [...jobs.values()].slice(0, limit).map((job) => ({ ...job })),

    // ---- library -----------------------------------------------------------------------
    upsert_track: (input) => {
      assert.equal(typeof input.filePath, 'string');
      const id = nextId;
      nextId += 1;
      tracks.set(id, {
        id,
        filePath: input.filePath,
        title: input.title,
        artist: input.artist,
        durationMs: input.durationMs ?? null,
        codec: input.codec ?? null,
        bitrateBps: input.bitrateBps ?? null,
        sampleRate: input.sampleRate ?? null,
        channelCount: input.channelCount ?? null,
        tagsJson: input.tagsJson ?? '{}',
        bpm: input.bpm ?? null,
        bpmConfidence: 0.42,
        keyConfidence: 0.42,
        isUserVerified: false,
        musicalKey: null,
        camelotKey: null,
      });
      return id;
    },
    search_tracks_page: ({ query = '', limit = 50 } = {}) => {
      assert.ok(query.length <= 256, 'search query stays inside the Rust limit');
      return [...tracks.values()].slice(0, limit).map(trackSummary);
    },
    correct_track_grid: ({ trackId, correction }) => {
      const track = tracks.get(trackId);
      assert.ok(track, `correct_track_grid needs a stored track, got ${trackId}`);
      assert.ok(correction && typeof correction === 'object');
      if (correction.bpm !== undefined && correction.bpm !== null) {
        assert.ok(
          Number.isFinite(correction.bpm) && correction.bpm >= 30 && correction.bpm <= 300,
          'the Rust side accepts a BPM correction only between 30 and 300',
        );
        track.bpm = correction.bpm;
      }
      if (correction.gridShiftMs !== undefined && correction.gridShiftMs !== null) {
        assert.ok(
          Math.abs(correction.gridShiftMs) <= 2_000,
          'the Rust side accepts a grid shift of at most 2000 ms',
        );
      }
      if (correction.musicalKey) track.musicalKey = correction.musicalKey;
      if (correction.camelotKey) track.camelotKey = correction.camelotKey;
      track.bpmConfidence = 1;
      track.keyConfidence = 1;
      track.isUserVerified = true;
      return {
        trackId,
        title: track.title,
        bpm: track.bpm,
        bpmConfidence: 1,
        keyConfidence: 1,
        isUserVerified: true,
        musicalKey: track.musicalKey,
        camelotKey: track.camelotKey,
        beats: [0, 0.46875, 0.9375],
        gridShiftMs: correction.gridShiftMs ?? 0,
        liveDeckUpdated: deck.trackId === trackId,
        mixEngineUpdated: false,
        compatibleCamelot: ['8B', '9A', '7A'],
        adoptionWarning: null,
      };
    },
    get_track_waveform_packet: () => new Uint8Array(0),

    // ---- audio / deck ------------------------------------------------------------------
    load_track_audio: ({ trackId }) => {
      const track = tracks.get(trackId);
      assert.ok(track, `load_track_audio needs a stored track, got ${trackId}`);
      deck = { trackId, controls: null };
      audio = { ...audio, loaded: true, playing: false, failed: false };
      return audioStatus();
    },
    set_audio_controls: ({ update }) => {
      assert.ok(update && typeof update === 'object');
      return audioStatus();
    },
    set_deck_audio_controls: ({ deck: index, update }) => {
      assert.ok(index === 0 || index === 1, 'only two decks exist');
      assert.ok(update && typeof update === 'object');
      deck.controls = { index, update };
      return audioStatus();
    },
    play_audio: () => {
      audio = { ...audio, playing: true };
      return audioStatus();
    },
    pause_audio: () => {
      audio = { ...audio, playing: false };
      return audioStatus();
    },
    get_audio_status: () => audioStatus(),
    get_mix_engine_status: () => ({ running: false, playing: false, trackId: null }),
    set_mix_engine_intensity: () => ({ running: false, playing: false, trackId: null }),
    set_mix_engine_preset: () => ({ running: false, playing: false, trackId: null }),

    // ---- master recording --------------------------------------------------------------
    get_recording_formats: () => [
      {
        id: 'wav24',
        label: 'WAV 24-bit',
        extension: 'wav',
        description: 'PCM 24-bit',
      },
    ],
    authorize_recording: () => {
      requireLicense('record');
      return { ...license };
    },
    start_master_recording: ({ format = 'wav24' } = {}) => {
      requireLicense('record');
      recording = {
        active: true,
        statusCode: 'recording.armed',
        format,
        sampleRate: 48_000,
        bitDepth: 24,
        path: '/recordings/championvibe-live.wav',
        framesWritten: 0,
        droppedFrames: 0,
        error: null,
      };
      return { ...recording };
    },
    get_master_recording_status: () => ({ ...recording }),
    stop_master_recording: () => {
      requireLicense('save');
      recording = { ...recording, active: false, statusCode: 'recording.ready' };
      return {
        name: 'championvibe-live.wav',
        path: recording.path,
        format: recording.format,
        sampleRate: recording.sampleRate,
        channels: 2,
        bitDepth: recording.bitDepth,
        frames: 48_000 * 184,
        sizeBytes: 48_000 * 184 * 3 * 2,
      };
    },
    save_recording_metadata: (input) => {
      requireLicense('save');
      for (const key of [
        'name',
        'filePath',
        'durationMs',
        'format',
        'sampleRate',
        'channelCount',
        'bitDepth',
        'fileSizeBytes',
      ]) {
        assert.ok(key in input, `save_recording_metadata must send ${key}`);
      }
      const id = nextId;
      nextId += 1;
      return id;
    },
  };

  const internals = {
    metadata: { currentWindow: { label: 'main' }, currentWebview: { label: 'main' } },
    async invoke(command, args = {}) {
      calls.push({ command, args: JSON.parse(JSON.stringify(args ?? {})) });
      const handler = handlers[command];
      assert.ok(handler, `unmocked native command: ${command}`);
      return handler(args ?? {});
    },
    transformCallback(callback, once = false) {
      callbackId += 1;
      callbacks.set(callbackId, { callback, once });
      return callbackId;
    },
    unregisterCallback(id) {
      callbacks.delete(id);
    },
    convertFileSrc(path, protocol = 'asset') {
      return `${protocol}://localhost/${encodeURIComponent(path)}`;
    },
  };

  // Browser mode: the page reads the same bridge the WebView would use.
  if (typeof window !== 'undefined') {
    window.__TAURI_INTERNALS__ = internals;
    window.__TAURI_INTERNALS_CALLS__ = calls;
  }

  return {
    internals,
    calls,
    /** Emits an app event exactly like `import/mod.rs` does through `AppHandle::emit`. */
    emit(event, payload) {
      let delivered = 0;
      for (const [id, listener] of listeners) {
        if (listener.event !== event) continue;
        const entry = callbacks.get(listener.handler);
        assert.ok(entry, `event ${event} was registered without a callback`);
        entry.callback({ event, id, payload });
        if (entry.once) callbacks.delete(listener.handler);
        delivered += 1;
      }
      return delivered;
    },
    listenerCount(event) {
      let count = 0;
      for (const listener of listeners.values()) {
        if (listener.event === event) count += 1;
      }
      return count;
    },
    /** Grants a Pro license without going through the issuer, for recording-flow setup. */
    grantLicense(plan = 'Monthly') {
      return handlers.activate_license({ plan });
    },
    commands() {
      return calls.map((call) => call.command);
    },
    lastCall(command) {
      const matches = calls.filter((call) => call.command === command);
      return matches[matches.length - 1];
    },
    reset() {
      calls.length = 0;
    },
  };
}

/**
 * Minimal `@tauri-apps/api` v2 client over a stub.
 *
 * `listen` mirrors `event.js`: the handler goes through `transformCallback` and the registration is
 * a `plugin:event|listen` invocation, so the driver exercises the same bridge the WebView uses.
 */
export function createIpcClient(stub) {
  return {
    invoke: (command, args) => stub.internals.invoke(command, args),
    emit: (event, payload) => stub.emit(event, payload),
    async listen(event, handler) {
      const handlerId = stub.internals.transformCallback(handler);
      const eventId = await stub.internals.invoke('plugin:event|listen', {
        event,
        target: { kind: 'Any' },
        handler: handlerId,
      });
      return async () => {
        await stub.internals.invoke('plugin:event|unlisten', { event, eventId });
        stub.internals.unregisterCallback(handlerId);
      };
    },
  };
}

/** Imports one audio file and follows the job through the single app event. */
export async function runImportFlow(ipc, stub) {
  const events = [];
  const unlisten = await ipc.listen('import-job-updated', (event) => events.push(event));
  assert.equal(stub.listenerCount('import-job-updated'), 1);

  const job = await ipc.invoke('start_import_job', {
    request: {
      paths: ['/library/riddim-01.wav'],
      separationModelPath: '/models/spleeter-2stems.onnx',
      whisperModelPath: '/models/whisper-tiny.bin',
      useDirectml: false,
      cpuThreads: 4,
    },
  });
  assert.equal(typeof job.id, 'number');
  assert.equal(job.status, 'queued');
  assert.equal(job.totalItems, 1);
  assert.ok(job.statusMessage?.en && job.statusMessage?.de, 'job messages stay localized');

  const request = stub.lastCall('start_import_job').args.request;
  assert.deepEqual(Object.keys(request).sort(), [
    'cpuThreads',
    'paths',
    'separationModelPath',
    'useDirectml',
    'whisperModelPath',
  ]);

  const delivered = ipc.emit('import-job-updated', {
    ...job,
    status: 'running',
    runningItems: 1,
    currentProgress: 0.5,
  });
  assert.equal(delivered, 1, 'the running job must reach the listener');
  assert.equal(events[0].event, 'import-job-updated');
  assert.equal(events[0].payload.status, 'running');
  assert.equal(events[0].payload.currentProgress, 0.5);

  const jobs = await ipc.invoke('get_import_jobs', { limit: 20 });
  assert.equal(jobs.length, 1);
  assert.equal(jobs[0].id, job.id);
  await unlisten();
  assert.equal(stub.listenerCount('import-job-updated'), 0, 'the listener must be released');
  return job;
}

/** Stores a track and loads it into the single deck through the native audio path. */
export async function runDeckFlow(ipc) {
  const trackId = await ipc.invoke('upsert_track', {
    filePath: '/library/riddim-01.wav',
    title: 'Riddim 01',
    artist: 'Test Selector',
    durationMs: 184_000,
    codec: 'pcm_s24le',
    bitrateBps: 2_304_000,
    sampleRate: 48_000,
    channelCount: 2,
    bpm: 70,
    tagsJson: '{}',
  });
  assert.equal(typeof trackId, 'number');

  const loaded = await ipc.invoke('load_track_audio', { trackId });
  assert.equal(loaded.loaded, true);
  assert.equal(loaded.playing, false);
  assert.equal(loaded.activeStems, 4);

  await ipc.invoke('set_audio_controls', { update: { stemGains: [1, 1, 0.75, 0] } });
  await ipc.invoke('set_deck_audio_controls', { deck: 1, update: { gain: 0.8 } });
  const playing = await ipc.invoke('play_audio');
  assert.equal(playing.playing, true);
  const status = await ipc.invoke('get_audio_status');
  assert.equal(status.playing, true);

  return trackId;
}

/** Applies a manual beat-grid correction and verifies the row, the badge and the deck agree. */
export async function runGridFlow(ipc, trackId) {
  const { applyCorrectionToTrack, clampGridShiftMs, nudgeBpm } = await import(
    '../src/ui/grid_editor.js'
  );
  const correction = {
    bpm: nudgeBpm(70, 0.1),
    gridShiftMs: clampGridShiftMs(-10),
    musicalKey: 'A minor',
    camelotKey: '8A',
  };
  assert.equal(correction.bpm, 70.1);
  assert.equal(correction.gridShiftMs, -10, 'grid shifts are whole milliseconds');

  const response = await ipc.invoke('correct_track_grid', { trackId, correction });
  assert.equal(response.trackId, trackId);
  assert.equal(response.bpm, 70.1);
  assert.equal(response.gridShiftMs, -10);
  assert.equal(response.isUserVerified, true);
  assert.equal(response.bpmConfidence, 1, 'a manual correction is trusted');
  assert.equal(response.camelotKey, '8A');
  assert.deepEqual(response.compatibleCamelot, ['8B', '9A', '7A']);
  assert.equal(response.liveDeckUpdated, true, 'the loaded deck adopts the corrected grid');

  const patched = applyCorrectionToTrack({ id: trackId, bpm: 70, bpmConfidence: 0.4 }, response);
  assert.equal(patched.bpm, 70.1);
  assert.equal(patched.bpmConfidence, 1);
  assert.equal(patched.keyConfidence, 1);
  assert.equal(patched.isUserVerified, true);
  assert.equal(patched.camelotKey, '8A');

  const page = await ipc.invoke('search_tracks_page', { query: '', limit: 50 });
  assert.equal(page.length, 1);
  assert.equal(page[0].bpm, 70.1, 'the corrected BPM is what the list shows');
  assert.equal(page[0].bpmConfidence, 1);
  return response;
}

/** Runs the licensed recording loop: gate, arm, capture, stop and store the metadata. */
export async function runRecordingFlow(ipc, stub) {
  const demo = await ipc.invoke('get_license_status');
  assert.equal(demo.active, false);
  assert.equal(demo.demoMode, true);
  await assert.rejects(() => ipc.invoke('authorize_recording'), /Demo mode/);

  stub.grantLicense('Monthly');
  const status = await ipc.invoke('get_license_status');
  assert.equal(status.active, true);

  await ipc.invoke('authorize_recording');
  const started = await ipc.invoke('start_master_recording', { format: 'wav24' });
  assert.equal(started.active, true);
  assert.equal(started.format, 'wav24');
  assert.equal(started.sampleRate, 48_000);

  const live = await ipc.invoke('get_master_recording_status');
  assert.equal(live.active, true);
  assert.equal(live.droppedFrames, 0);

  const file = await ipc.invoke('stop_master_recording');
  assert.equal(file.name, 'championvibe-live.wav');
  assert.equal(file.format, 'wav24');
  assert.equal(file.channels, 2);
  assert.ok(file.frames > 0);

  const savedId = await ipc.invoke('save_recording_metadata', {
    name: file.name,
    filePath: file.path,
    durationMs: 184_000,
    format: file.format,
    sampleRate: file.sampleRate,
    channelCount: file.channels,
    bitDepth: file.bitDepth,
    bitrateBps: 2_304_000,
    fileSizeBytes: file.sizeBytes,
  });
  assert.equal(typeof savedId, 'number');

  const sent = stub.lastCall('save_recording_metadata').args;
  assert.deepEqual(Object.keys(sent).sort(), [
    'bitDepth',
    'bitrateBps',
    'channelCount',
    'durationMs',
    'filePath',
    'fileSizeBytes',
    'format',
    'name',
    'sampleRate',
  ]);
  assert.equal(stub.commands().includes('start_master_recording'), true);
}

/** Every command a flow expects must appear in the slice of the log it produced. */
function assertFlowCommands(calls, flow) {
  const used = calls.map((call) => call.command);
  for (const command of EXPECTED_COMMANDS[flow]) {
    assert.ok(used.includes(command), `${flow} flow must call ${command}, saw ${used.join(', ')}`);
  }
}

/** Every flow against a fresh stub, in the order a real session runs them. */
export async function runAllFlows({ log = () => {} } = {}) {
  const stub = createTauriIpcStub();
  const ipc = createIpcClient(stub);
  const completed = [];

  async function step(name, run) {
    const from = stub.calls.length;
    const result = await run();
    assertFlowCommands(stub.calls.slice(from), name);
    completed.push(name);
    return result;
  }

  const job = await step('import', () => runImportFlow(ipc, stub));
  log(`import: job ${job.id} queued and reported through import-job-updated`);

  const trackId = await step('deck', () => runDeckFlow(ipc));
  log(`deck: track ${trackId} loaded into the deck with stem gains applied`);

  const correction = await step('grid', () => runGridFlow(ipc, trackId));
  log(`grid: BPM ${correction.bpm} / shift ${correction.gridShiftMs} ms adopted by SQLite and deck`);

  await step('recording', () => runRecordingFlow(ipc, stub));
  log('recording: demo gate refused, licensed gate armed, file stopped and stored');

  return { stub, completed, commands: stub.commands() };
}

// ---------------------------------------------------------------------------------------------
// Playwright registration (optional)
// ---------------------------------------------------------------------------------------------

let playwright = null;
try {
  playwright = await import('@playwright/test');
} catch {
  playwright = null;
}

if (playwright?.test) {
  const { test, expect } = playwright;
  const distIndex = resolve(repositoryRoot, 'dist/index.html');

  test.describe('ChampionVibe native IPC (headless)', () => {
    test('the four flows satisfy the native command contract', async () => {
      const { completed } = await runAllFlows();
      expect(completed).toEqual(['import', 'deck', 'grid', 'recording']);
    });

    test('the built frontend reaches Rust through the stubbed bridge', async ({ page }) => {
      test.skip(
        !existsSync(distIndex),
        'dist/index.html is missing; run `npm run build` before the browser test',
      );
      await page.addInitScript(`(${createTauriIpcStub.toString()})()`);
      page.on('pageerror', (error) => {
        throw error;
      });
      await page.goto(pathToFileURL(distIndex).href);
      await page.waitForFunction(() => (window.__TAURI_INTERNALS_CALLS__ ?? []).length > 0);
      const commands = await page.evaluate(() =>
        (window.__TAURI_INTERNALS_CALLS__ ?? []).map((call) => call.command),
      );
      expect(commands).toContain('get_license_status');
    });
  });
}

// ---------------------------------------------------------------------------------------------
// Direct driver mode
// ---------------------------------------------------------------------------------------------

const invokedDirectly =
  typeof process.argv[1] === 'string' &&
  pathToFileURL(resolve(process.argv[1])).href === import.meta.url;

if (invokedDirectly) {
  const started = Date.now();
  try {
    const { completed, commands } = await runAllFlows({ log: (line) => console.log(`  ${line}`) });
    const unique = [...new Set(commands)];
    console.log(
      `\nTauri IPC driver: ${completed.length}/4 flows passed (${unique.length} distinct commands) in ${Date.now() - started} ms`,
    );
    console.log(`  commands: ${unique.join(', ')}`);
    if (!playwright) {
      console.log(
        '  note: @playwright/test is not installed, so only the stub-driven driver ran. The flows are the same ones the browser tests use.',
      );
    }
    process.exit(0);
  } catch (error) {
    console.error(`\nTauri IPC driver failed: ${error.message}`);
    console.error(error.stack);
    process.exit(1);
  }
}
