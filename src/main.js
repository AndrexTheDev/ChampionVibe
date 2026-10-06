import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { open } from '@tauri-apps/plugin-dialog';
import './styles.css';
import { decodeWaveformPacket } from './waveform.js';
import { getAudioFormat } from './audio-format.js';
import { applyStaticTranslations, getLocale, setLocale, t } from './i18n.js';
import { parseSemanticTrackQuery } from './search.js';
import { createKokoroWasmService, pushWasmTtsAudioBuffer } from './services/tts_wasm.js';

applyStaticTranslations(document);
const $ = (selector) => document.querySelector(selector);
const isTauri = Boolean(window.__TAURI_INTERNALS__);
const platformText = `${navigator.userAgent || ''} ${navigator.userAgentData?.platform || ''}`;
const isAndroid = /android/i.test(platformText);
const PAGE_SIZE = 200;
const TRACK_ROW_HEIGHT = 66;
const TRACK_OVERSCAN = 4;
const STEM_NAMES = Object.freeze(['Vocals', 'Instrumental', 'Bass', 'Drums']);
const BUSY_BUTTON_IDLE_LABELS = Object.freeze({
  'create-quote': 'Angebot berechnen',
  'activate-license': 'Zahlung verifizieren & aktivieren',
  'generate-dubplate': 'Dubplate rendern & laden',
  'save-riddim-tags': 'Tags speichern & Familien neu berechnen',
  'rebuild-riddims': 'Analysieren ↻',
  'start-import': 'Importieren & analysieren',
});
const SESSION_CACHE_KEY = 'championvibe.proSession.v1';
const storedWaveformMode = localStorage.getItem('championvibe.waveformMode');
const initialWaveformMode = isAndroid
  ? (storedWaveformMode === 'off' ? 'off' : 'progress')
  : (['canvas', 'progress', 'off'].includes(storedWaveformMode) ? storedWaveformMode : 'canvas');
const storedIntensity = Number.parseInt(localStorage.getItem('championvibe.mixIntensity') || '2', 10);
const initialIntensity = Number.isInteger(storedIntensity) && storedIntensity >= 1 && storedIntensity <= 4
  ? storedIntensity
  : 2;
const storedAutoGainTarget = Number(localStorage.getItem('championvibe.autoGainTargetLufs') || '-14');
const initialAutoGainTarget = Number.isFinite(storedAutoGainTarget) && Math.abs(storedAutoGainTarget + 16) < 0.01
  ? -16
  : -14;
const initialAutoGainEnabled = localStorage.getItem('championvibe.autoGainEnabled') !== 'false';
const initialPitchLockEnabled = localStorage.getItem('championvibe.pitchLockEnabled') !== 'false';
const state = {
  plan: 'monthly',
  quote: null,
  paymentOptions: {},
  oracleStatus: '',
  oracleRequestId: 0,
  licenseStatus: null,
  binding: null,
  wallet: null,
  audioTrack: null,
  selectedTrack: null,
  audioStatus: null,
  stemGains: [1, 1, 1, 1],
  autoGainTargetLufs: initialAutoGainTarget,
  autoGainEnabled: initialAutoGainEnabled,
  pitchLockEnabled: initialPitchLockEnabled,
  stemBusy: false,
  visibleStemLimit: null,
  audioBusy: false,
  mixStatus: null,
  mixBusy: false,
  mixIntensity: initialIntensity,
  intensityOverride: localStorage.getItem('championvibe.intensityOverride') === 'true',
  intensityBusy: false,
  intensityEditing: false,
  performanceBusy: false,
  sampleCatalog: [],
  sampleFolder: '',
  sampleFolderError: '',
  piperModelPath: localStorage.getItem('championvibe.piperModelPath') || '',
  piperConfigPath: localStorage.getItem('championvibe.piperConfigPath') || '',
  ttsAvailable: false,
  kokoroWasmService: null,
  kokoroWasmReady: false,
  kokoroWasmBusy: false,
  riddimBusy: false,
  riddimFamilies: [],
  riddimFamilyError: '',
  selectedRiddimFamily: null,
  selectedRiddimTagError: '',
  toastTimer: null,
  importPaths: [],
  importJobs: [],
  importBusy: false,
  tracks: new Map(),
  trackIndexById: new Map(),
  trackTotal: 0,
  trackQuery: '',
  trackSearch: { text: '', filters: {} },
  trackRequestId: 0,
  trackPendingPages: new Set(),
  trackRowPool: [],
  trackPlane: null,
  trackRenderFrame: 0,
  trackSearchTimer: 0,
  waveformMode: initialWaveformMode,
  waveformTrackId: null,
  waveformPeaks: null,
  waveformBeatGrid: null,
  waveformBpm: null,
  waveformContext: null,
  waveformRequestId: 0,
  waveformResizeObserver: null,
  trackResizeObserver: null,
  progressRaf: 0,
  progressSyncTime: 0,
  proSessionTimer: 0,
  restoringSession: false,
  sessionRestored: false,
  recordBusy: false,
  recordAuthorized: false,
  recordingActive: false,
  recordingSnapshot: null,
  recordingFormat: localStorage.getItem('championvibe.recordingFormat') || 'wavPcm16',
  separationModelPath: localStorage.getItem('championvibe.separationModelPath') || '',
  whisperModelPath: localStorage.getItem('championvibe.whisperModelPath') || '',
  modelCatalog: [],
  modelBusyId: null,
  customModelBusy: false,
};

const chainNames = { solana: 'Solana', bitcoin: 'Bitcoin', ethereum: 'Ethereum' };
const assetNames = { solana: 'SOL', bitcoin: 'BTC', ethereum: 'ETH' };

function toast(message, isError = false) {
  const element = $('#toast');
  element.textContent = t(message);
  element.classList.toggle('error', isError);
  element.classList.add('visible');
  clearTimeout(state.toastTimer);
  state.toastTimer = setTimeout(() => element.classList.remove('visible'), 4200);
}

function shortHash(value) {
  if (!value) return t('nicht verfügbar');
  return value.length > 30 ? `${value.slice(0, 12)} … ${value.slice(-9)}` : value;
}

function dateText(unixSeconds) {
  if (!unixSeconds) return '—';
  return new Intl.DateTimeFormat(getLocale(), { dateStyle: 'medium', timeStyle: 'short' }).format(new Date(unixSeconds * 1000));
}

const RECORDING_FORMAT_LABELS = Object.freeze({
  wavPcm16: 'WAV PCM 16-bit',
  wavPcm24: 'WAV PCM 24-bit',
  flac: 'FLAC PCM 16-bit',
  mp3: 'MP3 320 kbps',
  oggVorbis: 'OGG Vorbis',
  m4aAac: 'M4A/AAC',
});

function recordingFormatLabel(format) {
  return t(RECORDING_FORMAT_LABELS[format] || 'WAV PCM 16-bit');
}

async function syncRecordingFormatAvailability() {
  if (!isTauri) return;
  try {
    const formats = await invoke('get_recording_formats');
    const availability = new Map((formats || []).map((format) => [format.id, Boolean(format.available)]));
    const select = $('#recording-format');
    for (const option of select.options) {
      if (availability.has(option.value)) option.disabled = !availability.get(option.value);
      else option.disabled = true;
    }
    if (select.selectedOptions[0]?.disabled) {
      select.value = [...select.options].find((option) => !option.disabled)?.value || '';
    }
    state.recordingFormat = select.value || 'wavPcm16';
    localStorage.setItem('championvibe.recordingFormat', state.recordingFormat);
  } catch (error) {
    // Keep the statically conservative choices when native encoder discovery is unavailable.
    console.warn('Recording encoder availability is unavailable:', error);
  }
}

function trackMusicalKey(track) {
  return track?.musicalKey || track?.musical_key || null;
}

function trackDubplateEmotion(track) {
  const label = String(track?.moodLabel || track?.mood_label || '').toLowerCase();
  if (/aggressive/u.test(label)) return 'aggressive';
  if (/euphoric|high energy/u.test(label)) return 'energetic';
  if (/chill|calm|melancholic/u.test(label)) return 'calm';
  if (/uplifting/u.test(label)) return 'uplifting';
  if (/tense/u.test(label)) return 'conscious';
  return 'confident';
}

function syncDubplateAvailability() {
  const usesKokoroWasm = state.kokoroWasmReady && Boolean(state.kokoroWasmService);
  const hasNativePiper = state.ttsAvailable && Boolean(state.piperModelPath) && Boolean(state.piperConfigPath);
  const hasSelectedTrack = Boolean(state.selectedTrack);
  const keyAvailable = Boolean(trackMusicalKey(state.selectedTrack));
  const ready = isTauri
    && (usesKokoroWasm || hasNativePiper)
    && hasSelectedTrack
    && (usesKokoroWasm || keyAvailable)
    && !state.performanceBusy;
  const reason = !isTauri
    ? 'Dubplate-TTS ist nur in der installierten App verfügbar.'
    : !usesKokoroWasm && !state.ttsAvailable
      ? 'Lade die lokal geprüften Kokoro-Gewichte und die Stimme, dann initialisiere WASM.'
      : !hasSelectedTrack
        ? 'Wähle zuerst einen Track aus.'
        : !usesKokoroWasm && !keyAvailable
          ? 'Für diesen Track wurde kein musikalischer Key analysiert.'
          : !usesKokoroWasm && (!state.piperModelPath || !state.piperConfigPath)
            ? 'Wähle ein lokales Piper-ONNX-Modell und dessen Voice-JSON aus.'
            : state.performanceBusy
              ? 'Eine Live-Aktion wird gerade ausgeführt.'
              : 'Dubplate rendern & laden';
  const button = $('#generate-dubplate');
  const shoutout = $('#dj-shoutout');
  const loadButton = $('#load-kokoro-wasm');
  const actionLabel = usesKokoroWasm ? 'Kokoro-Shoutout abfeuern' : 'Dubplate rendern & laden';
  if (button) {
    button.disabled = !ready;
    if (!button.dataset.original) button.textContent = t(actionLabel);
    button.title = t(ready
      ? usesKokoroWasm ? 'Lokalen Kokoro-WASM-Shoutout abspielen' : 'Dubplate rendern & laden'
      : reason);
  }
  if (shoutout) {
    shoutout.disabled = !ready;
    shoutout.title = t(ready ? 'DJ-Shoutout lokal abfeuern' : reason);
  }
  if (loadButton) loadButton.disabled = !isTauri || state.kokoroWasmBusy || state.kokoroWasmReady;
}

function setBusy(button, busy, label) {
  button.disabled = busy || !isTauri;
  if (busy) {
    if (!button.dataset.original) {
      button.dataset.original = button.innerHTML;
      button.dataset.originalLocale = getLocale();
    }
    button.dataset.busyLabelKey = label;
    button.innerHTML = `<span class="spinner"></span><span>${t(label)}</span>`;
  } else if (button.dataset.original) {
    button.innerHTML = button.dataset.original;
    if (button.dataset.originalLocale !== getLocale()) {
      const idleKey = BUSY_BUTTON_IDLE_LABELS[button.id];
      if (idleKey) {
        const labelNode = button.querySelector('.button-arrow')?.previousElementSibling;
        if (labelNode) labelNode.textContent = t(idleKey);
        else button.textContent = t(idleKey);
      }
    }
    delete button.dataset.original;
    delete button.dataset.originalLocale;
    delete button.dataset.busyLabelKey;
  }
}

function displayStatus(status) {
  state.licenseStatus = status || null;
  if (!status?.active) {
    state.recordAuthorized = false;
    clearTimeout(state.proSessionTimer);
    state.proSessionTimer = 0;
  }
  const banner = $('#license-status');
  const title = $('#status-title');
  const tag = $('#status-tag');
  const message = $('#status-message');
  const expiry = $('#status-expiry');
  const activate = banner.querySelector('.button-dark');
  banner.classList.toggle('licensed', Boolean(status?.active));
  title.textContent = t(status?.active ? 'ChampionVibe Pro aktiv' : 'Demo-Modus');
  tag.textContent = status?.active ? t('LIZENZIERT') : t('DEMO');
  message.textContent = t(status?.message || 'Lizenzstatus nicht verfügbar.');
  expiry.querySelector('strong').textContent = status?.active ? dateText(status.expires_at) : t('Bibliothek verfügbar');
  expiry.querySelector('span').textContent = status?.active ? t('LÄUFT AB AM') : t('FUNKTIONEN');
  activate.innerHTML = status?.active ? `${t('Lizenzdetails')} <span>↗</span>` : `${t('Lizenz aktivieren')} <span>↗</span>`;
  syncFreemiumUI(status);
  if (status?.active && $('#license-modal').open) $('#license-modal').close();
}

function syncFreemiumUI(status = state.licenseStatus) {
  const pro = Boolean(status?.active);
  const shell = $('#app-shell');
  if (shell) shell.dataset.licenseTier = pro ? 'pro' : 'demo';
  const entitlementStems = isAndroid && !pro ? 2 : 4;
  const reportedStems = Number(state.audioStatus?.activeStems);
  const stems = state.audioStatus?.loaded && [2, 4].includes(reportedStems)
    ? reportedStems
    : entitlementStems;
  if (state.visibleStemLimit === null) {
    for (let index = stems; index < 4; index += 1) state.stemGains[index] = 0;
  } else if (stems < state.visibleStemLimit) {
    for (let index = stems; index < state.visibleStemLimit; index += 1) state.stemGains[index] = 0;
  } else if (stems > state.visibleStemLimit) {
    for (let index = state.visibleStemLimit; index < stems; index += 1) state.stemGains[index] = 1;
  }
  state.visibleStemLimit = stems;
  renderStemSwitches(stems);
  const stemLabel = $('#demo-stem-indicator');
  if (stemLabel) stemLabel.textContent = t('{tier} · {count} STEMS', { tier: pro ? 'PRO' : 'DEMO', count: stems });
  const recordState = $('#recording-state-badge');
  const cacheState = $('#session-cache-state');
  const recordCopy = $('#recording-mode-copy');
  const recordButton = $('#record-button');
  const recordingFormat = $('#recording-format');
  const gateButton = $('#check-recording');
  if (recordState) recordState.textContent = t(pro
    ? (state.recordingActive ? 'PRO · MASTER REC ●' : (state.recordAuthorized ? 'PRO · GATE GEPRÜFT' : 'PRO · BEREIT'))
    : 'DEMO · PAYWALL');
  if (cacheState) cacheState.textContent = t(pro ? 'SESSION-CACHE AN' : 'CACHE AUS');
  if (recordCopy) recordCopy.textContent = pro
    ? (state.recordingActive
      ? t('Master-Capture läuft lokal · {format} · beide Decks.', { format: recordingFormatLabel(state.recordingSnapshot?.format || state.recordingFormat) })
      : t('Pro: Master-Capture und lokaler Session-Cache freigeschaltet.'))
    : t('Demo: keine Session-Speicherung · {count} aktive Stem-Kanäle.', { count: stems });
  if (recordButton) {
    recordButton.disabled = state.recordBusy;
    const label = recordButton.querySelector('span');
    if (label) label.textContent = t(state.recordingActive ? 'Stop' : 'Record');
    recordButton.classList.toggle('is-recording', state.recordingActive);
    const action = state.recordingActive ? 'Master-Aufnahme stoppen' : 'Master-Aufnahme starten';
    recordButton.setAttribute('aria-label', t(pro ? action : 'Pro-Lizenz für Aufnahme öffnen'));
    recordButton.title = t(pro ? action : 'Pro-Lizenz erforderlich');
  }
  if (recordingFormat) recordingFormat.disabled = state.recordBusy || state.recordingActive;
  if (gateButton) gateButton.disabled = state.recordBusy;
}

