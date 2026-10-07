import { t as translate } from '../i18n.js';

/**
 * Beat-grid / key editor for one library track.
 *
 * The module is split in two halves on purpose:
 *
 * * the exported pure helpers (`camelotForMusicalKey`, `nudgeBpm`, `respaceBeats`, `confidenceBand`,
 *   …) hold every rule the editor applies — Camelot-wheel transposition, ±0.1 BPM nudging, the
 *   millisecond grid shift and the confidence badge bands. They run in plain Node and are covered
 *   by `grid_editor.test.js` and mirrored by the Rust side (`camelot.rs`, `database.rs`).
 * * `initGridEditor` wires the `<dialog id="grid-editor-modal">` markup to those helpers. Saving
 *   calls the `correct_track_grid` command, which sets `is_user_verified` and hands the corrected
 *   grid to the live deck and the mix engine.
 */

/** Confidences at or above this value count as trusted; below it the row is a yellow/orange guess. */
export const CONFIDENCE_THRESHOLD = 0.7;
/** One BPM nudge step. */
export const BPM_NUDGE = 0.1;
export const MIN_BPM = 30;
export const MAX_BPM = 300;
/** Largest grid shift a single nudge sequence may reach, mirroring `MAX_GRID_SHIFT_MS` in Rust. */
export const MAX_GRID_SHIFT_MS = 2000;
/** Fine and coarse grid-shift steps in milliseconds. */
export const GRID_SHIFT_STEPS_MS = Object.freeze([-10, -1, 1, 10]);

const NOTE_NAMES = Object.freeze([
  'C', 'C#', 'D', 'D#', 'E', 'F', 'F#', 'G', 'G#', 'A', 'A#', 'B',
]);
const FLAT_EQUIVALENTS = Object.freeze({
  Db: 'C#', Eb: 'D#', Gb: 'F#', Ab: 'G#', Bb: 'A#', Cb: 'B', Fb: 'E', 'E#': 'F',
});

/** Canonical Camelot wheel: minor positions hold the `A` codes, major positions the `B` codes. */
export const CAMELOT_WHEEL = Object.freeze([
  Object.freeze({ code: '1A', key: 'G# minor' }),
  Object.freeze({ code: '2A', key: 'D# minor' }),
  Object.freeze({ code: '3A', key: 'A# minor' }),
  Object.freeze({ code: '4A', key: 'F minor' }),
  Object.freeze({ code: '5A', key: 'C minor' }),
  Object.freeze({ code: '6A', key: 'G minor' }),
  Object.freeze({ code: '7A', key: 'D minor' }),
  Object.freeze({ code: '8A', key: 'A minor' }),
  Object.freeze({ code: '9A', key: 'E minor' }),
  Object.freeze({ code: '10A', key: 'B minor' }),
  Object.freeze({ code: '11A', key: 'F# minor' }),
  Object.freeze({ code: '12A', key: 'C# minor' }),
  Object.freeze({ code: '1B', key: 'B major' }),
  Object.freeze({ code: '2B', key: 'F# major' }),
  Object.freeze({ code: '3B', key: 'C# major' }),
  Object.freeze({ code: '4B', key: 'G# major' }),
  Object.freeze({ code: '5B', key: 'D# major' }),
  Object.freeze({ code: '6B', key: 'A# major' }),
  Object.freeze({ code: '7B', key: 'F major' }),
  Object.freeze({ code: '8B', key: 'C major' }),
  Object.freeze({ code: '9B', key: 'G major' }),
  Object.freeze({ code: '10B', key: 'D major' }),
  Object.freeze({ code: '11B', key: 'A major' }),
  Object.freeze({ code: '12B', key: 'E major' }),
]);

