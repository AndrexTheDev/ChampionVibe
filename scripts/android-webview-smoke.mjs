// Test the built Android-facing UI in Chromium with simulated Tauri IPC, not a device or APK.
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { extname, resolve, sep } from 'node:path';
import { chromium } from 'playwright-core';
import { createTauriIpcStub, repositoryRoot } from '../tests/tauri_e2e.spec.mjs';

const root = resolve(repositoryRoot, 'dist');
await readFile(resolve(root, 'index.html')).catch(() => {
  throw new Error('Build the app frontend first with npm run build. No APK is needed.');
});
const contentTypes = { '.html': 'text/html', '.css': 'text/css', '.js': 'text/javascript', '.wasm': 'application/wasm' };
const server = createServer(async (request, response) => {
  try {
    const { pathname } = new URL(request.url, 'http://localhost');
    const path = resolve(root, `.${pathname === '/' ? '/index.html' : pathname}`);
    if (!path.startsWith(root + sep)) throw new Error('outside app build');
    const data = await readFile(path);
    response.setHeader('Content-Type', contentTypes[extname(path)] || 'application/octet-stream');
    response.end(data);
  } catch {
    response.writeHead(404).end();
  }
});
await new Promise((ready) => server.listen(0, '127.0.0.1', ready));

const userAgent = 'Mozilla/5.0 (Linux; Android 15; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/133.0.0.0 Mobile Safari/537.36';
let browser;
try {
  browser = await chromium.launch({ executablePath: process.env.CHROMIUM_EXECUTABLE || undefined });
  const page = await browser.newPage({ viewport: { width: 360, height: 800 }, isMobile: true, hasTouch: true, userAgent, locale: 'en-US' });
  const errors = [];
  page.on('pageerror', (error) => errors.push(error.message));
  await page.addInitScript(`(() => {
    const stub = (${createTauriIpcStub.toString()})();
    const original = stub.internals.invoke;
    localStorage.setItem('championvibe.whisperModelPath', '/app/models/ggml-tiny.bin');
    window.flushAudioPicker = null;
    stub.internals.invoke = async (command, args = {}) => {
      const responses = {
        get_model_catalog: [], get_separator_catalog: [], get_installed_separator: null,
        list_byom_models: [], get_sample_catalog: [],
        get_riddim_families: [], get_tts_engine_status: false,
        set_auto_rewind: false, get_auto_rewind: false,
        pick_android_model: '/app/models/htdemucs.onnx',
        validate_byom_model: { contract: 'HT-Demucs' },
        import_byom_model: { path: '/app/models/htdemucs.onnx' },
      };
      if (command === 'pick_android_import_paths') {
        stub.calls.push({ command, args });
        return args.directory ? [] : new Promise((resolve) => { window.flushAudioPicker = resolve; });
      }
      if (Object.hasOwn(responses, command)) {
        stub.calls.push({ command, args });
        return responses[command];
      }
      return original(command, args);
    };
  })()`);
  await page.goto(`http://127.0.0.1:${server.address().port}/`);
  await page.waitForFunction(() => document.querySelector('#app-shell').dataset.licenseTier === 'demo');

  const callCount = (command) => page.evaluate((name) => window.__TAURI_INTERNALS_CALLS__.filter((call) => call.command === name).length, command);
  const lastCall = (command) => page.evaluate((name) => window.__TAURI_INTERNALS_CALLS__.filter((call) => call.command === name).at(-1), command);
  const activeStems = await page.locator('[data-stem-index]:enabled').count();
  assert.equal(activeStems, 2);
  assert.equal(await page.locator('#license-modal').evaluate((element) => element.open), false);
  assert.equal(await page.locator('#waveform-mode').inputValue(), 'progress');
  assert.equal(await page.locator('#waveform-mode option[value="canvas"]').isDisabled(), true);
  await page.locator('#record-button').click();
  assert.equal(await page.locator('#license-modal').evaluate((element) => element.open), true);
  assert.equal(await callCount('start_master_recording'), 0);
  await page.locator('#close-license-modal').click();

  await page.locator('.local-import-disclosure > summary').click();
  await page.locator('#pick-audio-files').click();
  assert.equal(await page.locator('#pick-audio-folders').isDisabled(), true);
  const audioToken = '{"uri":"content://provider/document/demo.wav"}';
  await page.evaluate((token) => window.flushAudioPicker([token]), audioToken);
  await page.waitForFunction(() => !document.querySelector('#pick-audio-folders').disabled);
  await page.locator('#pick-audio-folders').click(); // cancelled picker must preserve the chosen file
  assert.deepEqual((await lastCall('pick_android_import_paths')).args, { directory: true });

  await page.locator('.model-library-disclosure > summary').click();
  assert.equal(await page.locator('#byom-import').count(), 1);
  assert.equal(await page.locator('#byom-import').isDisabled(), true);
  await page.locator('#byom-dropzone').click({ force: true });
  assert.equal(await callCount('pick_android_model'), 0);
  await page.locator('#byom-rights-ack').check();
  assert.equal(await callCount('pick_android_model'), 0);
  await page.locator('#byom-import').click();
  await page.waitForFunction(() => document.querySelector('#separator-model-path').value.endsWith('/htdemucs.onnx'));
  assert.deepEqual((await lastCall('pick_android_model')).args, { kind: 'separator' });
  assert.equal(await callCount('plugin:dialog|open'), 0);
  assert.equal((await lastCall('import_byom_model')).args.acknowledged, true);
  await page.locator('#start-import').click();
  const imported = (await lastCall('start_import_job')).args.request;
  assert.deepEqual(imported.paths, [audioToken]);
  assert.equal(imported.separationModelPath, '/app/models/htdemucs.onnx');
  assert.equal(imported.useDirectml, false);

  for (const locale of ['de', 'en', 'es', 'fr']) {
    await page.locator('#language-select').selectOption(locale);
    assert.equal(await page.locator('html').getAttribute('lang'), locale);
    for (const width of [320, 360, 412]) {
      await page.setViewportSize({ width, height: 800 });
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth), width, `${locale}: mobile app overflows at ${width}px`);
    }
  }
  await page.locator('#toggle-tools').click();
  assert.equal(await page.locator('#app-shell').getAttribute('data-tools-open'), 'true');
  assert.equal(await page.evaluate(() => document.documentElement.scrollWidth), 412);
  await page.locator('#toggle-performance').click();
  assert.equal(await page.locator('#app-shell').getAttribute('data-view-mode'), 'performance');
  assert.equal(await page.evaluate(() => document.documentElement.scrollWidth), 412);
  assert.deepEqual(errors, []);
  console.log('Android WebView simulation passed: Demo, recording gate, SAF file/folder and model import, playback UI and four locales at three portrait widths.');
} finally {
  await browser?.close();
  await new Promise((done) => server.close(done));
}