function sessionSnapshot() {
  const track = state.selectedTrack || state.audioTrack;
  return {
    version: 1,
    savedAt: Date.now(),
    viewMode: $('#app-shell').dataset.viewMode,
    preset: $('#mix-preset').value,
    mixIntensity: state.mixIntensity,
    intensityOverride: state.intensityOverride,
    track: track ? {
      id: track.id,
      file_path: track.file_path,
      title: track.title,
      artist: track.artist,
      durationMs: track.durationMs ?? track.duration_ms ?? null,
      bpm: track.bpm ?? null,
      musicalKey: track.musicalKey ?? track.musical_key ?? null,
      sentiment: track.sentiment ?? null,
      acousticTag: track.acousticTag ?? track.acoustic_tag ?? null,
    } : null,
  };
}

function saveProSession() {
  if (!state.licenseStatus?.active || state.restoringSession) return;
  clearTimeout(state.proSessionTimer);
  state.proSessionTimer = window.setTimeout(() => {
    state.proSessionTimer = 0;
    if (!state.licenseStatus?.active || state.restoringSession) return;
    try {
      localStorage.setItem(SESSION_CACHE_KEY, JSON.stringify(sessionSnapshot()));
      const cacheState = $('#session-cache-state');
      if (cacheState) cacheState.textContent = t('SESSION-CACHE AN');
    } catch (error) {
      const cacheState = $('#session-cache-state');
      if (cacheState) cacheState.textContent = t('CACHE FEHLER');
      console.warn('ChampionVibe Pro session cache unavailable:', error);
    }
  }, 80);
}

function restoreProSession() {
  if (!state.licenseStatus?.active || state.sessionRestored) return;
  state.sessionRestored = true;
  try {
    const snapshot = JSON.parse(localStorage.getItem(SESSION_CACHE_KEY) || 'null');
    if (!snapshot || snapshot.version !== 1) return;
    state.restoringSession = true;
    const preset = $('#mix-preset');
    if ([...preset.options].some((option) => option.value === snapshot.preset)) preset.value = snapshot.preset;
    if (Number.isInteger(snapshot.mixIntensity) && snapshot.mixIntensity >= 1 && snapshot.mixIntensity <= 4) {
      state.mixIntensity = snapshot.mixIntensity;
    }
    state.intensityOverride = Boolean(snapshot.intensityOverride);
    if (snapshot.track?.id && snapshot.track?.title) {
      state.selectedTrack = snapshot.track;
      state.audioTrack = snapshot.track;
      updateCurrentTrackUI(snapshot.track);
    }
    if (snapshot.viewMode === 'performance') setViewMode('performance');
    if (state.mixStatus) displayMixStatus(state.mixStatus);
  } catch (error) {
    console.warn('ChampionVibe saved Pro session could not be restored:', error);
  } finally {
    state.restoringSession = false;
  }
}

async function refreshStatus() {
  if (!isTauri) {
    $('#device-fingerprint').textContent = t('nur in der installierten App');
    $('#binding-state').textContent = t('VORSCHAU');
    $('#environment-pill').innerHTML = `<span></span> ${t('Browser-Vorschau')}`;
    const status = { active: false, message: 'Die Vorschau läuft ohne Tauri-Rust-Backend. Lizenz- und Geräteprüfungen sind hier deaktiviert.' };
    displayStatus(status);
    return status;
  }
  $('#environment-pill').innerHTML = `<span></span> ${t('Tauri-App')}`;
  try {
    const [binding, license] = await Promise.all([
      invoke('get_device_binding'),
      invoke('get_license_status'),
    ]);
    state.binding = binding;
    $('#device-fingerprint').textContent = shortHash(binding);
    $('#binding-state').textContent = t('GEBUNDEN');
    displayStatus(license);
    return license;
  } catch (error) {
    $('#device-fingerprint').textContent = t('nicht verfügbar');
    $('#binding-state').textContent = t('FEHLER');
    const status = { active: false, message: String(error) };
    displayStatus(status);
    return status;
  }
}

function selectedChain() {
  return $('#chain-select').value;
}

function openLicenseModal() {
  const dialog = $('#license-modal');
  if (!dialog.open) dialog.showModal();
  loadOraclePrices();
}

function closeLicenseModal() {
  const dialog = $('#license-modal');
  if (dialog.open) dialog.close();
}

function openInfoModal(tab = 'help') {
  const dialog = $('#info-modal');
  if (!dialog.open) dialog.showModal();
  setInfoTab(tab);
}

function closeInfoModal() {
  const dialog = $('#info-modal');
  if (dialog.open) dialog.close();
}

function setInfoTab(tab) {
  for (const button of document.querySelectorAll('[data-info-tab]')) {
    const active = button.dataset.infoTab === tab;
    button.classList.toggle('active', active);
    button.setAttribute('aria-selected', String(active));
  }
  for (const panel of document.querySelectorAll('[data-info-page]')) {
    const active = panel.dataset.infoPage === tab;
    panel.classList.toggle('active', active);
    panel.hidden = !active;
  }
}

function cachedQuoteIsFresh(quote) {
  return Boolean(quote && Number(quote.expires_at) * 1000 > Date.now() + 20_000);
}

function setOracleStatus(key, values = undefined) {
  state.oracleStatus = { key, values };
  $('#oracle-status').textContent = t(key, values);
}

function renderOraclePrices(plan = state.plan) {
  const quotes = state.paymentOptions[plan] || {};
  for (const card of document.querySelectorAll('.oracle-price-card')) {
    const chain = card.dataset.chain;
    const quote = quotes[chain];
    card.classList.toggle('available', Boolean(quote));
    card.querySelector('[data-oracle-price]').textContent = quote
      ? `1 ${quote.asset} ≈ $${quote.asset_usd}`
      : t('Preis nicht verfügbar');
    card.querySelector('[data-eur-price]').textContent = quote
      ? `1 EUR ≈ $${quote.eur_usd}`
      : 'EUR/USD —';
    card.querySelector('[data-recipient-address]').textContent = quote?.recipient || t('Empfänger vom Issuer laden');
    card.querySelector('[data-recipient-address]').title = quote?.recipient || '';
    card.querySelector('[data-published-at]').textContent = quote
      ? t('Chainlink · {date}', { date: dateText(quote.price_published_at) })
      : t('Chainlink-Zeit —');
    card.querySelector('[data-select-chain]').disabled = !quote || !isTauri;
  }
}

async function loadOraclePrices(force = false) {
  if (!isTauri) {
    setOracleStatus('Oracle-Preise und dynamische Empfängeradressen sind in der Browser-Vorschau nicht abrufbar.');
    renderOraclePrices();
    return;
  }
  const plan = state.plan;
  const existing = state.paymentOptions[plan] || {};
  const fresh = ['solana', 'bitcoin', 'ethereum'].every((chain) => cachedQuoteIsFresh(existing[chain]));
  if (!force && fresh) {
    setOracleStatus('Oracle-Referenz aktualisiert · {date}', { date: dateText(Math.min(...Object.values(existing).map((quote) => quote.price_published_at))) });
    renderOraclePrices(plan);
    return;
  }

  const requestId = ++state.oracleRequestId;
  const issuerUrl = $('#issuer-url').value.trim();
  setOracleStatus('Chainlink-Oracle und Issuer werden parallel abgefragt …');
  $('#refresh-oracle-prices').disabled = true;
  try {
    const results = await Promise.allSettled(['solana', 'bitcoin', 'ethereum'].map((chain) =>
      invoke('create_payment_quote', { issuerUrl, chain, plan }),
    ));
    if (requestId !== state.oracleRequestId) return;
    const quotes = {};
    const failures = [];
    results.forEach((result, index) => {
      const chain = ['solana', 'bitcoin', 'ethereum'][index];
      if (result.status === 'fulfilled') quotes[chain] = result.value;
      else failures.push(`${chainNames[chain]}: ${String(result.reason)}`);
    });
    state.paymentOptions[plan] = quotes;
    renderOraclePrices(plan);
    if (Object.keys(quotes).length) {
      const publishedAt = Math.min(...Object.values(quotes).map((quote) => quote.price_published_at));
      setOracleStatus(failures.length
        ? 'Oracle-Zeit {date} · Angebote 15 Min. gültig · {count} Netzwerk(e) nicht verfügbar'
        : 'Oracle-Zeit {date} · Angebote 15 Min. gültig', {
        date: dateText(publishedAt),
        count: failures.length,
      });
    } else {
      setOracleStatus('Issuer/Oracle nicht erreichbar. Issuer-URL prüfen und erneut versuchen.');
    }
  } catch (error) {
    if (requestId === state.oracleRequestId) setOracleStatus('Oracle-Abfrage fehlgeschlagen: {error}', { error: String(error) });
  } finally {
    if (requestId === state.oracleRequestId) $('#refresh-oracle-prices').disabled = false;
  }
}

function setQuote(quote) {
  state.quote = quote;
  $('#chain-select').value = quote.chain;
  $('#quote-card').classList.remove('hidden');
  $('#quote-amount').textContent = `${quote.display_amount} ${quote.asset}`;
  $('#quote-conversion').textContent = `${quote.amount_eur} € · 1 ${quote.asset} = ${quote.asset_usd} USD · 1 EUR = ${quote.eur_usd} USD`;
  $('#quote-recipient').textContent = quote.recipient;
  $('#quote-expiry').textContent = t('gültig bis {date}', { date: dateText(quote.expires_at) });
  $('#tx-hash').placeholder = t(quote.chain === 'bitcoin' ? 'Bitcoin-TXID (64 Hex-Zeichen)' : quote.chain === 'ethereum' ? 'Ethereum-Transaktionshash (0x…)' : 'Solana-Signatur (Base58)');
  $('#activate-license').disabled = !isTauri;
}

async function createQuote() {
  if (!isTauri) return toast('Bitte die Desktop- oder Android-App starten, damit Rust den Issuer sicher anspricht.', true);
  const button = $('#create-quote');
  setBusy(button, true, 'Oracle-Kurs wird geprüft …');
  try {
    const chain = selectedChain();
    const cached = state.paymentOptions[state.plan]?.[chain];
    const quote = cachedQuoteIsFresh(cached)
      ? cached
      : await invoke('create_payment_quote', {
        issuerUrl: $('#issuer-url').value.trim(),
        chain,
        plan: state.plan,
      });
    state.paymentOptions[state.plan] = { ...state.paymentOptions[state.plan], [chain]: quote };
    renderOraclePrices(state.plan);
    setQuote(quote);
    toast(t('Angebot für {chain} erstellt. Es ist 15 Minuten gültig.', { chain: chainNames[quote.chain] }));
  } catch (error) {
    toast(String(error), true);
  } finally {
    setBusy(button, false);
  }
}

async function activate() {
  if (!isTauri) return toast('Aktivierung ist nur in der Tauri-App verfügbar.', true);
  if (!state.quote) return toast('Bitte erst ein Preisangebot erstellen.', true);
  const hash = $('#tx-hash').value.trim();
  if (!hash) return toast('Bitte den Transaktionshash einfügen.', true);
  const button = $('#activate-license');
  setBusy(button, true, 'Finalität und Zahlung werden verifiziert …');
  try {
    const status = await invoke('activate_license', {
      issuerUrl: $('#issuer-url').value.trim(),
      quoteId: state.quote.id,
      transactionHash: hash,
    });
    displayStatus(status);
    restoreProSession();
    saveProSession();
    state.quote = null;
    $('#quote-card').classList.add('hidden');
    $('#tx-hash').value = '';
    toast('Lizenz wurde vom Issuer signiert und lokal verifiziert.');
  } catch (error) {
    toast(String(error), true);
  } finally {
    setBusy(button, false);
  }
}

async function connectWallet() {
  const button = $('#connect-wallet');
  button.disabled = true;
  try {
    let address = null;
    if (window.solana?.connect) {
      const response = await window.solana.connect();
      address = response?.publicKey?.toString?.() || window.solana.publicKey?.toString?.();
    } else if (window.ethereum?.request) {
      const accounts = await window.ethereum.request({ method: 'eth_requestAccounts' });
      address = accounts?.[0] || null;
    } else if (window.unisat?.requestAccounts) {
      const accounts = await window.unisat.requestAccounts();
      address = accounts?.[0] || null;
    }
    if (!address) {
      toast('Keine injizierte Wallet gefunden. In Tauri-WebViews Wallet extern verwenden und anschließend den TxHash einfügen.', true);
      return;
    }
    state.wallet = address;
    $('#wallet-label').textContent = shortHash(address);
    toast(t('Wallet-Adresse verbunden ({address}). Dies ist kein Zahlungsnachweis.', { address: shortHash(address) }));
  } catch (error) {
    toast(t('Wallet-Verbindung abgebrochen: {error}', { error: String(error) }), true);
  } finally {
    button.disabled = !isTauri;
  }
}

async function copyText(value, label) {
  if (!value) return toast(t('{label} ist noch nicht verfügbar.', { label: t(label) }), true);
  try {
    await navigator.clipboard.writeText(value);
    toast(t('{label} kopiert.', { label: t(label) }));
  } catch {
    toast('Kopieren ist in dieser Umgebung nicht verfügbar.', true);
  }
}

async function toggleMasterRecording() {
  if (!state.licenseStatus?.active) {
    openLicenseModal();
    return;
  }
  if (!isTauri) return toast('Master-Aufnahme ist nur in der installierten App verfügbar.', true);
  if (state.recordBusy) return;
  state.recordBusy = true;
  syncFreemiumUI();
  try {
    if (state.recordingActive) {
      const file = await invoke('stop_master_recording');
      state.recordingActive = false;
      state.recordingSnapshot = null;
      state.recordAuthorized = true;
      toast(t('Master-Aufnahme gespeichert: {filename}', { filename: file.name }));
    } else {
      const snapshot = await invoke('start_master_recording', { format: state.recordingFormat });
      state.recordingActive = Boolean(snapshot.active);
      state.recordingSnapshot = snapshot;
      state.recordAuthorized = true;
      toast('Master-Aufnahme läuft: beide Decks werden lokal erfasst.');
    }
  } catch (error) {
    try {
      const snapshot = await invoke('get_master_recording_status');
      state.recordingSnapshot = snapshot;
      state.recordingActive = Boolean(snapshot.active);
      if (snapshot.error) toast(snapshot.message?.de || snapshot.error, true);
      else if (!state.recordingActive) toast(String(error), true);
    } catch {
      const status = await refreshStatus();
      if (!status?.active) openLicenseModal();
      else toast(String(error), true);
    }
  } finally {
    state.recordBusy = false;
    syncFreemiumUI();
  }
}

async function checkRecordingGate() {
  if (!state.licenseStatus?.active) {
    openLicenseModal();
    return;
  }
  if (!isTauri) return toast('Die Pro-Lizenz ist in der Browser-Vorschau nicht prüfbar.', true);
  if (state.recordBusy) return;
  state.recordBusy = true;
  syncFreemiumUI();
  try {
    await invoke('authorize_recording');
    state.recordAuthorized = true;
    toast('Pro-Recording-Gate bestätigt. Die native Aufnahme wird im Rust-Audiopfad freigeschaltet.');
  } catch (error) {
    const status = await refreshStatus();
    if (!status?.active) openLicenseModal();
    else toast(String(error), true);
  } finally {
    state.recordBusy = false;
    syncFreemiumUI();
  }
}

function formatTrackTime(totalSeconds) {
  const seconds = Number.isFinite(totalSeconds) && totalSeconds > 0 ? Math.floor(totalSeconds) : 0;
  return `${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, '0')}`;
}