function normaliseTonic(value) {
  const text = String(value ?? '').trim();
  if (!text) return null;
  const letter = text[0].toUpperCase();
  if (!/[A-G]/u.test(letter)) return null;
  const accidentals = text.slice(1).replace(/♯/gu, '#').replace(/♭/gu, 'b');
  if (!/^(#|b)*$/u.test(accidentals)) return null;
  const spelled = `${letter}${accidentals}`;
  return FLAT_EQUIVALENTS[spelled] ?? spelled;
}

/** Splits `A minor` / `Bb major` into a sharp-spelled tonic and a mode flag. */
export function parseMusicalKey(value) {
  const parts = String(value ?? '').trim().split(/\s+/u);
  const tonic = normaliseTonic(parts[0]);
  if (!tonic || !NOTE_NAMES.includes(tonic)) return null;
  const mode = (parts[1] ?? 'major').toLowerCase();
  if (parts.length > 2) return null;
  if (['major', 'maj', 'ionian'].includes(mode)) return { tonic, major: true, mode: 'major' };
  if (['minor', 'min', 'aeolian'].includes(mode)) return { tonic, major: false, mode: 'minor' };
  return null;
}

/** Camelot code of a musical key, e.g. `A minor` → `8A`, `C major` → `8B`. */
export function camelotForMusicalKey(value) {
  const parsed = parseMusicalKey(value);
  if (!parsed) return null;
  const spelled = parsed.major ? `${parsed.tonic} major` : `${parsed.tonic} minor`;
  return CAMELOT_WHEEL.find((entry) => entry.key === spelled)?.code ?? null;
}

/** Musical key of a Camelot code, e.g. `8A` → `A minor`. */
export function musicalKeyForCamelot(code) {
  const normalised = String(code ?? '').trim().toUpperCase();
  return CAMELOT_WHEEL.find((entry) => entry.code === normalised)?.key ?? null;
}

/** The three wheel positions that mix harmonically: other mode, plus and minus one step. */
export function compatibleCamelotCodes(code) {
  const normalised = String(code ?? '').trim().toUpperCase();
  const entry = CAMELOT_WHEEL.find((item) => item.code === normalised);
  if (!entry) return [];
  const [, digits, letter] = /^(\d+)([AB])$/u.exec(normalised) ?? [];
  const position = Number(digits);
  const otherLetter = letter === 'A' ? 'B' : 'A';
  const step = (offset) => ((position - 1 + offset + 12) % 12) + 1;
  return [`${position}${otherLetter}`, `${step(1)}${letter}`, `${step(-1)}${letter}`];
}

/** Moves a key by whole semitones inside its mode, e.g. `8A` + 1 → `3A`. */
export function transposeCamelot(code, semitones) {
  const key = musicalKeyForCamelot(code);
  const parsed = parseMusicalKey(key);
  if (!parsed) return null;
  const target = (((NOTE_NAMES.indexOf(parsed.tonic) + semitones) % 12) + 12) % 12;
  const spelled = `${NOTE_NAMES[target]} ${parsed.mode}`;
  return camelotForMusicalKey(spelled) ?? null;
}

/** Clamps and rounds a BPM value to one decimal, the precision the nudges work in. */
export function clampBpm(value) {
  const numeric = Number(value);
  if (!Number.isFinite(numeric)) return null;
  const clamped = Math.min(Math.max(numeric, MIN_BPM), MAX_BPM);
  return Math.round(clamped * 10) / 10;
}

/** Absolute BPM the editor starts from: the stored value, or the 120 BPM default when unanalysed. */
export function initialBpm(track) {
  const stored = Number(track?.bpm);
  return Number.isFinite(stored) && stored > 0 ? clampBpm(stored) : 120;
}

/** Applies one ±0.1 (or larger) nudge and returns the absolute value to send back. */
export function nudgeBpm(bpm, delta = BPM_NUDGE) {
  const numeric = Number(bpm);
  if (!Number.isFinite(numeric)) return null;
  const base = numeric > 0 ? clampBpm(numeric) : 120;
  if (base === null) return null;
  return clampBpm(Math.round((base + delta) * 10) / 10);
}

/**
 * Clamps a grid shift into the range the backend accepts. Shifts are whole milliseconds: the
 * correction is applied to a beat grid stored in seconds, and sub-millisecond moves would only add
 * rounding noise to the stored JSON.
 */
export function clampGridShiftMs(value) {
  const numeric = Number(value);
  if (!Number.isFinite(numeric)) return 0;
  return Math.round(Math.min(Math.max(numeric, -MAX_GRID_SHIFT_MS), MAX_GRID_SHIFT_MS));
}

/**
 * Re-spaces a beat grid for a new tempo while keeping the downbeat where it is, so nudging the BPM
 * never makes the grid drift away from the music. Mirrors `respace_beats` in `database.rs`.
 */
export function respaceBeats(beats, bpm) {
  const list = Array.isArray(beats) ? beats.filter((beat) => Number.isFinite(beat)) : [];
  const tempo = clampBpm(bpm);
  if (!list.length || tempo === null) return [...list];
  const period = 60 / tempo;
  const phase = Math.max(list[0], 0);
  return list.map((_, index) => phase + index * period);
}

/** Shifts every beat by milliseconds and drops beats that land before the track start. */
export function shiftBeats(beats, deltaMs) {
  const list = Array.isArray(beats) ? beats.filter((beat) => Number.isFinite(beat)) : [];
  const delta = clampGridShiftMs(deltaMs) / 1000;
  if (!delta) return [...list];
  return list.map((beat) => beat + delta).filter((beat) => beat >= 0);
}

/** Beat index nearest to a position in seconds, for drawing the moved grid. */
export function nearestBeatIndex(beats, seconds) {
  if (!Array.isArray(beats) || !beats.length || !Number.isFinite(seconds)) return -1;
  let best = 0;
  let bestDistance = Infinity;
  beats.forEach((beat, index) => {
    const distance = Math.abs(beat - seconds);
    if (distance < bestDistance) {
      bestDistance = distance;
      best = index;
    }
  });
  return best;
}

function readConfidence(track, camelCase, snakeCase) {
  const value = Number(track?.[camelCase] ?? track?.[snakeCase]);
  return Number.isFinite(value) ? Math.min(Math.max(value, 0), 1) : 0;
}

export function trackBpmConfidence(track) {
  return readConfidence(track, 'bpmConfidence', 'bpm_confidence');
}

export function trackKeyConfidence(track) {
  return readConfidence(track, 'keyConfidence', 'key_confidence');
}

export function trackIsVerified(track) {
  return Boolean(track?.isUserVerified ?? track?.is_user_verified);
}

/**
 * Classifies a row for the confidence badge:
 * `verified` (green, human corrected), `trusted` (green, confident heuristic),
 * `heuristic` (yellow/orange, uncertain heuristic) or `unknown` (nothing analysed yet).
 */
export function confidenceBand(track) {
  if (trackIsVerified(track)) return 'verified';
  const bpm = Number(track?.bpm);
  if (!Number.isFinite(bpm) || bpm <= 0) return 'unknown';
  const confidences = [trackBpmConfidence(track)];
  if (track?.musicalKey ?? track?.musical_key) confidences.push(trackKeyConfidence(track));
  const weakest = Math.min(...confidences);
  return weakest >= CONFIDENCE_THRESHOLD ? 'trusted' : 'heuristic';
}

/** Weakest analysed confidence in percent, or null when there is nothing to show. */
export function confidencePercent(track) {
  const band = confidenceBand(track);
  if (band === 'unknown') return null;
  if (band === 'verified') return 100;
  const confidences = [trackBpmConfidence(track)];
  if (track?.musicalKey ?? track?.musical_key) confidences.push(trackKeyConfidence(track));
  return Math.round(Math.min(...confidences) * 100);
}

/** Badge text/tone for the virtual track list. Keys are translated by the caller. */
export function confidenceBadge(track) {
  const band = confidenceBand(track);
  const percent = confidencePercent(track);
  if (band === 'unknown') return { tone: 'unknown', key: 'Nicht analysiert', values: {} };
  if (band === 'verified') return { tone: 'verified', key: 'Verifiziert', values: {} };
  return {
    tone: band,
    key: 'Konfidenz {percent} %',
    values: { percent: percent ?? 0 },
  };
}

/** Detail line for the badge tooltip. */
export function confidenceTooltip(track) {
  const band = confidenceBand(track);
  const bpm = Math.round(trackBpmConfidence(track) * 100);
  const keyPercent = Math.round(trackKeyConfidence(track) * 100);
  if (band === 'unknown') return { key: 'Nicht analysiert · Stift startet die manuelle Analyse', values: {} };
  if (band === 'verified') {
    return { key: 'Manuell verifiziert · BPM {bpm} % · Tonart {key} %', values: { bpm, key: keyPercent } };
  }
  if (band === 'trusted') {
    return { key: 'Heuristik vertrauenswürdig · BPM {bpm} % · Tonart {key} %', values: { bpm, key: keyPercent } };
  }
  return {
    key: 'Heuristik unsicher · BPM {bpm} % · Tonart {key} % · bitte prüfen',
    values: { bpm, key: keyPercent },
  };
}

/**
 * Merges a `correct_track_grid` outcome into a cached track row, so the badge and the BPM column
 * update without a full library reload. Returns a new object; the input is not mutated.
 */
export function applyCorrectionToTrack(track, outcome) {
  if (!track || !outcome) return track;
  const bpm = Number(outcome.bpm ?? track.bpm);
  const musicalKey = outcome.musicalKey ?? track.musicalKey ?? track.musical_key ?? null;
  return {
    ...track,
    bpm: Number.isFinite(bpm) ? bpm : track.bpm,
    bpmConfidence: Number(outcome.bpmConfidence ?? track.bpmConfidence ?? 0),
    keyConfidence: Number(outcome.keyConfidence ?? track.keyConfidence ?? 0),
    isUserVerified: outcome.isUserVerified ?? true,
    musicalKey,
    camelotKey: outcome.camelotKey ?? camelotForMusicalKey(musicalKey),
  };
}

/**
 * Rewrites one track inside the paged track cache. Returns the number of pages that changed.
 * The cache is a `Map<pageIndex, track[]>` exactly as `main.js` keeps it.
 */
export function replaceTrackInPages(pages, trackId, patchedTrack) {
  if (!(pages instanceof Map) || !patchedTrack) return 0;
  let changed = 0;
  for (const [pageIndex, page] of pages) {
    if (!Array.isArray(page)) continue;
    const index = page.findIndex((track) => String(track?.id) === String(trackId));
    if (index < 0) continue;
    const next = [...page];
    next[index] = patchedTrack;
    pages.set(pageIndex, next);
    changed += 1;
  }
  return changed;
}

/** Formats a whole-millisecond grid shift for display. */
export function formatShiftMs(value) {
  const rounded = clampGridShiftMs(value);
  return `${rounded > 0 ? '+' : ''}${rounded} ms`;
}

/** Rounds a BPM value for display, always with one decimal; a missing tempo reads as `—`. */
export function formatBpm(value) {
  const numeric = Number(value);
  if (!Number.isFinite(numeric) || numeric <= 0) return '—';
  const clamped = clampBpm(numeric);
  return clamped === null ? '—' : clamped.toFixed(1);
}

/**
 * Builds the payload for `correct_track_grid`. Only the values the user actually touched are sent,
 * so an untouched key or grid keeps its analysed value.
 */
export function buildCorrection({ bpm, bpmChanged, gridShiftMs, musicalKey, camelotKey }) {
  const correction = {};
  if (bpmChanged && clampBpm(bpm) !== null) correction.bpm = clampBpm(bpm);
  const shift = clampGridShiftMs(gridShiftMs ?? 0);
  if (shift) correction.gridShiftMs = shift;
  if (musicalKey) correction.musicalKey = musicalKey;
  if (camelotKey) correction.camelotKey = camelotKey;
  return correction;
}

/**
 * Wires the grid-editor dialog. `save` is provided by `main.js` and performs the Tauri call; the
 * editor only owns the controls, the wheel and the ±0.1 / ±1 ms nudge arithmetic.
 */
export function initGridEditor({ save, translate: translator = translate, document: doc = globalThis.document } = {}) {
  const dialog = doc?.querySelector('#grid-editor-modal');
  if (!dialog || typeof save !== 'function') return null;
  const t = (key, values) => translator(key, values);

  const elements = {
    context: dialog.querySelector('#grid-editor-context'),
    bpmValue: dialog.querySelector('#grid-editor-bpm-value'),
    bpmDown: dialog.querySelector('#grid-editor-bpm-down'),
    bpmUp: dialog.querySelector('#grid-editor-bpm-up'),
    bpmRange: dialog.querySelector('#grid-editor-bpm-range'),
    shiftValue: dialog.querySelector('#grid-editor-shift-value'),
    shiftRange: dialog.querySelector('#grid-editor-shift-range'),
    keySelect: dialog.querySelector('#grid-editor-key-select'),
    keyValue: dialog.querySelector('#grid-editor-key-value'),
    transposition: dialog.querySelector('#grid-editor-transposition'),
    wheel: dialog.querySelector('#grid-editor-wheel'),
    compatible: dialog.querySelector('#grid-editor-compatible'),
    reset: dialog.querySelector('#grid-editor-reset'),
    save: dialog.querySelector('#grid-editor-save'),
    close: dialog.querySelector('#close-grid-editor'),
  };

  const working = {
    track: null,
    bpm: 120,
    bpmChanged: false,
    shiftMs: 0,
    musicalKey: null,
    camelotKey: null,
  };

  // The wheel, the key list and the semitone transposition buttons are generated from data, so the
  // dialog markup stays free of 24 untranslatable option labels.
  if (elements.wheel && !elements.wheel.childElementCount) {
    for (const entry of CAMELOT_WHEEL) {
      const button = doc.createElement('button');
      button.type = 'button';
      button.className = 'camelot-key';
      button.dataset.camelot = entry.code;
      button.textContent = entry.code;
      button.title = entry.key;
      elements.wheel.append(button);
    }
  }
  if (elements.keySelect && !elements.keySelect.childElementCount) {
    for (const entry of CAMELOT_WHEEL) {
      const option = doc.createElement('option');
      option.value = entry.code;
      option.textContent = `${entry.code} · ${entry.key}`;
      elements.keySelect.append(option);
    }
  }
  if (elements.transposition && !elements.transposition.childElementCount) {
    for (const semitones of [-1, 1]) {
      const button = doc.createElement('button');
      button.type = 'button';
      button.className = 'camelot-transpose';
      button.dataset.semitones = String(semitones);
      button.textContent = semitones > 0 ? '+1' : '−1';
      button.title = t(semitones > 0 ? 'Einen Halbton höher' : 'Einen Halbton tiefer');
      elements.transposition.append(button);
    }
  }

  function selectedKey() {
    return working.musicalKey ?? null;
  }

  function render() {
    if (elements.bpmValue) elements.bpmValue.textContent = formatBpm(working.bpm);
    if (elements.bpmRange && String(elements.bpmRange.value) !== String(working.bpm)) {
      elements.bpmRange.value = String(working.bpm);
    }
    if (elements.shiftValue) elements.shiftValue.textContent = formatShiftMs(working.shiftMs);
    if (elements.shiftRange && String(elements.shiftRange.value) !== String(working.shiftMs)) {
      elements.shiftRange.value = String(working.shiftMs);
    }
    const key = selectedKey();
    if (elements.keyValue) {
      elements.keyValue.textContent = key
        ? `${working.camelotKey ?? '—'} · ${key}`
        : t('Keine Tonart analysiert');
    }
    if (elements.keySelect && working.camelotKey) elements.keySelect.value = working.camelotKey;
    for (const button of elements.wheel?.querySelectorAll('[data-camelot]') ?? []) {
      const active = button.dataset.camelot === working.camelotKey;
      button.classList.toggle('active', active);
      button.setAttribute('aria-pressed', String(active));
    }
    if (elements.compatible) {
      const codes = working.camelotKey ? compatibleCamelotCodes(working.camelotKey) : [];
      elements.compatible.replaceChildren(
        ...codes.map((code) => {
          const button = doc.createElement('button');
          button.type = 'button';
          button.className = 'camelot-key compatible';
          button.dataset.camelot = code;
          button.textContent = code;
          button.title = t('Kompatibel: {key}', { key: musicalKeyForCamelot(code) ?? code });
          return button;
        }),
      );
    }
    if (elements.context) {
      elements.context.textContent = working.track
        ? t('{title} · {bpm} BPM · gespeicherte Tonart {key}', {
            title: working.track.title,
            bpm: formatBpm(working.bpm),
            key: key ?? t('Keine Tonart analysiert'),
          })
        : '';
    }
  }

  function load(track) {
    working.track = track;
    working.bpm = initialBpm(track);
    working.bpmChanged = false;
    working.shiftMs = 0;
    const storedKey = track?.musicalKey ?? track?.musical_key ?? null;
    working.camelotKey = track?.camelotKey ?? camelotForMusicalKey(storedKey);
    working.musicalKey = working.camelotKey ? musicalKeyForCamelot(working.camelotKey) : storedKey;
    render();
  }

  function applyBpm(value) {
    const next = clampBpm(value);
    if (next === null) return;
    if (next !== working.bpm) working.bpmChanged = true;
    working.bpm = next;
    render();
  }

  function applyShift(value) {
    working.shiftMs = clampGridShiftMs(value);
    render();
  }

  function applyKey(camelotCode) {
    const key = musicalKeyForCamelot(camelotCode);
    if (!key) return;
    working.camelotKey = camelotCode;
    working.musicalKey = key;
    render();
  }

  elements.bpmDown?.addEventListener('click', () => applyBpm(nudgeBpm(working.bpm, -BPM_NUDGE)));
  elements.bpmUp?.addEventListener('click', () => applyBpm(nudgeBpm(working.bpm, BPM_NUDGE)));
  elements.bpmRange?.addEventListener('input', (event) => applyBpm(event.target.value));
  elements.shiftRange?.addEventListener('input', (event) => applyShift(event.target.value));
  elements.keySelect?.addEventListener('change', (event) => applyKey(event.target.value));
  dialog.addEventListener('click', (event) => {
    const wheelButton = event.target.closest?.('[data-camelot]');
    if (wheelButton) {
      applyKey(wheelButton.dataset.camelot);
      return;
    }
    const transposeButton = event.target.closest?.('[data-semitones]');
    if (transposeButton && working.camelotKey) {
      const transposed = transposeCamelot(working.camelotKey, Number(transposeButton.dataset.semitones));
      if (transposed) applyKey(transposed);
    }
  });
  const stepStrip = dialog.querySelector('#grid-editor-shift-steps') ?? elements.shiftRange?.parentElement;
  for (const step of GRID_SHIFT_STEPS_MS) {
    const button = doc.createElement('button');
    button.type = 'button';
    button.className = 'grid-shift-step';
    button.dataset.shiftMs = String(step);
    button.textContent = step > 0 ? `+${step}` : String(step);
    button.title = t('Grid um {ms} ms verschieben', { ms: step });
    button.addEventListener('click', () => applyShift(working.shiftMs + step));
    stepStrip?.append(button);
  }
  // The wheel, the shift steps and the transpose buttons carry generated titles, so they follow the
  // locale like the statically translated markup around them.
  globalThis.addEventListener?.('championvibe:locale-change', () => {
    for (const button of elements.transposition?.querySelectorAll('[data-semitones]') ?? []) {
      const up = Number(button.dataset.semitones) > 0;
      button.title = t(up ? 'Einen Halbton höher' : 'Einen Halbton tiefer');
    }
    for (const button of stepStrip?.querySelectorAll('[data-shift-ms]') ?? []) {
      button.title = t('Grid um {ms} ms verschieben', { ms: Number(button.dataset.shiftMs) });
    }
    render();
  });
  elements.reset?.addEventListener('click', () => load(working.track));
  elements.close?.addEventListener('click', () => {
    if (dialog.open) dialog.close();
  });
  elements.save?.addEventListener('click', async () => {
    if (!working.track || !elements.save) return;
    elements.save.disabled = true;
    try {
      const correction = buildCorrection({
        bpm: working.bpm,
        bpmChanged: working.bpmChanged,
        gridShiftMs: working.shiftMs,
        musicalKey: working.musicalKey,
        camelotKey: working.camelotKey,
      });
      await save(working.track, correction);
      if (dialog.open) dialog.close();
    } catch {
      // `save` already surfaced the failure (toast); the dialog stays open for another attempt.
    } finally {
      if (elements.save) elements.save.disabled = false;
    }
  });

  return {
    open(track) {
      load(track);
      if (!dialog.open) dialog.showModal();
    },
    close() {
      if (dialog.open) dialog.close();
    },
    isOpen: () => dialog.open,
    currentTrackId: () => working.track?.id ?? null,
    workingValues: () => ({ ...working }),
  };
}