function clearWaveform() {
  state.waveformRequestId += 1;
  state.waveformTrackId = null;
  state.waveformPeaks = null;
  state.waveformBeatGrid = null;
  state.waveformBpm = null;
  const context = state.waveformContext;
  if (context) {
    const canvas = $('#waveform-canvas');
    context.clearRect(0, 0, canvas.width, canvas.height);
  }
  $('#waveform-mode-note').textContent = t(isAndroid
    ? 'Android: Canvas aus; nur einfache Fortschrittsanzeige oder keine Visualisierung.'
    : 'Peaks werden beim Start lokal aus dem SQLite-Cache geladen oder erzeugt.');
}

function drawWaveform() {
  if (state.waveformMode !== 'canvas' || !state.waveformPeaks?.length || isAndroid) return;
  const canvas = $('#waveform-canvas');
  const rect = canvas.getBoundingClientRect();
  if (!rect.width || !rect.height) return;
  const dpr = Math.min(window.devicePixelRatio || 1, 1.5);
  const width = Math.max(1, Math.round(rect.width * dpr));
  const height = Math.max(1, Math.round(rect.height * dpr));
  if (canvas.width !== width || canvas.height !== height) {
    canvas.width = width;
    canvas.height = height;
  }
  const context = state.waveformContext || canvas.getContext('2d', { alpha: false });
  if (!context) {
    $('#waveform-mode-note').textContent = t('Canvas nicht verfügbar · Fortschrittsleiste bleibt aktiv.');
    return;
  }
  state.waveformContext = context;
  context.clearRect(0, 0, width, height);
  context.fillStyle = '#0b151a';
  context.fillRect(0, 0, width, height);
  const center = height / 2;
  const bucketWidth = width / state.waveformPeaks.length;
  context.fillStyle = '#69e1d0';
  for (let index = 0; index < state.waveformPeaks.length; index += 1) {
    const normalized = state.waveformPeaks[index] / 65535;
    const barHeight = Math.max(2 * dpr, normalized * height * 0.82);
    context.fillRect(index * bucketWidth, center - barHeight / 2, Math.max(1, bucketWidth * 0.66), barHeight);
  }
  if (state.waveformBeatGrid?.length && state.audioTrack?.durationMs) {
    context.fillStyle = 'rgba(220, 245, 240, .18)';
    const durationMs = Number(state.audioTrack.durationMs);
    const stride = Math.max(1, Math.ceil(state.waveformBeatGrid.length / 2048));
    for (let index = 0; index < state.waveformBeatGrid.length; index += stride) {
      const x = (state.waveformBeatGrid[index] / durationMs) * width;
      if (x >= 0 && x <= width) context.fillRect(x, height * 0.12, Math.max(1, dpr), height * 0.76);
    }
  }
}

function updatePlaybackProgress(status = state.audioStatus, elapsedOverride = null, updateLabels = true) {
  const track = state.audioTrack || state.selectedTrack;
  const durationMs = Number(track?.durationMs ?? track?.duration_ms ?? 0);
  const sampleRate = Number(status?.sampleRate || 0);
  const positionFrames = Number(status?.positionFrames || 0);
  const elapsedSeconds = elapsedOverride ?? (sampleRate > 0 ? positionFrames / sampleRate : 0);
  const ratio = durationMs > 0 ? Math.max(0, Math.min(1, elapsedSeconds * 1000 / durationMs)) : 0;
  if (updateLabels) {
    $('#track-time-current').textContent = formatTrackTime(elapsedSeconds);
    $('#track-time-total').textContent = formatTrackTime(durationMs / 1000);
  }
  $('#progress-only-fill').style.transform = `scaleX(${ratio})`;
  $('#waveform-playhead').style.transform = `translate3d(${ratio * 100}%, 0, 0)`;
}

function renderStemSwitches(limit = state.visibleStemLimit ?? 4) {
  const buttons = document.querySelectorAll('[data-stem-index]');
  for (const button of buttons) {
    const index = Number(button.dataset.stemIndex);
    const name = t(STEM_NAMES[index] || 'Stem');
    const enabled = Number(state.stemGains[index] || 0) > 0;
    const available = index < limit;
    const label = t('{stem} stem', { stem: name });
    button.classList.toggle('active', enabled);
    button.classList.toggle('unavailable', !available);
    button.setAttribute('aria-pressed', String(enabled));
    button.setAttribute('aria-label', t('Toggle {stem}', { stem: label }));
    button.title = !isTauri
      ? t('Requires the installed Tauri app')
      : available
        ? t('Toggle {stem}', { stem: label })
        : t('{stem} is available with Pro on Android', { stem: label });
    button.disabled = !isTauri || !available || state.stemBusy || state.audioBusy;
    const text = button.querySelector('.stem-switch-label');
    if (text) text.textContent = name;
  }
}

async function toggleStemSwitch(index) {
  if (!isTauri) return toast(t('Requires the installed Tauri app'), true);
  if (state.stemBusy || index < 0 || index >= (state.visibleStemLimit ?? 4)) return;
  const nextGains = [...state.stemGains];
  nextGains[index] = nextGains[index] > 0 ? 0 : 1;
  state.stemBusy = true;
  renderStemSwitches();
  try {
    const status = await invoke('set_audio_controls', { update: { stemGains: nextGains } });
    state.stemGains = nextGains;
    displayAudioStatus(status);
  } catch (error) {
    toast(t('Could not update the {stem} stem: {error}', {
      stem: t(STEM_NAMES[index]),
      error: String(error),
    }), true);
  } finally {
    state.stemBusy = false;
    renderStemSwitches();
  }
}

function animatePlaybackProgress() {
  state.progressRaf = 0;
  const status = state.audioStatus;
  if (!status?.playing || state.waveformMode === 'off') return;
  const sampleRate = Number(status.sampleRate || 0);
  const baseSeconds = sampleRate > 0 ? Number(status.positionFrames || 0) / sampleRate : 0;
  const elapsedSeconds = baseSeconds + ((performance.now() - state.progressSyncTime) / 1000) * Number(status.playbackRate || 1);
  updatePlaybackProgress(status, elapsedSeconds, false);
  state.progressRaf = requestAnimationFrame(animatePlaybackProgress);
}

function syncProgressAnimation() {
  const shouldAnimate = Boolean(state.audioStatus?.playing && state.waveformMode !== 'off');
  if (shouldAnimate && !state.progressRaf) {
    state.progressSyncTime = performance.now();
    state.progressRaf = requestAnimationFrame(animatePlaybackProgress);
  } else if (!shouldAnimate && state.progressRaf) {
    cancelAnimationFrame(state.progressRaf);
    state.progressRaf = 0;
  }
}

async function loadWaveformOnPlay(track) {
  if (!track || state.waveformMode !== 'canvas' || isAndroid) return;
  if (state.waveformTrackId === track.id && state.waveformPeaks) {
    drawWaveform();
    return;
  }
  const requestId = ++state.waveformRequestId;
  state.waveformTrackId = track.id;
  state.waveformPeaks = null;
  state.waveformBeatGrid = null;
  state.waveformBpm = null;
  const note = $('#waveform-mode-note');
  note.textContent = t('SQLite-Peaks werden nach Play lokal geladen …');
  try {
    const packet = await invoke('get_track_waveform_packet', { trackId: track.id, peakCount: 1024 });
    const decoded = decodeWaveformPacket(packet);
    if (requestId !== state.waveformRequestId || state.waveformMode !== 'canvas' || state.audioTrack?.id !== track.id) return;
    state.waveformPeaks = decoded.peaks;
    state.waveformBeatGrid = decoded.beats;
    state.waveformBpm = decoded.bpm;
    drawWaveform();
    note.textContent = t('{peaks} Peak-Buckets · {beats} Beat-Marker{bpm} · lokal', {
      peaks: decoded.peaks.length,
      beats: decoded.beats.length,
      bpm: decoded.bpm ? ` · ${decoded.bpm.toFixed(1)} BPM` : '',
    });
  } catch (error) {
    if (requestId === state.waveformRequestId) {
      note.textContent = t('Canvas-Analyse nicht verfügbar · Fortschritt bleibt aktiv ({error})', { error: String(error) });
    }
  }
}

function applyWaveformMode(mode) {
  const allowed = ['canvas', 'progress', 'off'];
  const selected = allowed.includes(mode) ? mode : 'progress';
  state.waveformMode = isAndroid && selected === 'canvas' ? 'progress' : selected;
  $('#waveform-mode').value = state.waveformMode;
  const visual = $('#track-visualization');
  visual.dataset.waveformMode = state.waveformMode;
  visual.hidden = state.waveformMode === 'off';
  $('.track-time-row').hidden = state.waveformMode === 'off';
  localStorage.setItem('championvibe.waveformMode', state.waveformMode);
  if (state.waveformMode === 'canvas' && state.waveformPeaks) drawWaveform();
  if (state.waveformMode !== 'canvas') {
    state.waveformRequestId += 1;
    if (state.waveformContext) {
      const canvas = $('#waveform-canvas');
      state.waveformContext.clearRect(0, 0, canvas.width, canvas.height);
    }
  }
  $('#waveform-mode-note').textContent = t(isAndroid
    ? 'Android: Canvas deaktiviert; nutze Fortschritt oder Waveform aus.'
    : 'SQLite-Peaks und Beat-Grid werden erst beim Drücken von Play geladen.');
  syncProgressAnimation();
}

function displayAudioStatus(status) {
  state.audioStatus = status;
  const reportedStems = Number(status?.activeStems);
  if (status?.loaded && [2, 4].includes(reportedStems) && reportedStems !== state.visibleStemLimit) {
    syncFreemiumUI();
  }
  if (status?.playing) state.progressSyncTime = performance.now();
  updatePlaybackProgress(status);
  updateAutoGainApplied();
  syncProgressAnimation();
  const indicator = $('#audio-engine-status');
  if (!status) {
    indicator.textContent = t(isTauri ? 'Native Ausgabe bereit' : 'Nur in der Tauri-App verfügbar');
    return;
  }
  const mode = status.failed ? 'FEHLER' : status.playing ? 'WIEDERGABE' : status.finished ? 'ENDE' : status.loaded ? 'PAUSIERT' : 'BEREIT';
  const rate = status.sampleRate ? ` · ${(status.sampleRate / 1000).toFixed(1)} kHz` : '';
  const tempo = status.playbackRate && Math.abs(status.playbackRate - 1) > 0.001
    ? ` · ${t('tempo')} ×${Number(status.playbackRate).toFixed(2)}`
    : '';
  const xruns = status.underruns ? ` · XRUN ${status.underruns}` : '';
  const reductionDb = Number(status.masterGainReductionDb || 0);
  const limiter = ` · GR ${reductionDb.toFixed(1)} dB`;
  const limiterLatency = Number(status.limiterLatencyMs || 0);
  const lookahead = limiterLatency > 0 ? ` · LA ${limiterLatency.toFixed(1)} ms` : '';
  indicator.textContent = status.lastError
    ? t(status.lastError)
    : `${t(mode)}${rate}${tempo}${limiter}${lookahead}${xruns}`;
  indicator.title = status.lastError
    ? t(status.lastError)
    : `${t('Backend {backend}; {frames} Frames', { backend: status.backend, frames: status.positionFrames || 0 })} · ${reductionDb.toFixed(2)} dB master gain reduction · ${limiterLatency.toFixed(2)} ms lookahead`;
}

async function refreshRecordingStatus() {
  if (!isTauri || !state.recordingActive || state.recordBusy) return;
  try {
    const snapshot = await invoke('get_master_recording_status');
    const wasActive = state.recordingActive;
    state.recordingSnapshot = snapshot;
    state.recordingActive = Boolean(snapshot.active);
    if (wasActive && !state.recordingActive) {
      state.recordAuthorized = !snapshot.error;
      syncFreemiumUI();
      toast(snapshot.message?.[getLocale()] || snapshot.message?.de || snapshot.error || 'Master-Aufnahme beendet.', Boolean(snapshot.error));
    }
  } catch (error) {
    console.warn('Master recording status unavailable:', error);
  }
}

async function refreshAudioStatus() {
  if (!isTauri) return displayAudioStatus(null);
  try {
    displayAudioStatus(await invoke('get_audio_status'));
  } catch (error) {
    $('#audio-engine-status').textContent = t('Audio-Status nicht verfügbar: {error}', { error: String(error) });
  }
}

function autoGainOffsetForTrack(track) {
  if (!state.autoGainEnabled) return 0;
  const rawLufs = track?.lufsIntegrated ?? track?.lufs_integrated;
  if (rawLufs === null || rawLufs === undefined || rawLufs === '') return null;
  const integratedLufs = Number(rawLufs);
  if (!Number.isFinite(integratedLufs) || integratedLufs < -70 || integratedLufs > 10) return null;
  return Math.max(-48, Math.min(24, state.autoGainTargetLufs - integratedLufs));
}

function updateAutoGainApplied(track = state.audioTrack) {
  const indicator = $('#auto-gain-applied');
  if (!indicator) return;
  if (!state.autoGainEnabled) {
    indicator.textContent = t('Auto-Gain Off');
    indicator.dataset.state = 'off';
    indicator.title = t('Auto-Gain is disabled for both decks.');
    return;
  }
  const offset = autoGainOffsetForTrack(track);
  if (offset === null) {
    indicator.textContent = t('Auto-Gain · LUFS unavailable');
    indicator.dataset.state = 'unavailable';
    indicator.title = t('No LUFS analysis is available; unity gain is used.');
    return;
  }
  const formatted = new Intl.NumberFormat(getLocale(), {
    minimumFractionDigits: 1,
    maximumFractionDigits: 1,
  }).format(Math.abs(offset));
  const signed = `${offset < 0 ? '−' : '+'}${formatted}`;
  indicator.textContent = t('Auto-Gain {gain} dB', { gain: signed });
  indicator.dataset.state = offset > 0.05 ? 'boost' : offset < -0.05 ? 'cut' : 'unity';
  indicator.title = t('Estimated from integrated LUFS and the selected target; the limiter and other DSP can alter final level.');
}

function updateCurrentTrackUI(track) {
  $('#audio-track-title').textContent = track?.title || t('Wähle einen Track aus deiner Bibliothek.');
  $('#audio-track-artist').textContent = track?.artist || track?.musicalKey || track?.musical_key || t('Lokale Sammlung · keine Cloud-Wiedergabe');
  const cover = $('#player-cover');
  cover.textContent = track?.musicalKey || track?.musical_key || '♫';
  cover.dataset.trackId = track?.id || '';
  const durationMs = Number(track?.durationMs ?? track?.duration_ms ?? 0);
  $('#track-time-total').textContent = formatTrackTime(durationMs / 1000);
  updateAutoGainApplied(track);
  updatePlaybackProgress(state.audioStatus);
}

async function loadAndPlayTrack(track, button = null) {
  if (!isTauri) return toast('Native Audio-Wiedergabe ist nur in der Tauri-App verfügbar.', true);
  if (state.audioBusy) return;
  state.audioBusy = true;
  renderStemSwitches();
  if (button && button.dataset.trackId === String(track.id)) {
    button.disabled = true;
    button.classList.add('loading');
  }
  state.audioTrack = track;
  void selectTrack(track);
  updateCurrentTrackUI(track);
  saveProSession();
  $('#audio-engine-status').textContent = t('Lokale Stems werden vorbereitet …');
  try {
    await invoke('load_track_audio', { trackId: track.id });
    await invoke('set_audio_controls', { update: { stemGains: state.stemGains } });
    displayAudioStatus(await invoke('play_audio'));
    void loadWaveformOnPlay(track);
    toast(t('Native Wiedergabe gestartet: {title}', { title: track.title }));
  } catch (error) {
    $('#audio-engine-status').textContent = t('Audio-Fehler');
    toast(String(error), true);
  } finally {
    state.audioBusy = false;
    renderStemSwitches();
    if (button && button.dataset.trackId === String(track.id)) {
      button.disabled = false;
      button.classList.remove('loading');
    }
  }
}

async function playAudio() {
  if (!isTauri) return toast('Native Audio-Wiedergabe ist nur in der Tauri-App verfügbar.', true);
  if (state.audioBusy) return;
  const track = state.audioTrack || state.selectedTrack;
  if (!track) return toast('Wähle zuerst einen lokalen Track aus.', true);
  state.audioTrack = track;
  state.audioBusy = true;
  renderStemSwitches();
  try {
    let status = await invoke('get_audio_status');
    if (!status.loaded) {
      await invoke('load_track_audio', { trackId: track.id });
    }
    await invoke('set_audio_controls', { update: { stemGains: state.stemGains } });
    status = await invoke('play_audio');
    displayAudioStatus(status);
    void loadWaveformOnPlay(track);
    saveProSession();
  } catch (error) {
    toast(String(error), true);
  } finally {
    state.audioBusy = false;
    renderStemSwitches();
  }
}

const mixPresetNames = {
  smoothSelector: 'P1 · Smooth Selector',
  clubHype: 'P2 · Club / Hype',
  aggressiveClash: 'P3 · Aggressiv / Clash',
  freestyleMaestro: 'P4 · Freestyle Maestro',
};
const mixStyleNames = {
  deepRoots: 'Deep / Roots',
  clubHype: 'Club / Hype',
  aggressiveClash: 'Aggressiv / Clash',
};
const mixIntensityNames = {
  1: 'Deep / Roots',
  2: 'Club / Hype',
  3: 'Aggressiv / Clash',
  4: 'Clash Max',
};
const mixPhaseNames = {
  stopped: 'GESTOPPT',
  loading: 'LÄDT STEMS',
  playing: 'AUTOPILOT',
  paused: 'PAUSIERT',
  waitingForRewind: 'WARTET AUF AUTO-REWIND',
  transitioning: 'ÜBERGANG',
  error: 'MIX-FEHLER',
};
const transitionNames = {
  longCrossfade: '8-Takt-Crossfade + Bass-Swap + Dub',
  hardChorusCut: 'Chorus-Hard-Cut',
  semanticRewindCut: 'Whisper-Wordplay + Auto-Rewind',
};
const tempoModeNames = { halfTime: 'Half-Time', normal: 'Normal', doubleTime: 'Double-Time' };

function setViewMode(mode) {
  const shell = $('#app-shell');
  const performance = mode === 'performance';
  shell.dataset.viewMode = performance ? 'performance' : 'player';
  const toggle = $('#toggle-performance');
  toggle.setAttribute('aria-pressed', String(performance));
  toggle.textContent = t(performance ? 'Zurück zum Player' : 'Performance Mode');
  if (performance) setToolsOpen(false);
  saveProSession();
}

function setToolsOpen(open) {
  const shell = $('#app-shell');
  const next = Boolean(open) && shell.dataset.viewMode !== 'performance';
  shell.dataset.toolsOpen = String(next);
  const button = $('#toggle-tools');
  button.setAttribute('aria-expanded', String(next));
  const label = t(next ? 'Tools schließen' : 'Library-Tools');
  button.textContent = label;
  button.setAttribute('aria-label', label);
  button.title = label;
}

function displayMixStatus(status) {
  state.mixStatus = status;
  const element = $('#mix-engine-status');
  if (!element || !status) return;
  const active = Boolean(status.active);
  element.classList.toggle('active', active);
  element.classList.toggle('error', status.phase === 'error');
  const phase = t(mixPhaseNames[status.phase] || String(status.phase || 'BEREIT').toUpperCase());
  if (status.error) {
    element.textContent = t('{phase} · {error}', { phase, error: status.error });
    element.title = t(status.decision?.rationale || '');
  } else if (status.decision) {
    const decision = status.decision;
    const rate = Number(decision.playbackRate || 1).toFixed(2);
    const tempo = `${t(tempoModeNames[decision.tempoMode] || decision.tempoMode)} ×${rate}`;
    const progress = status.phase === 'transitioning'
      ? t(' · {percent}%', { percent: Math.round((status.transitionProgress || 0) * 100) })
      : '';
    element.textContent = t('{phase} · {preset} → {style} · {current} → {next} · {tempo} · {transition}{progress}', {
      phase,
      preset: t(mixPresetNames[status.preset] || status.preset),
      style: t(mixStyleNames[status.style] || status.style),
      current: status.currentTitle || t('Track'),
      next: status.nextTitle || t('Next'),
      tempo,
      transition: t(transitionNames[decision.transition] || decision.transition),
      progress,
    });
    element.title = t(decision.rationale || '');
  } else if (status.phase === 'loading') {
    element.textContent = t('{phase} · Track #{id} wird lokal vorbereitet', { phase, id: status.currentTrackId || '…' });
    element.title = '';
  } else {
    element.textContent = t('Wähle einen Library-Track mit lokal verfügbaren Stems, dann Run Mix.');
    element.title = '';
  }
  const start = $('#mix-start');
  const stop = $('#mix-stop');
  const preset = $('#mix-preset');
  const starting = status.phase === 'loading';
  const transitioning = status.phase === 'transitioning';
  if (status.intensityOverride !== undefined && !state.intensityBusy) {
    state.intensityOverride = status.intensityOverride !== null;
    if (status.intensityOverride !== null && !state.intensityEditing) {
      state.mixIntensity = Number(status.intensityOverride);
    }
  }
  if (start) start.disabled = !isTauri || state.mixBusy || active || starting || !state.selectedTrack;
  if (stop) stop.disabled = !isTauri || (!active && status.phase !== 'loading' && status.phase !== 'transitioning');
  if (preset && status.preset && preset.value !== status.preset) preset.value = status.preset;
  const overrideToggle = $('#intensity-override-toggle');
  const intensitySlider = $('#intensity-slider');
  const intensityReadout = $('#intensity-readout');
  const intensityStatus = $('#intensity-status');
  if (overrideToggle) overrideToggle.checked = state.intensityOverride;
  if (intensitySlider) {
    if (!state.intensityEditing) intensitySlider.value = String(state.mixIntensity);
    intensitySlider.disabled = !isTauri || !state.intensityOverride || state.intensityBusy;
  }
  if (overrideToggle) overrideToggle.disabled = !isTauri || state.intensityBusy;
  if (intensityReadout) intensityReadout.textContent = `${state.mixIntensity} · ${t(mixIntensityNames[state.mixIntensity])}`;
  if (intensityStatus) {
    intensityStatus.textContent = state.intensityOverride
      ? t('Manueller Override {level} aktiv · {action}.', {
        level: state.mixIntensity,
        action: t(active ? 'ändert den nächsten Übergang' : 'wird beim nächsten Mix angewendet'),
      })
      : t('KI entscheidet automatisch. Override bleibt aus.');
  }
  const forceNext = $('#force-next-track');
  const pullUp = $('#pull-up-rewind');
  const dropSample = $('#drop-sample');
  const nextTrack = $('#audio-next');
  if (forceNext) forceNext.disabled = !isTauri || !active || starting || transitioning || state.performanceBusy;
  if (pullUp) pullUp.disabled = !isTauri || (!active && !state.audioStatus?.loaded) || state.performanceBusy;
  if (dropSample) dropSample.disabled = !isTauri || !state.sampleCatalog.length || state.performanceBusy;
  if (nextTrack) nextTrack.disabled = !isTauri || !state.trackTotal || active;
  for (const control of document.querySelectorAll('#audio-play, #audio-pause, #audio-stop')) {
    control.disabled = !isTauri || active;
  }
  syncDubplateAvailability();
}

async function refreshMixStatus() {
  if (!isTauri) return;
  try {
    displayMixStatus(await invoke('get_mix_engine_status'));
  } catch (error) {
    const element = $('#mix-engine-status');
    if (element) element.textContent = t('Mix-Status nicht verfügbar: {error}', { error: String(error) });
  }
}

async function initializeMixIntensity() {
  if (!isTauri) return;
  try {
    const intensity = state.intensityOverride ? state.mixIntensity : null;
    displayMixStatus(await invoke('set_mix_engine_intensity', { intensity }));
  } catch (error) {
    toast(t('Gespeicherter Intensitäts-Override konnte nicht initialisiert werden: {error}', { error: String(error) }), true);
  }
}

async function startMix() {
  if (!isTauri) return toast('Die automatische Mix-Engine ist nur in der Tauri-App verfügbar.', true);
  if (state.mixBusy) return;
  const track = state.selectedTrack || state.audioTrack;
  if (!track) return toast('Wähle zuerst einen Library-Track mit lokalen Stems aus.', true);
  state.mixBusy = true;
  displayMixStatus({ phase: 'loading', currentTrackId: track.id, preset: $('#mix-preset').value });
  try {
    const status = await invoke('start_mix_engine', { trackId: track.id, preset: $('#mix-preset').value });
    displayMixStatus(status);
    toast(t('Mix-Engine gestartet: {preset}', { preset: t(mixPresetNames[$('#mix-preset').value]) }));
  } catch (error) {
    toast(String(error), true);
  } finally {
    state.mixBusy = false;
    refreshMixStatus();
  }
}

async function stopMix() {
  if (!isTauri || state.mixBusy) return;
  state.mixBusy = true;
  try {
    displayMixStatus(await invoke('stop_mix_engine'));
    toast('Automatische Mix-Engine gestoppt.');
  } catch (error) {
    toast(String(error), true);
  } finally {
    state.mixBusy = false;
  }
}

async function changeMixPreset() {
  saveProSession();
  if (!isTauri || (!state.mixStatus?.active && state.mixStatus?.phase !== 'loading')) return;
  try {
    displayMixStatus(await invoke('set_mix_engine_preset', { preset: $('#mix-preset').value }));
  } catch (error) {
    toast(String(error), true);
  }
}

function updateIntensityReadout() {
  const value = Number($('#intensity-slider').value);
  state.intensityEditing = true;
  state.mixIntensity = value;
  $('#intensity-readout').textContent = `${value} · ${t(mixIntensityNames[value])}`;
}

async function applyIntensityOverride() {
  if (!isTauri || state.intensityBusy) return;
  state.intensityBusy = true;
  const toggle = $('#intensity-override-toggle');
  const previous = state.intensityOverride;
  const enabled = toggle.checked;
  state.intensityOverride = enabled;
  state.mixIntensity = Number($('#intensity-slider').value);
  try {
    const status = await invoke('set_mix_engine_intensity', {
      intensity: enabled ? state.mixIntensity : null,
    });
    localStorage.setItem('championvibe.intensityOverride', String(enabled));
    localStorage.setItem('championvibe.mixIntensity', String(state.mixIntensity));
    displayMixStatus(status);
    saveProSession();
    toast(enabled
      ? t('KI-Override {level} · {style} wird angewendet.', { level: state.mixIntensity, style: t(mixIntensityNames[state.mixIntensity]) })
      : 'Automatische Maestro-Entscheidung wieder aktiv.');
  } catch (error) {
    state.intensityOverride = previous;
    toggle.checked = previous;
    toast(t('Intensitäts-Override fehlgeschlagen: {error}', { error: String(error) }), true);
  } finally {
    state.intensityBusy = false;
    if (state.mixStatus) displayMixStatus(state.mixStatus);
  }
}

async function forceNextTrack() {
  if (!isTauri || !state.mixStatus?.active || state.performanceBusy) return;
  state.performanceBusy = true;
  try {
    displayMixStatus(await invoke('force_next_track'));
    toast('Force Next gesendet · der vorbereitete lokale Track wird übernommen.');
  } catch (error) {
    toast(t('Force Next fehlgeschlagen: {error}', { error: String(error) }), true);
  } finally {
    state.performanceBusy = false;
    refreshMixStatus();
  }
}

async function pullUpRewind() {
  if (!isTauri || (!state.mixStatus?.active && !state.audioStatus?.loaded) || state.performanceBusy) return;
  state.performanceBusy = true;
  try {
    displayAudioStatus(await invoke('wheel_and_come_again', { sampleId: null }));
    toast('Pull Up · lokaler Vinyl-Rewind und Neustart bei Beat eins.');
  } catch (error) {
    toast(t('Rewind fehlgeschlagen: {error}', { error: String(error) }), true);
  } finally {
    state.performanceBusy = false;
  }
}

async function dropSelectedSample() {
  const sample = state.sampleCatalog.find((item) => item.id === Number($('#performance-sample').value));
  if (!sample) return toast('Wähle zuerst einen geladenen lokalen Sample.', true);
  await fireSample(sample, $('#performance-quantization').value);
}

async function pauseAudio() {
  if (!isTauri) return;
  try {
    displayAudioStatus(await invoke('pause_audio'));
  } catch (error) {
    toast(String(error), true);
  }
}

async function stopAudio() {
  if (!isTauri) return;
  try {
    displayAudioStatus(await invoke('stop_audio'));
  } catch (error) {
    toast(String(error), true);
  }
}

function sampleDisplayName(sample) {
  const builtinLabels = {
    siren: 'Siren',
    airhorn: 'Airhorn',
    'vinyl-rewind': 'Vinyl-Rewind',
  };
  const translationKey = sample.builtin ? builtinLabels[sample.name.toLowerCase()] : null;
  return translationKey ? t(translationKey) : sample.name;
}

function renderSampleCatalog() {
  const pads = $('#sample-pads');
  const rewindSelect = $('#rewind-sample');
  const performanceSelect = $('#performance-sample');
  pads.replaceChildren();
  rewindSelect.replaceChildren();
  performanceSelect.replaceChildren();
  for (const sample of state.sampleCatalog) {
    const pad = document.createElement('button');
    pad.type = 'button';
    pad.className = `sample-pad${sample.builtin ? ' builtin' : ''}`;
    const displayName = sampleDisplayName(sample);
    pad.textContent = displayName;
    pad.title = t('{sample} · {duration} s · RAM', { sample: displayName, duration: (sample.durationMs / 1000).toFixed(1) });
    pad.disabled = !isTauri;
    pad.addEventListener('click', () => fireSample(sample));
    pads.append(pad);

    const option = document.createElement('option');
    option.value = String(sample.id);
    option.textContent = displayName;
    rewindSelect.append(option);
    const performanceOption = option.cloneNode(true);
    performanceSelect.append(performanceOption);
  }
  const preferred = state.sampleCatalog.find((sample) => sample.name.toLowerCase() === 'airhorn')
    || state.sampleCatalog.find((sample) => sample.name.toLowerCase() !== 'vinyl-rewind');
  if (preferred) {
    rewindSelect.value = String(preferred.id);
    performanceSelect.value = String(preferred.id);
  }
  if (state.mixStatus) displayMixStatus(state.mixStatus);
}

async function refreshSampleCatalog() {
  if (!isTauri) return;
  try {
    state.sampleCatalog = await invoke('get_sample_catalog');
    state.sampleFolderError = '';
    renderSampleCatalog();
  } catch (error) {
    state.sampleFolderError = String(error);
    $('#sample-folder-path').textContent = t('Sample-Katalog nicht verfügbar: {error}', { error: state.sampleFolderError });
  }
}

async function fireSample(sample, quantization = $('#sample-quantization').value) {
  if (!isTauri) return toast('Sample-Pads sind nur in der nativen App verfügbar.', true);
  try {
    const status = await invoke('trigger_sample', {
      sampleId: sample.id,
      quantization,
    });
    displayAudioStatus(status);
  } catch (error) {
    toast(t('Sample konnte nicht geplant werden: {error}', { error: String(error) }), true);
  }
}

async function loadSampleFolder() {
  if (!isTauri) return toast('WAV-Samples werden in der installierten App lokal geladen.', true);
  try {
    let path;
    if (isAndroid) {
      path = await invoke('pick_android_sample_folder');
      if (!path) return;
    } else {
      const selection = await open({ directory: true, multiple: false, title: t('Lokalen WAV-Sample-Ordner wählen') });
      path = Array.isArray(selection) ? selection[0] : selection;
      if (!path) return;
    }
    const samples = await invoke('load_sample_folder', { path });
    state.sampleFolder = path;
    state.sampleFolderError = '';
    state.sampleCatalog = samples;
    renderSampleCatalog();
    $('#sample-folder-path').textContent = t('{count} Samples im RAM · {path}', { count: samples.length, path });
    toast(t('{count} Samples lokal in den RAM geladen.', { count: samples.length }));
  } catch (error) {
    toast(t('Sample-Ordner konnte nicht geladen werden: {error}', { error: String(error) }), true);
  }
}

async function wheelAndComeAgain() {
  if (!isTauri) return toast('Wheel-and-Come-Again benötigt die native Audio-Engine.', true);
  if (!state.mixStatus?.active && !state.audioStatus?.loaded && !state.audioTrack) return toast('Wähle zuerst einen Track mit lokalen Stems aus.', true);
  const button = $('#wheel-and-come-again');
  button.disabled = true;
  try {
    const value = $('#rewind-sample').value;
    const status = await invoke('wheel_and_come_again', {
      sampleId: value ? Number(value) : null,
    });
    displayAudioStatus(status);
    toast('Wheel: Wiedergabestream neu geöffnet · Vinyl-Rewind und Sample laufen vor Beat eins.');
  } catch (error) {
    toast(t('Wheel fehlgeschlagen: {error}', { error: String(error) }), true);
  } finally {
    button.disabled = false;
  }
}

async function setAutoRewind(enabled) {
  if (!isTauri) return;
  try {
    displayAudioStatus(await invoke('set_auto_rewind', { enabled }));
    localStorage.setItem('championvibe.autoRewind', String(enabled));
  } catch (error) {
    toast(t('Auto-Wheel konnte nicht gesetzt werden: {error}', { error: String(error) }), true);
  }
}

async function refreshTtsStatus() {
  if (!isTauri) return;
  try {
    state.ttsAvailable = await invoke('get_tts_engine_status');
  } catch (error) {
    state.ttsAvailable = false;
    $('#tts-state').title = t(String(error));
  }
  $('#tts-state').textContent = t(state.kokoroWasmReady
    ? 'KOKORO · WASM'
    : state.ttsAvailable ? 'PIPER · CPU' : 'WASM · NICHT GELADEN');
  if (state.kokoroWasmReady) {
    $('#tts-state').title = t('Kokoro läuft lokal mit ONNX Runtime WebAssembly. Die Stimme spricht US-Englisch, keine authentische Patois-Stimme.');
  } else if (state.ttsAvailable) {
    $('#tts-state').title = t('Lokale Piper-Inferenz ist in diesem Build aktiviert.');
  } else if (!$('#tts-state').title) {
    $('#tts-state').title = t('Lade die geprüften Kokoro-Gewichte und die Stimme aus der lokalen Modellbibliothek.');
  }
  syncDubplateAvailability();
  if (state.mixStatus) displayMixStatus(state.mixStatus);
}

async function loadKokoroWasm() {
  if (!isTauri || state.kokoroWasmBusy || state.kokoroWasmReady) return;
  state.kokoroWasmBusy = true;
  const button = $('#load-kokoro-wasm');
  if (button) button.textContent = t('Kokoro WASM wird geladen …');
  syncDubplateAvailability();
  try {
    const service = createKokoroWasmService({ invoke });
    await service.load();
    state.kokoroWasmService = service;
    state.kokoroWasmReady = true;
    $('#tts-state').textContent = t('KOKORO · WASM');
    $('#tts-state').title = t('Kokoro läuft lokal mit ONNX Runtime WebAssembly. Die Stimme spricht US-Englisch, keine authentische Patois-Stimme.');
    toast(t('Kokoro WASM ist geladen. Patois-Phrase-Templates werden mit der US-englischen Stimme gesprochen.'));
  } catch (error) {
    state.kokoroWasmService?.dispose?.();
    state.kokoroWasmService = null;
    state.kokoroWasmReady = false;
    toast(t('Kokoro WASM konnte nicht geladen werden: {error}', { error: String(error) }), true);
  } finally {
    state.kokoroWasmBusy = false;
    if (button) button.textContent = t(state.kokoroWasmReady ? 'Kokoro WASM geladen' : 'Kokoro WASM laden');
    syncDubplateAvailability();
  }
}

async function generateDubplate() {
  if (!isTauri) return toast('Dubplate-TTS ist nur in der installierten App verfügbar.', true);
  const useKokoroWasm = state.kokoroWasmReady && Boolean(state.kokoroWasmService);
  if (!useKokoroWasm && !state.ttsAvailable) {
    return toast('Lade zuerst die geprüften Kokoro-Gewichte und die Stimme aus der lokalen Modellbibliothek.', true);
  }
  if (state.performanceBusy) return;
  if (!state.selectedTrack) return toast('Wähle zuerst einen Track aus.', true);
  if (!useKokoroWasm && !trackMusicalKey(state.selectedTrack)) return toast('Für diesen Track wurde kein musikalischer Key analysiert.', true);
  if (!useKokoroWasm && (!state.piperModelPath || !state.piperConfigPath)) return toast('Wähle ein lokales Piper-ONNX-Modell und dessen Voice-JSON aus.', true);
  state.performanceBusy = true;
  syncDubplateAvailability();
  const button = $('#generate-dubplate');
  setBusy(button, true, 'CPU rendert lokal …');
  try {
    if (useKokoroWasm) {
      const audio = await state.kokoroWasmService.synthesize({
        text: $('#dubplate-text').value.trim(),
        preset: $('#dubplate-preset').value,
        djName: $('#dubplate-dj-name').value.trim(),
        emotion: trackDubplateEmotion(state.selectedTrack),
      });
      const playback = await pushWasmTtsAudioBuffer(audio, {
        invoke,
        android: isAndroid,
        quantization: 'bar',
      });
      toast(t('Lokaler WASM-Shoutout für den Master-Ausgang am nächsten Takt vorgemerkt · {duration} s · US-englische Stimme, kein authentisches Patois.', {
        duration: playback.durationSeconds.toFixed(1),
      }));
      return;
    }
    const result = await invoke('generate_dubplate', {
      input: {
        modelPath: state.piperModelPath,
        configPath: state.piperConfigPath,
        text: $('#dubplate-text').value.trim(),
        preset: $('#dubplate-preset').value,
        djName: $('#dubplate-dj-name').value.trim() || null,
        musicalKey: trackMusicalKey(state.selectedTrack),
        emotion: trackDubplateEmotion(state.selectedTrack),
      },
    });
    state.sampleCatalog = await invoke('get_sample_catalog');
    renderSampleCatalog();
    await invoke('trigger_sample', {
      sampleId: result.sample.id,
      quantization: $('#sample-quantization').value,
    });
    toast(t('Dubplate auf {key} abgestimmt · Transposition {semitones} Halbtöne · am Grid geplant.', {
      key: trackMusicalKey(state.selectedTrack),
      semitones: result.transpositionSemitones,
    }));
  } catch (error) {
    toast(t('Dubplate konnte nicht gerendert werden: {error}', { error: String(error) }), true);
  } finally {
    setBusy(button, false);
    state.performanceBusy = false;
    syncDubplateAvailability();
    if (state.mixStatus) displayMixStatus(state.mixStatus);
  }
}

async function applyAudioControls(update) {
  if (!isTauri) return toast('Native DSP-Steuerung ist nur in der Tauri-App verfügbar.', true);
  try {
    displayAudioStatus(await invoke('set_audio_controls', { update }));
  } catch (error) {
    toast(t('DSP-Steuerung fehlgeschlagen: {error}', { error: String(error) }), true);
  }
}

function updateAutoGainControls() {
  const enabled = $('#auto-gain-enabled');
  const target = $('#auto-gain-target');
  const readout = $('#auto-gain-target-value');
  if (!enabled || !target || !readout) return;
  enabled.checked = state.autoGainEnabled;
  target.value = state.autoGainEnabled ? String(state.autoGainTargetLufs) : 'off';
  readout.textContent = state.autoGainEnabled
    ? t('{target} LUFS', { target: state.autoGainTargetLufs.toFixed(0) })
    : t('Off');
  updateAutoGainApplied();
}

async function applyAutoGainSettings(enabled, target = state.autoGainTargetLufs, quiet = false) {
  const targetLufs = Number(target);
  if (![-14, -16].includes(targetLufs)) return false;
  state.autoGainEnabled = Boolean(enabled);
  state.autoGainTargetLufs = targetLufs;
  localStorage.setItem('championvibe.autoGainEnabled', String(state.autoGainEnabled));
  localStorage.setItem('championvibe.autoGainTargetLufs', String(targetLufs));
  updateAutoGainControls();
  if (!isTauri) return true;
  const update = { autoGainEnabled: state.autoGainEnabled, autoGainTargetLufs: targetLufs };
  try {
    await Promise.all([
      invoke('set_audio_controls', { update }),
      ...[0, 1].map((deck) => invoke('set_deck_audio_controls', { deck, update })),
    ]);
    return true;
  } catch (error) {
    console.warn('Could not update per-deck EBU R128 gain controls:', error);
    if (!quiet) toast(t('DSP-Steuerung fehlgeschlagen: {error}', { error: String(error) }), true);
    return false;
  }
}

async function applyPitchLock(enabled, quiet = false) {
  state.pitchLockEnabled = Boolean(enabled);
  localStorage.setItem('championvibe.pitchLockEnabled', String(state.pitchLockEnabled));
  $('#pitch-lock-enabled').checked = state.pitchLockEnabled;
  $('#pitch-lock-mode').textContent = t(state.pitchLockEnabled ? 'Pitch-locked WSOLA' : 'Varispeed · pitch follows tempo');
  if (!isTauri) return true;
  const update = { pitchLockEnabled: state.pitchLockEnabled };
  try {
    await Promise.all([
      invoke('set_audio_controls', { update }),
      ...[0, 1].map((deck) => invoke('set_deck_audio_controls', { deck, update })),
    ]);
    return true;
  } catch (error) {
    console.warn('Could not update per-deck Pitch-Lock mode:', error);
    if (!quiet) toast(t('DSP-Steuerung fehlgeschlagen: {error}', { error: String(error) }), true);
    return false;
  }
}

function renderSelectedTrackContext(track = state.selectedTrack) {
  if (!track) return;
  $('#riddim-selected-track').textContent = `${track.title}${track.artist ? ` · ${track.artist}` : ''}`;
  const musicalKey = track.musicalKey || track.musical_key || t('Key nicht analysiert');
  $('#dubplate-key').textContent = t('Key: {key}', { key: musicalKey });
  if (state.selectedRiddimTagError) {
    $('#riddim-match').textContent = t('Tags nicht verfügbar: {error}', { error: state.selectedRiddimTagError });
  } else if (state.selectedRiddimFamily) {
    const family = state.selectedRiddimFamily;
    $('#riddim-match').textContent = t('{name} · {count} Tracks · {method} ({score}%)', {
      name: family.name,
      count: family.trackCount,
      method: t(family.matchMethod),
      score: Math.round(family.matchScore * 100),
    });
  } else {
    $('#riddim-match').textContent = t('Noch keiner Riddim-Familie zugeordnet');
  }
}

async function selectTrack(track) {
  if (state.selectedTrack?.id !== track.id) clearWaveform();
  state.selectedTrack = track;
  state.selectedRiddimFamily = null;
  state.selectedRiddimTagError = '';
  updateCurrentTrackUI(track);
  renderSelectedTrackContext(track);
  syncDubplateAvailability();
  saveProSession();
  if (state.mixStatus) displayMixStatus(state.mixStatus);
  $('#manual-riddim-name').value = '';
  $('#manual-riddim-tags').value = '';
  if (!isTauri) return;
  try {
    const [tags, family] = await Promise.all([
      invoke('get_track_manual_tags', { trackId: track.id }),
      invoke('get_track_riddim_family', { trackId: track.id }),
    ]);
    if (state.selectedTrack?.id !== track.id) return;
    $('#manual-riddim-name').value = tags.find((tag) => tag.namespace === 'riddim')?.tag || '';
    $('#manual-riddim-tags').value = tags
      .filter((tag) => tag.namespace !== 'riddim')
      .map((tag) => `${tag.namespace}:${tag.tag}`)
      .join(', ');
    state.selectedRiddimFamily = family;
    state.selectedRiddimTagError = '';
    renderSelectedTrackContext(track);
  } catch (error) {
    state.selectedRiddimTagError = String(error);
    renderSelectedTrackContext(track);
  }
}

function parseManualTagInput(value) {
  return value.split(',').map((token) => token.trim()).filter(Boolean).map((token) => {
    const separator = token.indexOf(':');
    if (separator > 0) {
      const namespace = token.slice(0, separator).trim().toLowerCase();
      const tag = token.slice(separator + 1).trim();
      if (['producer', 'genre', 'label', 'custom'].includes(namespace) && tag) return { namespace, tag };
    }
    return { namespace: 'custom', tag: token };
  });
}

async function saveRiddimTags() {
  if (!isTauri) return toast('Manuelle Riddim-Tags benötigen das lokale Rust-Backend.', true);
  if (!state.selectedTrack) return toast('Wähle zuerst einen Track aus der Bibliothek.', true);
  const tags = parseManualTagInput($('#manual-riddim-tags').value);
  const riddimName = $('#manual-riddim-name').value.trim();
  if (riddimName) tags.unshift({ namespace: 'riddim', tag: riddimName });
  const button = $('#save-riddim-tags');
  setBusy(button, true, 'Familien werden neu berechnet …');
  try {
    const report = await invoke('set_track_manual_tags', {
      trackId: state.selectedTrack.id,
      tags,
    });
    await Promise.all([refreshRiddimFamilies(), selectTrack(state.selectedTrack)]);
    toast(t('Tags gespeichert · {count} Riddim-Familien in SQLite.', { count: report.families }));
  } catch (error) {
    toast(t('Manuelle Tags konnten nicht gespeichert werden: {error}', { error: String(error) }), true);
  } finally {
    setBusy(button, false);
  }
}

function renderRiddimFamilies() {
  const container = $('#riddim-families');
  container.replaceChildren();
  if (state.riddimFamilyError) {
    container.textContent = t('Familien konnten nicht geladen werden: {error}', { error: state.riddimFamilyError });
    return;
  }
  if (!state.riddimFamilies.length) {
    const empty = document.createElement('div');
    empty.className = 'riddim-empty';
    empty.textContent = t('Noch keine Familien. Analysiere die Bibliothek oder vergebe denselben Riddim-Namen an mehrere Tracks.');
    container.append(empty);
    return;
  }
  for (const family of state.riddimFamilies) {
    const row = document.createElement('div');
    row.className = 'riddim-family';
    const name = document.createElement('strong');
    name.textContent = family.name;
    const count = document.createElement('span');
    count.textContent = t('{count} Tracks', { count: family.trackCount });
    row.append(name, count);
    container.append(row);
  }
}

async function refreshRiddimFamilies() {
  if (!isTauri) return;
  try {
    state.riddimFamilies = await invoke('get_riddim_families', { limit: 40 });
    state.riddimFamilyError = '';
  } catch (error) {
    state.riddimFamilyError = String(error);
  }
  renderRiddimFamilies();
}

async function rebuildRiddims() {
  if (!isTauri || state.riddimBusy) return;
  state.riddimBusy = true;
  const button = $('#rebuild-riddims');
  setBusy(button, true, 'Fingerprints …');
  try {
    const report = await invoke('rebuild_riddim_families');
    await Promise.all([refreshRiddimFamilies(), state.selectedTrack ? selectTrack(state.selectedTrack) : Promise.resolve()]);
    toast(t('Riddim-Scan: {instrumentals} Instrumentals geprüft · {tracks} Tracks in {families} Familien.', {
      instrumentals: report.indexedInstrumentals,
      tracks: report.groupedTracks,
      families: report.families,
    }));
  } catch (error) {
    toast(t('Riddim-Scan fehlgeschlagen: {error}', { error: String(error) }), true);
  } finally {
    state.riddimBusy = false;
    setBusy(button, false);
  }
}

function createVirtualTrackRow() {
  const row = document.createElement('div');
  row.className = 'track-row virtual-track-row';
  row.setAttribute('role', 'listitem');
  row.style.position = 'absolute';
  row.style.left = '0';
  row.style.right = '0';
  const cover = document.createElement('div');
  cover.className = 'track-cover';
  cover.setAttribute('aria-hidden', 'true');
  const select = document.createElement('button');
  select.type = 'button';
  select.className = 'track-copy track-select';
  const title = document.createElement('strong');
  const artist = document.createElement('span');
  artist.className = 'track-artist';
  const badges = document.createElement('span');
  badges.className = 'track-analysis-badges';
  const mood = document.createElement('span');
  mood.className = 'track-mood-badge';
  const rhythm = document.createElement('span');
  rhythm.className = 'track-rhythm-badge';
  badges.append(mood, rhythm);
  select.append(title, artist, badges);
  const bpm = document.createElement('span');
  bpm.className = 'track-bpm';
  const format = document.createElement('span');
  format.className = 'track-format-badge';
  const play = document.createElement('button');
  play.type = 'button';
  play.className = 'track-play';
  play.textContent = '▶';
  row.append(cover, select, format, bpm, play);
  return row;
}

function virtualTrackAt(index) {
  const page = state.tracks.get(Math.floor(index / PAGE_SIZE));
  return page?.[index % PAGE_SIZE] || null;
}

function pruneTrackPages(startIndex, endIndex) {
  if (!state.trackTotal) return;
  const keepFrom = Math.max(0, Math.floor(Math.max(0, startIndex) / PAGE_SIZE) - 1);
  const keepTo = Math.min(
    Math.ceil(state.trackTotal / PAGE_SIZE) - 1,
    Math.floor(Math.max(startIndex, endIndex) / PAGE_SIZE) + 1,
  );
  for (const pageIndex of state.tracks.keys()) {
    if (pageIndex < keepFrom || pageIndex > keepTo) state.tracks.delete(pageIndex);
  }
}

function renderVirtualTracks() {
  const list = $('#track-list');
  const plane = state.trackPlane;
  if (!plane) return;
  const viewportHeight = Math.max(list.clientHeight, TRACK_ROW_HEIGHT * 6);
  const visibleCount = Math.ceil(viewportHeight / TRACK_ROW_HEIGHT);
  const poolCount = Math.max(1, Math.min(15, visibleCount + TRACK_OVERSCAN * 2));
  while (state.trackRowPool.length < poolCount) {
    const row = createVirtualTrackRow();
    state.trackRowPool.push(row);
    plane.append(row);
  }
  while (state.trackRowPool.length > poolCount) state.trackRowPool.pop().remove();

  const firstVisible = Math.floor(list.scrollTop / TRACK_ROW_HEIGHT);
  const start = Math.max(0, firstVisible - TRACK_OVERSCAN);
  for (let slot = 0; slot < state.trackRowPool.length; slot += 1) {
    const row = state.trackRowPool[slot];
    const index = start + slot;
    const track = index < state.trackTotal ? virtualTrackAt(index) : null;
    row.style.height = `${TRACK_ROW_HEIGHT}px`;
    row.style.transform = `translate3d(0, ${index * TRACK_ROW_HEIGHT}px, 0)`;
    row.dataset.index = String(index);
    row.hidden = index >= state.trackTotal;
    const cover = row.querySelector('.track-cover');
    const select = row.querySelector('.track-select');
    const title = select.querySelector('strong');
    const artist = select.querySelector('.track-artist');
    const moodBadge = select.querySelector('.track-mood-badge');
    const rhythmBadge = select.querySelector('.track-rhythm-badge');
    const bpm = row.querySelector('.track-bpm');
    const format = row.querySelector('.track-format-badge');
    const play = row.querySelector('.track-play');
    row.classList.toggle('selected', Boolean(track && state.selectedTrack?.id === track.id));
    if (!track) {
      row.dataset.trackId = '';
      cover.textContent = '·';
      title.textContent = t('Tracks werden geladen …');
      artist.textContent = '';
      moodBadge.textContent = '';
      moodBadge.hidden = true;
      rhythmBadge.textContent = '';
      rhythmBadge.hidden = true;
      format.textContent = '';
      format.title = '';
      bpm.textContent = '';
      select.disabled = true;
      play.disabled = true;
      continue;
    }
    row.dataset.trackId = String(track.id);
    cover.textContent = track.musicalKey || '♫';
    select.dataset.trackId = String(track.id);
    select.setAttribute('aria-label', t('Track auswählen: {title}', { title: track.title }));
    title.textContent = track.title;
    artist.textContent = track.artist || track.filePath || '';
    const moodLabel = track.moodLabel || track.mood_label || '';
    moodBadge.hidden = !moodLabel;
    moodBadge.textContent = moodLabel ? t(moodLabel) : '';
    moodBadge.title = moodLabel ? t('Mood: {value}', { value: t(moodLabel) }) : '';
    const moodTone = /euphoric|uplifting|chill/iu.test(moodLabel)
      ? 'positive'
      : /aggressive|tense|high energy/iu.test(moodLabel)
        ? 'intense'
        : /melancholic/iu.test(moodLabel)
          ? 'reflective'
          : 'balanced';
    moodBadge.dataset.tone = moodTone;
    const rhythmLabel = track.rhythmPattern || track.rhythm_pattern || '';
    rhythmBadge.hidden = !rhythmLabel;
    rhythmBadge.textContent = rhythmLabel ? t(rhythmLabel) : '';
    rhythmBadge.title = rhythmLabel ? t('Rhythm: {value}', { value: t(rhythmLabel) }) : '';
    rhythmBadge.dataset.tone = /dembow/iu.test(rhythmLabel)
      ? 'latin'
      : /one drop|steppers/iu.test(rhythmLabel)
        ? 'roots'
        : /clave/iu.test(rhythmLabel)
          ? 'clave'
          : /boom bap/iu.test(rhythmLabel)
            ? 'hiphop'
            : 'other';
    const audioFormat = getAudioFormat(track);
    format.textContent = audioFormat;
    format.title = t('{format}-Audiodatei', { format: audioFormat });
    format.setAttribute('aria-label', t('Dateiformat {format}', { format: audioFormat }));
    bpm.textContent = track.bpm == null ? '' : `${Number(track.bpm).toFixed(0)} BPM`;
    select.disabled = false;
    play.dataset.trackId = String(track.id);
    play.title = t('Lokale Wiedergabe starten');
    play.setAttribute('aria-label', t('Wiedergabe starten: {title}', { title: track.title }));
    play.disabled = !isTauri;
  }

  const end = Math.min(state.trackTotal, start + state.trackRowPool.length + TRACK_OVERSCAN);
  void ensureTrackPages(start, end, state.trackRequestId);
  pruneTrackPages(start, end);
}

function refreshTrackListLocale() {
  const list = $('#track-list');
  if (state.trackPlane) {
    renderVirtualTracks();
  } else if (list.querySelector('.track-loading-state')) {
    const label = list.querySelector('.track-loading-state strong');
    if (label) label.textContent = t('Lokale Bibliothek wird gesucht …');
  } else {
    const empty = list.querySelector('.empty-state');
    if (empty) {
      const title = empty.querySelector('strong');
      const help = empty.querySelector('span:not(.spinner)');
      if (title) title.textContent = t(state.trackQuery ? 'Keine Treffer' : 'Noch keine Tracks');
      if (help) help.textContent = t(state.trackQuery ? 'Passe deine Suche an.' : 'Track-Metadaten, Beat-Grids und Stems werden lokal verwaltet.');
    }
  }
  $('#track-count').textContent = state.trackTotal.toLocaleString(getLocale());
}

async function fetchTrackPage(pageIndex, requestId = state.trackRequestId) {
  if (!isTauri || pageIndex < 0 || requestId !== state.trackRequestId) return;
  const key = `${requestId}:${pageIndex}`;
  if (state.tracks.has(pageIndex) || state.trackPendingPages.has(key)) return;
  const offset = pageIndex * PAGE_SIZE;
  if (pageIndex > 0 && offset >= state.trackTotal) return;
  state.trackPendingPages.add(key);
  try {
    const page = await invoke('search_tracks_page', {
      query: state.trackSearch.text,
      filters: state.trackSearch.filters,
      offset,
      limit: PAGE_SIZE,
    });
    if (requestId !== state.trackRequestId) return;
    state.trackTotal = Number(page.total) || 0;
    const tracks = page.tracks || [];
    state.tracks.set(pageIndex, tracks);
    for (let index = 0; index < tracks.length; index += 1) {
      state.trackIndexById.set(tracks[index].id, offset + index);
    }
    $('#track-count').textContent = state.trackTotal.toLocaleString(getLocale());
    if (state.trackTotal === 0) {
      const empty = document.createElement('div');
      empty.className = 'empty-state';
      const wave = document.createElement('div');
      wave.className = 'empty-wave';
      wave.textContent = '∿';
      const title = document.createElement('strong');
      title.textContent = t(state.trackQuery ? 'Keine Treffer' : 'Noch keine Tracks');
      const help = document.createElement('span');
      help.textContent = t(state.trackQuery ? 'Passe deine Suche an.' : 'Track-Metadaten, Beat-Grids und Stems werden lokal verwaltet.');
      empty.append(wave, title, help);
      $('#track-list').replaceChildren(empty);
      state.trackPlane = null;
      state.trackRowPool = [];
      return;
    }
    if (!state.trackPlane) {
      const plane = document.createElement('div');
      plane.className = 'track-virtual-plane';
      plane.setAttribute('role', 'list');
      $('#track-list').replaceChildren(plane);
      state.trackPlane = plane;
    }
    state.trackPlane.style.height = `${state.trackTotal * TRACK_ROW_HEIGHT}px`;
    renderVirtualTracks();
    if (state.mixStatus) displayMixStatus(state.mixStatus);
  } catch (error) {
    if (requestId === state.trackRequestId) toast(t('Bibliothek konnte nicht geladen werden: {error}', { error: String(error) }), true);
  } finally {
    state.trackPendingPages.delete(key);
  }
}

async function ensureTrackPages(startIndex, endIndex, requestId = state.trackRequestId) {
  if (state.trackTotal <= 0) return;
  const firstPage = Math.floor(Math.max(0, startIndex) / PAGE_SIZE);
  const lastPage = Math.floor(Math.max(0, Math.min(state.trackTotal - 1, endIndex)) / PAGE_SIZE);
  const lastToPrefetch = Math.min(Math.ceil(state.trackTotal / PAGE_SIZE) - 1, lastPage + 1);
  const requests = [];
  for (let page = firstPage; page <= lastToPrefetch; page += 1) {
    if (!state.tracks.has(page)) requests.push(fetchTrackPage(page, requestId));
  }
  if (requests.length) await Promise.allSettled(requests);
}

function formatSearchNumber(value, digits = 0) {
  return new Intl.NumberFormat(getLocale(), { maximumFractionDigits: digits }).format(value);
}

function renderSearchFeedback(query = $('#track-search').value) {
  const feedback = $('#track-search-feedback');
  if (!feedback) return;
  const filters = parseSemanticTrackQuery(query).filters;
  const pieces = [];
  const moodLabels = {
    sad: 'Sad',
    positive: 'Positive',
    calm: 'Calm',
    energetic: 'Energetic',
    aggressive: 'Aggressive',
    tense: 'Tense',
    uplifting: 'Uplifting',
    euphoric: 'Euphoric',
    balanced: 'Balanced',
  };
  if (filters.mood) pieces.push(t('Mood: {value}', { value: t(moodLabels[filters.mood] || filters.mood) }));
  if (filters.rhythm) pieces.push(t('Rhythm: {value}', { value: t(filters.rhythm) }));
  if (filters.key) pieces.push(t('Key: {value}', { value: filters.key }));
  if (filters.bpmMin !== undefined || filters.bpmMax !== undefined) {
    pieces.push(t('BPM: {min}–{max}', {
      min: filters.bpmMin === undefined ? '—' : formatSearchNumber(filters.bpmMin),
      max: filters.bpmMax === undefined ? '—' : formatSearchNumber(filters.bpmMax),
    }));
  }
  if (filters.lufsMin !== undefined || filters.lufsMax !== undefined) {
    pieces.push(t('Loudness: {min}–{max} LUFS', {
      min: filters.lufsMin === undefined ? '—' : formatSearchNumber(filters.lufsMin, 1),
      max: filters.lufsMax === undefined ? '—' : formatSearchNumber(filters.lufsMax, 1),
    }));
  }
  feedback.textContent = pieces.length ? t('Local filters: {filters}', { filters: pieces.join(' · ') }) : '';
}

async function loadTracks(query = '') {
  if (!isTauri) return;
  const requestId = ++state.trackRequestId;
  state.trackQuery = query;
  state.trackSearch = parseSemanticTrackQuery(query);
  renderSearchFeedback(query);
  state.tracks.clear();
  state.trackIndexById.clear();
  state.trackTotal = 0;
  state.trackPendingPages.clear();
  state.trackPlane = null;
  state.trackRowPool = [];
  const list = $('#track-list');
  list.scrollTop = 0;
  const loading = document.createElement('div');
  loading.className = 'empty-state track-loading-state';
  const spinner = document.createElement('span');
  spinner.className = 'spinner';
  const label = document.createElement('strong');
  label.textContent = t('Lokale Bibliothek wird gesucht …');
  loading.append(spinner, label);
  list.replaceChildren(loading);
  await fetchTrackPage(0, requestId);
}

function scheduleTrackSearch(query) {
  clearTimeout(state.trackSearchTimer);
  state.trackSearchTimer = window.setTimeout(() => loadTracks(query), 130);
}

function findLoadedTrackIndex(trackId) {
  return state.trackIndexById.get(trackId) ?? -1;
}

async function playNextTrack() {
  if (!isTauri) return toast('Nächster Track benötigt die native Audio-Engine.', true);
  if (!state.trackTotal) return toast('Die lokale Bibliothek ist leer.', true);
  const current = state.audioTrack || state.selectedTrack;
  const currentIndex = current ? findLoadedTrackIndex(current.id) : -1;
  const nextIndex = currentIndex < 0 ? 0 : (currentIndex + 1) % state.trackTotal;
  const nextPage = Math.floor(nextIndex / PAGE_SIZE);
  await fetchTrackPage(nextPage, state.trackRequestId);
  const track = virtualTrackAt(nextIndex);
  if (!track) return toast('Nächster Track konnte nicht aus SQLite geladen werden.', true);
  void loadAndPlayTrack(track);
}

function mergeImportPaths(paths) {
  const next = new Set(state.importPaths);
  for (const path of paths) next.add(path);
  state.importPaths = [...next];
  $('#import-selection-count').textContent = state.importPaths.length
    ? t('{count} Dateien ausgewählt · Ordner werden lokal durchsucht', { count: state.importPaths.length })
    : t('Keine Audio-Dateien ausgewählt');
}

async function pickAudioFiles(directory = false) {
  if (!isTauri) return toast('Dateiimport ist nur in der installierten Tauri-App verfügbar.', true);
  try {
    if (isAndroid) {
      const paths = await invoke('pick_android_import_paths', { directory });
      if (paths.length) mergeImportPaths(paths);
      return;
    }
    const selection = await open(directory
      ? { directory: true, multiple: true, title: t('Lokale Audio-Ordner auswählen') }
      : {
          multiple: true,
          title: t('Lokale Audio-Dateien auswählen'),
          filters: [{ name: t('Audio'), extensions: ['mp3', 'flac', 'ogg', 'oga', 'opus', 'wav', 'wave', 'aac', 'm4a', 'mp4', 'aiff', 'aif', 'aifc', 'webm', 'caf'] }],
        });
    const paths = Array.isArray(selection) ? selection : selection ? [selection] : [];
    if (paths.length) mergeImportPaths(paths);
  } catch (error) {
    toast(t('Dateiauswahl fehlgeschlagen: {error}', { error: String(error) }), true);
  }
}

async function pickModel(kind) {
  if (!isTauri) return toast('Modellauswahl ist nur in der installierten Tauri-App verfügbar.', true);
  const specs = {
    separator: { title: 'HT-Demucs-ONNX oder MDX-Modellmanifest auswählen', extension: 'onnx', extensions: ['onnx', 'json'], input: '#separator-model-path', stateKey: 'separationModelPath', storageKey: 'championvibe.separationModelPath' },
    whisper: { title: 'Lokales Whisper-Tiny-Modell auswählen', extension: 'bin', input: '#whisper-model-path', stateKey: 'whisperModelPath', storageKey: 'championvibe.whisperModelPath' },
    'piper-model': { title: 'Lokales Piper-ONNX-Modell auswählen', extension: 'onnx', input: '#piper-model-path', stateKey: 'piperModelPath', storageKey: 'championvibe.piperModelPath' },
    'piper-config': { title: 'Lokale Piper-Voice-JSON auswählen', extension: 'json', input: '#piper-config-path', stateKey: 'piperConfigPath', storageKey: 'championvibe.piperConfigPath' },
  };
  const spec = specs[kind];
  if (!spec) return;
  try {
    const path = isAndroid
      ? await invoke('pick_android_model', { kind })
      : await open({
          multiple: false,
          title: t(spec.title),
          filters: [{ name: t('Lokale Modell-Datei'), extensions: spec.extensions || [spec.extension] }],
        });
    if (!path || Array.isArray(path)) return;
    if (kind === 'separator' || kind === 'whisper') {
      const validationKind = kind === 'whisper'
        ? 'whisper'
        : (String(path).toLowerCase().endsWith('.cvmodel.json') ? 'separator' : 'htdemucs');
      await invoke('validate_local_model', { path, kind: validationKind });
    }
    state[spec.stateKey] = path;
    $(spec.input).value = path;
    localStorage.setItem(spec.storageKey, path);
    if (state.mixStatus) displayMixStatus(state.mixStatus);
  } catch (error) {
    toast(t('Modell konnte nicht geprüft oder ausgewählt werden: {error}', { error: String(error) }), true);
  }
}

function renderManagedModelCatalog() {
  const container = $('#managed-model-catalog');
  if (!container) return;
  container.replaceChildren();
  const items = state.modelCatalog.filter((item) => {
    if (item.id === 'separator-android') return isAndroid;
    return true;
  });
  if (!items.length) {
    const empty = document.createElement('span');
    empty.className = 'model-library-empty';
    empty.textContent = t('Keine lokalen Modellkatalog-Einträge verfügbar.');
    container.append(empty);
    return;
  }
  for (const item of items) {
    const card = document.createElement('article');
    card.className = 'managed-model-card';
    const head = document.createElement('div');
    head.className = 'managed-model-card-head';
    const copy = document.createElement('div');
    const title = document.createElement('strong');
    title.textContent = item.title;
    const purpose = document.createElement('span');
    purpose.className = 'managed-model-purpose';
    purpose.textContent = item.purpose;
    copy.append(title, purpose);
    const button = document.createElement('button');
    button.type = 'button';
    button.dataset.modelId = item.id;
    button.disabled = state.modelBusyId === item.id;
    button.textContent = state.modelBusyId === item.id
      ? t('Prüfen …')
      : item.installed ? t('Erneut prüfen') : t('Herunterladen');
    head.append(copy, button);
    const meta = document.createElement('span');
    meta.className = 'managed-model-meta';
    meta.textContent = `${item.approximateSize} · ${item.license}`;
    const rights = document.createElement('p');
    rights.className = 'managed-model-rights';
    rights.textContent = item.rightsNote;
    const hash = document.createElement('code');
    hash.className = 'managed-model-hash';
    hash.textContent = `SHA-256: ${item.sha256}`;
    const link = document.createElement('a');
    link.className = 'managed-model-source';
    link.href = item.sourceUrl;
    link.target = '_blank';
    link.rel = 'noopener noreferrer';
    link.textContent = t('Gepinnte Quelle und Revision ansehen');
    card.append(head, meta, rights, hash, link);
    if (item.installedPath) {
      const localPath = document.createElement('span');
      localPath.className = 'managed-model-purpose';
      localPath.textContent = `${t('Lokal')}: ${item.installedPath}`;
      card.append(localPath);
    }
    container.append(card);
  }
}

async function refreshManagedModelCatalog() {
  if (!isTauri) return;
  try {
    state.modelCatalog = await invoke('get_model_catalog');
    renderManagedModelCatalog();
    syncDubplateAvailability();
  } catch (error) {
    const container = $('#managed-model-catalog');
    if (container) container.textContent = t('Modellkatalog konnte nicht geladen werden: {error}', { error: String(error) });
  }
}

async function downloadManagedModel(modelId) {
  if (!isTauri || state.modelBusyId) return;
  state.modelBusyId = modelId;
  renderManagedModelCatalog();
  try {
    const path = await invoke('download_managed_model', { modelId });
    if (modelId === 'separator-android' || modelId === 'separator-four-stem') {
      state.separationModelPath = path;
      $('#separator-model-path').value = path;
      localStorage.setItem('championvibe.separationModelPath', path);
    } else if (modelId === 'whisper-tiny') {
      state.whisperModelPath = path;
      $('#whisper-model-path').value = path;
      localStorage.setItem('championvibe.whisperModelPath', path);
    }
    toast(t('Lokales Modell heruntergeladen, SHA-256 und Tensorvertrag geprüft.'));
  } catch (error) {
    toast(t('Lokaler Modelldownload oder Prüfung fehlgeschlagen: {error}', { error: String(error) }), true);
  } finally {
    state.modelBusyId = null;
    await refreshManagedModelCatalog();
  }
}

async function importCustomMdxModels() {
  if (!isTauri || state.customModelBusy) return;
  const button = $('#import-custom-mdx');
  const mode = isAndroid ? 'two-stem' : $('#custom-mdx-mode').value;
  state.customModelBusy = true;
  button.disabled = true;
  button.textContent = t('Dateien werden geprüft …');
  try {
    let selection;
    if (isAndroid) {
      const selectedPaths = [];
      const expectedFiles = mode === 'four-stem' ? 4 : 1;
      for (let index = 0; index < expectedFiles; index += 1) {
        const path = await invoke('pick_android_model', { kind: 'separator' });
        if (!path) return;
        selectedPaths.push(path);
      }
      selection = selectedPaths;
    } else {
      selection = await open({
        multiple: mode === 'four-stem',
        title: t(mode === 'four-stem' ? 'Vier benannte MDX-Net-Ziele auswählen' : 'MDX-Net-Vocals-Modell auswählen'),
        filters: [{ name: t('MDX-Net ONNX'), extensions: ['onnx'] }],
      });
    }
    const paths = Array.isArray(selection) ? selection : selection ? [selection] : [];
    if (!paths.length) return;
    for (const path of paths) await invoke('validate_local_model', { path, kind: 'separator' });
    const manifestPath = await invoke('import_custom_separator_models', { paths, mode });
    state.separationModelPath = manifestPath;
    $('#separator-model-path').value = manifestPath;
    localStorage.setItem('championvibe.separationModelPath', manifestPath);
    toast(t(mode === 'two-stem'
      ? 'MDX-Net-Modell als 2-Stem-Profil importiert: Vocals + Mix-minus-Vocals.'
      : 'MDX-Net-4-Stem-Profil importiert: Vocals, Other, Bass und Drums.'));
    await refreshManagedModelCatalog();
  } catch (error) {
    toast(t('Benutzerdefinierte MDX-Net-Dateien wurden nicht importiert: {error}', { error: String(error) }), true);
  } finally {
    state.customModelBusy = false;
    button.disabled = false;
    button.textContent = t('Lokale ONNX-Datei(en) prüfen & importieren');
  }
}

function importStageText(stage = '') {
  const separation = /^separating-(2|4)-stems(?:-(CPU|DirectML))?$/u.exec(stage);
  if (separation) {
    const [, count, backend] = separation;
    const label = count === '2' ? 'Zwei Stems' : 'Vier Stems';
    return t(backend ? `${label} · ${backend}` : `${label} trennen`);
  }
  const labels = {
    starting: 'Warteschlange wird vorbereitet',
    'loading-local-models': 'Lokale Modelle laden',
    'decoding-source': 'Audio lokal dekodieren',
    'analyzing-bpm-key-energy': 'BPM, Tonart, Lautheit, Stimmung und Rhythmus analysieren',
    'preparing-acapella-for-whisper': 'Acapella für Whisper vorbereiten',
    'transcribing-lyrics-locally': 'Lyrics lokal transkribieren',
    'committing-track-metadata': 'Metadaten lokal speichern',
    committing: 'Metadaten lokal speichern',
  };
  return t(labels[stage] || stage || 'Wird vorbereitet');
}

function importStatusText(status) {
  return ({
    queued: 'In Warteschlange',
    running: 'In Bearbeitung',
    completed: 'Abgeschlossen',
    failed: 'Fehlgeschlagen',
    cancelled: 'Abgebrochen',
  })[status] || t(status);
}

function renderImportJobs() {
  const container = $('#import-jobs');
  container.replaceChildren();
  if (!state.importJobs.length) {
    const empty = document.createElement('div');
    empty.className = 'import-jobs-empty';
    empty.textContent = t('Noch keine Import-Aufträge');
    container.append(empty);
    return;
  }
  for (const job of state.importJobs.slice(0, 8)) {
    const card = document.createElement('article');
    card.className = 'import-job';
    const heading = document.createElement('div');
    heading.className = 'import-job-heading';
    const title = document.createElement('strong');
    title.textContent = `Import #${job.id}`;
    const status = document.createElement('span');
    status.className = `import-job-status ${job.status}`;
    status.textContent = importStatusText(job.status);
    heading.append(title, status);

    const counts = document.createElement('div');
    counts.className = 'import-job-counts';
    counts.textContent = t('{successful} / {total} fertig · {failed} Fehler{cancelled}', {
      successful: job.successfulItems,
      total: job.totalItems,
      failed: job.failedItems,
      cancelled: job.cancelledItems ? t(' · {count} abgebrochen', { count: job.cancelledItems }) : '',
    });

    const stage = document.createElement('div');
    stage.className = 'import-job-stage';
    stage.textContent = job.status === 'running'
      ? t('{stage}{path}', {
        stage: importStageText(job.currentStage),
        path: job.currentPath ? ` · ${job.currentPath.split(/[\\/]/).pop()}` : '',
      })
      : t(job.error || importStatusText(job.status));
    stage.title = t(job.currentPath || job.error || '');

    const progress = document.createElement('div');
    progress.className = 'import-progress-track';
    progress.setAttribute('role', 'progressbar');
    progress.setAttribute('aria-valuemin', '0');
    progress.setAttribute('aria-valuemax', '100');
    const percentage = job.status === 'completed' ? 100 : Math.round((job.currentProgress || 0) * 100);
    progress.setAttribute('aria-valuenow', String(percentage));
    const fill = document.createElement('span');
    fill.style.width = `${percentage}%`;
    progress.append(fill);

    card.append(heading, counts, stage, progress);
    if (job.status === 'queued' || job.status === 'running') {
      const cancel = document.createElement('button');
      cancel.type = 'button';
      cancel.className = 'import-cancel-button';
      cancel.textContent = t('Auftrag abbrechen');
      cancel.addEventListener('click', async () => {
        cancel.disabled = true;
        try {
          await invoke('cancel_import_job', { jobId: job.id });
          toast(t('Import #{id} wird abgebrochen.', { id: job.id }));
        } catch (error) {
          toast(t('Import konnte nicht abgebrochen werden: {error}', { error: String(error) }), true);
        } finally {
          cancel.disabled = false;
        }
      });
      card.append(cancel);
    }
    container.append(card);
  }
}

async function refreshImportJobs() {
  if (!isTauri) return;
  try {
    state.importJobs = await invoke('get_import_jobs', { limit: 20 });
    renderImportJobs();
  } catch (error) {
    toast(t('Importstatus konnte nicht geladen werden: {error}', { error: String(error) }), true);
  }
}

async function startImport() {
  if (!isTauri) return toast('Lokale Analyse ist nur in der installierten Tauri-App verfügbar.', true);
  if (!state.importPaths.length) return toast('Wähle zuerst Audio-Dateien oder einen Ordner aus.', true);
  if (!state.separationModelPath || !state.whisperModelPath) {
    return toast('Wähle ein lokal geprüftes Separator-Modell oder MDX-Manifest sowie ein Whisper-Tiny-.bin-Modell aus.', true);
  }
  const button = $('#start-import');
  if (state.importBusy) return;
  state.importBusy = true;
  setBusy(button, true, isAndroid ? 'Dateien lokal übernehmen …' : 'Import wird vorbereitet …');
  try {
    const job = await invoke('start_import_job', {
      request: {
        paths: state.importPaths,
        separationModelPath: state.separationModelPath,
        whisperModelPath: state.whisperModelPath,
        useDirectml: $('#use-directml').checked,
        cpuThreads: Number($('#cpu-threads').value),
      },
    });
    state.importPaths = [];
    mergeImportPaths([]);
    state.importJobs = [job, ...state.importJobs.filter((item) => item.id !== job.id)].slice(0, 20);
    renderImportJobs();
    toast(t('Import #{id} gestartet · nur lokale Verarbeitung.', { id: job.id }));
  } catch (error) {
    toast(t('Import konnte nicht gestartet werden: {error}', { error: String(error) }), true);
  } finally {
    state.importBusy = false;
    setBusy(button, false);
  }
}

function updateImportJob(job) {
  const previous = state.importJobs.find((item) => item.id === job.id);
  state.importJobs = [job, ...state.importJobs.filter((item) => item.id !== job.id)]
    .sort((left, right) => right.id - left.id)
    .slice(0, 20);
  renderImportJobs();
  if (job.successfulItems > (previous?.successfulItems || 0)) loadTracks($('#track-search').value.trim());
  if (job.status === 'completed' && previous?.status !== 'completed') refreshRiddimFamilies();
}

function refreshLocaleDependentUI() {
  const locale = getLocale();
  $('#language-select').value = locale;
  for (const button of document.querySelectorAll('[data-busy-label-key]')) {
    button.innerHTML = `<span class="spinner"></span><span>${t(button.dataset.busyLabelKey)}</span>`;
  }
  const performance = $('#app-shell').dataset.viewMode === 'performance';
  $('#toggle-performance').textContent = t(performance ? 'Zurück zum Player' : 'Performance Mode');
  const toolsButton = $('#toggle-tools');
  const toolsLabel = t($('#app-shell').dataset.toolsOpen === 'true' ? 'Tools schließen' : 'Library-Tools');
  toolsButton.textContent = toolsLabel;
  toolsButton.setAttribute('aria-label', toolsLabel);
  toolsButton.title = toolsLabel;

  if (state.licenseStatus) displayStatus(state.licenseStatus);
  else syncFreemiumUI();
  displayAudioStatus(state.audioStatus);
  updateAutoGainControls();
  $('#pitch-lock-mode').textContent = t(state.pitchLockEnabled ? 'Pitch-locked WSOLA' : 'Varispeed · pitch follows tempo');
  renderSearchFeedback();
  if (state.mixStatus) displayMixStatus(state.mixStatus);
  if (state.audioTrack || state.selectedTrack) updateCurrentTrackUI(state.audioTrack || state.selectedTrack);
  if (state.selectedTrack) renderSelectedTrackContext(state.selectedTrack);
  refreshTrackListLocale();
  mergeImportPaths([]);
  renderImportJobs();
  renderSampleCatalog();
  if (state.sampleFolderError) {
    $('#sample-folder-path').textContent = t('Sample-Katalog nicht verfügbar: {error}', { error: state.sampleFolderError });
  } else if (state.sampleFolder) {
    $('#sample-folder-path').textContent = t('{count} Samples im RAM · {path}', { count: state.sampleCatalog.length, path: state.sampleFolder });
  }
  renderRiddimFamilies();
  renderOraclePrices();
  if (state.oracleStatus) setOracleStatus(state.oracleStatus.key, state.oracleStatus.values);
  if (state.quote) setQuote(state.quote);
  if (state.waveformPeaks?.length) {
    $('#waveform-mode-note').textContent = t('{peaks} Peak-Buckets · {beats} Beat-Marker{bpm} · lokal', {
      peaks: state.waveformPeaks.length,
      beats: state.waveformBeatGrid?.length || 0,
      bpm: state.waveformBpm ? ` · ${state.waveformBpm.toFixed(1)} BPM` : '',
    });
  } else {
    applyWaveformMode(state.waveformMode);
  }
  $('#tts-state').textContent = t(state.kokoroWasmReady
    ? 'KOKORO · WASM'
    : state.ttsAvailable ? 'PIPER · CPU' : 'WASM · NICHT GELADEN');
  $('#tts-state').title = t(state.kokoroWasmReady
    ? 'Kokoro läuft lokal mit ONNX Runtime WebAssembly. Die Stimme spricht US-Englisch, keine authentische Patois-Stimme.'
    : state.ttsAvailable
      ? 'Lokale Piper-Inferenz ist in diesem Build aktiviert.'
      : 'Lade die geprüften Kokoro-Gewichte und die Stimme aus der lokalen Modellbibliothek.');
  const loadKokoroButton = $('#load-kokoro-wasm');
  if (loadKokoroButton) {
    loadKokoroButton.textContent = t(state.kokoroWasmBusy
      ? 'Kokoro WASM wird geladen …'
      : state.kokoroWasmReady ? 'Kokoro WASM geladen' : 'Kokoro WASM laden');
  }
  syncDubplateAvailability();
}

$('#language-select').value = getLocale();
$('#language-select').addEventListener('change', (event) => setLocale(event.target.value));
window.addEventListener('championvibe:locale-change', refreshLocaleDependentUI);

for (const option of document.querySelectorAll('.plan-option')) {
  option.addEventListener('click', () => {
    state.plan = option.dataset.plan;
    document.querySelectorAll('.plan-option').forEach((item) => {
      const selected = item === option;
      item.classList.toggle('selected', selected);
      item.setAttribute('aria-pressed', String(selected));
    });
    state.quote = null;
    $('#quote-card').classList.add('hidden');
    state.paymentOptions[state.plan] = {};
    renderOraclePrices(state.plan);
    if ($('#license-modal').open) loadOraclePrices(true);
  });
}

$('#chain-select').addEventListener('change', () => {
  state.quote = null;
  $('#quote-card').classList.add('hidden');
});
$('#create-quote').addEventListener('click', createQuote);
$('#activate-license').addEventListener('click', activate);
$('#connect-wallet').addEventListener('click', connectWallet);
$('#open-license-modal').addEventListener('click', openLicenseModal);
$('#close-license-modal').addEventListener('click', closeLicenseModal);
$('#license-modal').addEventListener('click', (event) => {
  if (event.target === $('#license-modal')) closeLicenseModal();
});
$('#open-info-modal').addEventListener('click', () => openInfoModal());
$('#close-info-modal').addEventListener('click', closeInfoModal);
$('#info-modal').addEventListener('click', (event) => {
  if (event.target === $('#info-modal')) closeInfoModal();
});
for (const tab of document.querySelectorAll('[data-info-tab]')) {
  tab.addEventListener('click', () => setInfoTab(tab.dataset.infoTab));
}
$('#refresh-oracle-prices').addEventListener('click', () => loadOraclePrices(true));
$('#oracle-price-grid').addEventListener('click', (event) => {
  const button = event.target.closest('[data-select-chain]');
  if (!button) return;
  $('#chain-select').value = button.dataset.selectChain;
  const quote = state.paymentOptions[state.plan]?.[button.dataset.selectChain];
  if (cachedQuoteIsFresh(quote)) setQuote(quote);
  else createQuote();
});
$('#issuer-url').addEventListener('change', () => {
  state.paymentOptions = {};
  renderOraclePrices();
  if ($('#license-modal').open) loadOraclePrices(true);
});
$('#refresh-status').addEventListener('click', refreshStatus);
$('#toggle-performance').addEventListener('click', () => {
  setViewMode($('#app-shell').dataset.viewMode === 'performance' ? 'player' : 'performance');
});
$('#toggle-tools').addEventListener('click', () => setToolsOpen($('#app-shell').dataset.toolsOpen !== 'true'));
for (const button of document.querySelectorAll('[data-toggle-tools]')) {
  button.addEventListener('click', () => {
    setToolsOpen(true);
    $('#device').scrollIntoView({ behavior: 'smooth', block: 'start' });
  });
}
for (const button of document.querySelectorAll('[data-open-license-modal]')) {
  button.addEventListener('click', openLicenseModal);
}
$('#force-next-track').addEventListener('click', forceNextTrack);
$('#pull-up-rewind').addEventListener('click', pullUpRewind);
$('#drop-sample').addEventListener('click', dropSelectedSample);
$('#dj-shoutout').addEventListener('click', generateDubplate);
$('#intensity-override-toggle').addEventListener('change', applyIntensityOverride);
$('#waveform-mode').addEventListener('change', (event) => applyWaveformMode(event.target.value));
$('#record-button').addEventListener('click', toggleMasterRecording);
$('#intensity-slider').addEventListener('input', updateIntensityReadout);
$('#intensity-slider').addEventListener('change', () => {
  state.intensityEditing = false;
  if (state.intensityOverride) applyIntensityOverride();
});
$('#check-recording').addEventListener('click', checkRecordingGate);
$('#copy-recipient').addEventListener('click', () => copyText(state.quote?.recipient, 'Empfängeradresse'));
$('#copy-binding').addEventListener('click', () => copyText(state.binding, 'Geräte-Fingerprint'));
$('#track-search').addEventListener('input', (event) => {
  renderSearchFeedback(event.target.value);
  scheduleTrackSearch(event.target.value.trim());
});
$('#track-list').addEventListener('scroll', () => {
  if (state.trackRenderFrame) return;
  state.trackRenderFrame = requestAnimationFrame(() => {
    state.trackRenderFrame = 0;
    renderVirtualTracks();
  });
}, { passive: true });
$('#track-list').addEventListener('click', (event) => {
  const row = event.target.closest('.virtual-track-row');
  if (!row || !row.dataset.trackId) return;
  const track = virtualTrackAt(Number(row.dataset.index));
  if (!track || row.dataset.trackId !== String(track.id)) return;
  if (event.target.closest('.track-play')) loadAndPlayTrack(track, row.querySelector('.track-play'));
  else if (event.target.closest('.track-select')) void selectTrack(track);
});
$('#pick-audio-files').addEventListener('click', () => pickAudioFiles(false));
$('#pick-audio-folders').addEventListener('click', () => pickAudioFiles(true));
$('#pick-separator-model').addEventListener('click', () => pickModel('separator'));
$('#pick-whisper-model').addEventListener('click', () => pickModel('whisper'));
$('#refresh-model-catalog').addEventListener('click', refreshManagedModelCatalog);
$('#managed-model-catalog').addEventListener('click', (event) => {
  const button = event.target.closest('[data-model-id]');
  if (button && !button.disabled) void downloadManagedModel(button.dataset.modelId);
});
$('#import-custom-mdx').addEventListener('click', importCustomMdxModels);
$('#pick-piper-model').addEventListener('click', () => pickModel('piper-model'));
$('#pick-piper-config').addEventListener('click', () => pickModel('piper-config'));
$('#load-kokoro-wasm').addEventListener('click', loadKokoroWasm);
$('#save-riddim-tags').addEventListener('click', saveRiddimTags);
$('#rebuild-riddims').addEventListener('click', rebuildRiddims);
$('#load-sample-folder').addEventListener('click', loadSampleFolder);
$('#wheel-and-come-again').addEventListener('click', wheelAndComeAgain);
$('#generate-dubplate').addEventListener('click', generateDubplate);
$('#auto-rewind').addEventListener('change', (event) => setAutoRewind(event.target.checked));
$('#clear-import-selection').addEventListener('click', () => {
  state.importPaths = [];
  mergeImportPaths([]);
});
$('#start-import').addEventListener('click', startImport);
$('#refresh-import-jobs').addEventListener('click', refreshImportJobs);
$('#stem-switches').addEventListener('click', (event) => {
  const button = event.target.closest('[data-stem-index]');
  if (button && !button.disabled) void toggleStemSwitch(Number(button.dataset.stemIndex));
});
$('#audio-play').addEventListener('click', playAudio);
$('#audio-pause').addEventListener('click', pauseAudio);
$('#audio-stop').addEventListener('click', stopAudio);
$('#audio-next').addEventListener('click', playNextTrack);
$('#mix-start').addEventListener('click', startMix);
$('#mix-stop').addEventListener('click', stopMix);
$('#mix-preset').addEventListener('change', changeMixPreset);

for (const button of document.querySelectorAll('[data-kill]')) {
  button.addEventListener('click', () => {
    const enabled = button.getAttribute('aria-pressed') !== 'true';
    button.setAttribute('aria-pressed', String(enabled));
    applyAudioControls({ [button.dataset.kill]: enabled });
  });
}

for (const [inputId, outputId, control] of [
  ['tape-mix', 'tape-mix-value', 'tapeMix'],
  ['spring-mix', 'spring-mix-value', 'springMix'],
]) {
  const input = $(`#${inputId}`);
  const output = $(`#${outputId}`);
  input.addEventListener('input', () => { output.textContent = `${input.value}%`; });
  input.addEventListener('change', () => {
    applyAudioControls({ [control]: Number(input.value) / 100 });
  });
}

updateAutoGainControls();
$('#auto-gain-enabled').addEventListener('change', (event) => {
  void applyAutoGainSettings(event.target.checked, state.autoGainTargetLufs);
});
$('#auto-gain-target').addEventListener('change', (event) => {
  if (event.target.value === 'off') {
    void applyAutoGainSettings(false, state.autoGainTargetLufs);
  } else {
    void applyAutoGainSettings(true, event.target.value);
  }
});
$('#pitch-lock-enabled').checked = state.pitchLockEnabled;
$('#pitch-lock-mode').textContent = t(state.pitchLockEnabled ? 'Pitch-locked WSOLA' : 'Varispeed · pitch follows tempo');
$('#pitch-lock-enabled').addEventListener('change', (event) => applyPitchLock(event.target.checked));
$('#recording-format').value = [...$('#recording-format').options].some((option) => option.value === state.recordingFormat && !option.disabled)
  ? state.recordingFormat
  : 'wavPcm16';
state.recordingFormat = $('#recording-format').value;
$('#recording-format').addEventListener('change', (event) => {
  state.recordingFormat = event.target.value;
  localStorage.setItem('championvibe.recordingFormat', state.recordingFormat);
});
$('#separator-model-path').value = state.separationModelPath;
$('#whisper-model-path').value = state.whisperModelPath;
$('#piper-model-path').value = state.piperModelPath;
$('#piper-config-path').value = state.piperConfigPath;
syncDubplateAvailability();
$('#auto-rewind').checked = localStorage.getItem('championvibe.autoRewind') === 'true';
if (isAndroid) $('#waveform-mode').querySelector('option[value="canvas"]').disabled = true;
applyWaveformMode(state.waveformMode);
renderStemSwitches();
if (typeof ResizeObserver !== 'undefined') {
  state.waveformResizeObserver = new ResizeObserver(() => {
    if (state.waveformPeaks) requestAnimationFrame(drawWaveform);
  });
  state.waveformResizeObserver.observe($('#track-visualization'));
  state.trackResizeObserver = new ResizeObserver(() => {
    if (state.trackRenderFrame) return;
    state.trackRenderFrame = requestAnimationFrame(() => {
      state.trackRenderFrame = 0;
      renderVirtualTracks();
    });
  });
  state.trackResizeObserver.observe($('#track-list'));
}
mergeImportPaths([]);
renderImportJobs();
if (!/windows/i.test(platformText)) $('#directml-option').classList.add('hidden');

if (!isTauri) {
  $('#create-quote').disabled = true;
  $('#activate-license').disabled = true;
  $('#connect-wallet').disabled = true;
  $('#refresh-oracle-prices').disabled = true;
  for (const control of document.querySelectorAll('#audio-play, #audio-pause, #audio-stop, #audio-next, #mix-start, #mix-stop, #mix-preset, [data-kill], #tape-mix, #spring-mix, #auto-gain-enabled, #auto-gain-target, #pitch-lock-enabled, #pick-audio-files, #pick-audio-folders, #pick-separator-model, #pick-whisper-model, #pick-piper-model, #pick-piper-config, #load-kokoro-wasm, #refresh-model-catalog, #import-custom-mdx, #custom-mdx-mode, #clear-import-selection, #start-import, #refresh-import-jobs, #save-riddim-tags, #rebuild-riddims, #load-sample-folder, #sample-quantization, #performance-sample, #performance-quantization, #rewind-sample, #wheel-and-come-again, #auto-rewind, #dubplate-text, #dubplate-preset, #dubplate-dj-name, #generate-dubplate, #dj-shoutout')) {
    control.disabled = true;
  }
  displayAudioStatus(null);
} else {
  void syncRecordingFormatAvailability();
  void applyAutoGainSettings(state.autoGainEnabled, state.autoGainTargetLufs, true);
  void applyPitchLock(state.pitchLockEnabled, true);
  refreshImportJobs();
  refreshManagedModelCatalog();
  refreshSampleCatalog();
  refreshTtsStatus();
  refreshRiddimFamilies();
  setAutoRewind($('#auto-rewind').checked);
  listen('import-job-updated', (event) => updateImportJob(event.payload))
    .catch((error) => toast(t('Importereignisse sind nicht verfügbar: {error}', { error: String(error) }), true));
}
refreshStatus().then((status) => {
  if (status?.active) restoreProSession();
  else openLicenseModal();
});
loadTracks();
refreshAudioStatus();
if (isTauri) {
  window.setInterval(refreshAudioStatus, 1000);
  window.setInterval(refreshRecordingStatus, 1000);
  void initializeMixIntensity().finally(() => {
    refreshMixStatus();
    window.setInterval(refreshMixStatus, 500);
  });
} else {
  displayMixStatus({ phase: 'stopped' });
}
