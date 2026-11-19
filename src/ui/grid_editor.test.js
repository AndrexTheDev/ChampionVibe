import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dictionaries, SUPPORTED_LOCALES } from '../i18n.js';
import {
  applyCorrectionToTrack,
  BPM_NUDGE,
  buildCorrection,
  camelotForMusicalKey,
  CAMELOT_WHEEL,
  clampBpm,
  clampGridShiftMs,
  compatibleCamelotCodes,
  confidenceBadge,
  confidenceBand,
  confidencePercent,
  CONFIDENCE_THRESHOLD,
  formatBpm,
  formatShiftMs,
  nearestBeatIndex,
  nudgeBpm,
  replaceTrackInPages,
  respaceBeats,
  shiftBeats,
  transposeCamelot,
  trackIsVerified,
  GRID_SHIFT_STEPS_MS,
  MAX_GRID_SHIFT_MS,
  musicalKeyForCamelot,
} from './grid_editor.js';

const editorSource = readFileSync(new URL('./grid_editor.js', import.meta.url), 'utf8');

test('mirrors the Rust Camelot wheel and stays in step with the Rust tests', () => {
  assert.equal(CAMELOT_WHEEL.length, 24);
  assert.equal(new Set(CAMELOT_WHEEL.map((entry) => entry.code)).size, 24);
  // The same four cases asserted by `camelot.rs` in the Rust test suite.
  assert.equal(camelotForMusicalKey('A minor'), '8A');
  assert.equal(camelotForMusicalKey('C major'), '8B');
  assert.equal(camelotForMusicalKey('C# minor'), '12A');
  assert.equal(camelotForMusicalKey('Bb major'), '6B');
  // Enharmonic and unicode spellings must land on the same wheel position.
  assert.equal(camelotForMusicalKey('B♭ major'), '6B');
  assert.equal(camelotForMusicalKey('A♯ minor'), '3A');
  assert.equal(camelotForMusicalKey('a MINOR'), '8A');
  assert.equal(musicalKeyForCamelot('8a'), 'A minor');
  assert.equal(camelotForMusicalKey('H minor'), null);
  assert.equal(camelotForMusicalKey(''), null);
  assert.equal(musicalKeyForCamelot('13A'), null);
});

test('every wheel entry round-trips between Camelot code and musical key', () => {
  for (const entry of CAMELOT_WHEEL) {
    assert.equal(camelotForMusicalKey(entry.key), entry.code, `${entry.key} → ${entry.code}`);
    assert.equal(musicalKeyForCamelot(entry.code), entry.key, `${entry.code} → ${entry.key}`);
  }
});

test('reports the same harmonically compatible positions as the Rust wheel', () => {
  assert.deepEqual(compatibleCamelotCodes('8A'), ['8B', '9A', '7A']);
  assert.deepEqual(compatibleCamelotCodes('12B'), ['12A', '1B', '11B']);
  assert.deepEqual(compatibleCamelotCodes('1a'), ['1B', '2A', '12A']);
  assert.deepEqual(compatibleCamelotCodes('nope'), []);
});

test('transposes inside the Camelot wheel without leaving the mode', () => {
  assert.equal(transposeCamelot('8A', 1), '3A');
  assert.equal(transposeCamelot('8A', -1), '1A');
  assert.equal(transposeCamelot('8B', 5), '7B');
  assert.equal(transposeCamelot('8A', 12), '8A');
  assert.equal(transposeCamelot('8A', -13), '1A');
  assert.equal(transposeCamelot('bad', 1), null);
});

test('nudges the BPM in 0.1 steps and clamps into the correction range', () => {
  assert.equal(BPM_NUDGE, 0.1);
  assert.equal(nudgeBpm(122), 122.1);
  assert.equal(nudgeBpm(122, -BPM_NUDGE), 121.9);
  assert.equal(nudgeBpm(121.9, BPM_NUDGE), 122);
  assert.equal(nudgeBpm(29.95, -1), 30);
  assert.equal(nudgeBpm(300, 1), 300);
  assert.equal(nudgeBpm(0), 120.1, 'an unanalysed row falls back to the 120 BPM default');
  assert.equal(nudgeBpm(null), 120.1);
  assert.equal(nudgeBpm(120.04), 120.1);
  assert.equal(nudgeBpm('nope'), null);
  assert.equal(clampBpm(Number.NaN), null);
  assert.equal(clampBpm(121.849), 121.8);
  assert.equal(formatBpm(122), '122.0');
  assert.equal(formatBpm(null), '—');
});

test('re-spaces the grid around a fixed downbeat when the BPM changes', () => {
  const beats = [0.25, 0.75, 1.25, 1.75];
  const respaced = respaceBeats(beats, 120);
  assert.equal(respaced.length, beats.length);
  assert.equal(respaced[0], 0.25, 'the downbeat keeps its phase');
  for (let index = 1; index < respaced.length; index += 1) {
    assert.ok(Math.abs(respaced[index] - (0.25 + index * 0.5)) < 1e-9, `beat ${index}`);
  }
  const doubled = respaceBeats(beats, 240);
  assert.ok(Math.abs(doubled[1] - 0.5) < 1e-9);
  assert.deepEqual(respaceBeats([], 120), []);
  assert.deepEqual(respaceBeats(beats, Number.NaN), beats);
  assert.deepEqual(respaceBeats([0.25, Number.NaN, 1.25], 120).length, 2, 'non-finite beats are dropped');
});

test('shifts the grid in milliseconds and never produces negative timestamps', () => {
  assert.deepEqual(shiftBeats([1, 2, 3], 10), [1.01, 2.01, 3.01]);
  assert.deepEqual(shiftBeats([1, 2, 3], -10), [0.99, 1.99, 2.99]);
  assert.deepEqual(shiftBeats([0.005, 1], -10), [0.99]);
  assert.deepEqual(shiftBeats([1], 0), [1]);
  assert.equal(clampGridShiftMs(50_000), MAX_GRID_SHIFT_MS);
  assert.equal(clampGridShiftMs(-50_000), -MAX_GRID_SHIFT_MS);
  assert.equal(clampGridShiftMs(Number.NaN), 0);
  assert.equal(formatShiftMs(12.34), '+12 ms');
  assert.equal(formatShiftMs(-4), '-4 ms');
});

test('exposes the four ±ms macro steps and the nearest beat for the grid overlay', () => {
  assert.deepEqual([...GRID_SHIFT_STEPS_MS], [-10, -1, 1, 10]);
  assert.equal(nearestBeatIndex([0, 0.5, 1, 1.5], 0.9), 2);
  assert.equal(nearestBeatIndex([0, 0.5], -3), 0);
  assert.equal(nearestBeatIndex([], 1), -1);
  assert.equal(nearestBeatIndex([1, 2], Number.NaN), -1);
});

test('classifies rows into the green, yellow-orange and unknown confidence bands', () => {
  assert.equal(CONFIDENCE_THRESHOLD, 0.7);
  assert.equal(confidenceBand({ bpm: 122, bpmConfidence: 0.91, keyConfidence: 0.8 }), 'trusted');
  assert.equal(confidenceBand({ bpm: 122, bpmConfidence: 0.69, keyConfidence: 0.9 }), 'heuristic');
  assert.equal(
    confidenceBand({ bpm: 122, bpmConfidence: 0.9, keyConfidence: 0.2, musicalKey: 'A minor' }),
    'heuristic',
    'a weak key drags the badge down when a key was stored',
  );
  assert.equal(confidenceBand({ bpm: 122, bpmConfidence: 0.9, isUserVerified: true }), 'verified');
  assert.equal(confidenceBand({ bpm: 0 }), 'unknown');
  assert.equal(confidenceBand({}), 'unknown');
  // Camel-case rows (Tauri payloads) and snake_case rows (raw SQLite dumps) agree.
  assert.equal(confidenceBand({ bpm: 122, bpm_confidence: 0.4, key_confidence: 0.4 }), 'heuristic');
  assert.equal(trackIsVerified({ is_user_verified: 1 }), true);
  assert.equal(confidencePercent({ bpm: 122, bpmConfidence: 0.912 }), 91);
  assert.equal(confidencePercent({ bpm: 122 }), 0, 'a row without any confidence reads as 0 %');
  assert.equal(confidencePercent({ bpm: 0 }), null);
  // A row without a key only weighs the BPM confidence.
  assert.equal(confidenceBand({ bpm: 122, bpmConfidence: 0.95, keyConfidence: 0 }), 'trusted');
  assert.equal(confidencePercent({ bpm: 122, bpmConfidence: 0.95, keyConfidence: 0 }), 95);
});

test('badge and tooltip copy are real translations in all four locales', () => {
  const rows = [
    { bpm: 122, bpmConfidence: 0.95, keyConfidence: 0.9 },
    { bpm: 122, bpmConfidence: 0.4, keyConfidence: 0.3, musicalKey: 'A minor' },
    { bpm: 122, bpmConfidence: 0.8, isUserVerified: true },
    { bpm: 0 },
  ];
  const keys = new Set();
  for (const row of rows) keys.add(confidenceBadge(row).key);
  for (const row of rows) keys.add(readTooltipKey(row));
  for (const key of keys) {
    for (const locale of SUPPORTED_LOCALES) {
      assert.ok(Object.hasOwn(dictionaries[locale], key), `${locale} is missing ${JSON.stringify(key)}`);
    }
  }
});

// `confidenceTooltip` is not exported; both badges are produced by the same bands, so the test
// reads the tooltip keys straight out of the module source instead of duplicating the table.
function readTooltipKey(row) {
  const source = readFileSync(new URL('./grid_editor.js', import.meta.url), 'utf8');
  const body = source.slice(source.indexOf('export function confidenceTooltip'));
  const keys = [...body.matchAll(/key: '([^']+)'/gu)].map(([, key]) => key);
  const band = confidenceBadge(row).tone;
  const index = { unknown: 0, verified: 1, trusted: 2, heuristic: 3 }[band];
  return keys[index];
}

test('builds a correction payload that only carries the values the user touched', () => {
  assert.deepEqual(buildCorrection({ bpm: 122.1, bpmChanged: true, gridShiftMs: 0 }), { bpm: 122.1 });
  // Shifts are whole milliseconds, so a fractional nudge is rounded before it is sent.
  assert.deepEqual(buildCorrection({ bpm: 122, bpmChanged: false, gridShiftMs: 25.4 }), {
    gridShiftMs: 25,
  });
  assert.deepEqual(
    buildCorrection({ bpm: 122.1, bpmChanged: true, gridShiftMs: -10, musicalKey: 'A minor', camelotKey: '8A' }),
    { bpm: 122.1, gridShiftMs: -10, musicalKey: 'A minor', camelotKey: '8A' },
  );
  assert.deepEqual(buildCorrection({ bpmChanged: false, gridShiftMs: 5000 }), {
    gridShiftMs: MAX_GRID_SHIFT_MS,
  });
  assert.deepEqual(buildCorrection({ bpm: Number.NaN, bpmChanged: true, gridShiftMs: 0 }), {});
});

test('merges a correction outcome into the cached row without mutating it', () => {
  const track = Object.freeze({ id: 7, bpm: 100, bpmConfidence: 0.3, musicalKey: 'C major', title: 'Dub' });
  const patched = applyCorrectionToTrack(track, {
    bpm: 122.25,
    bpmConfidence: 1,
    keyConfidence: 1,
    isUserVerified: true,
    musicalKey: 'A minor',
  });
  assert.notEqual(patched, track);
  assert.equal(track.bpm, 100, 'the frozen row is untouched');
  assert.equal(patched.bpm, 122.25);
  assert.equal(patched.bpmConfidence, 1);
  assert.equal(patched.keyConfidence, 1);
  assert.equal(patched.isUserVerified, true);
  assert.equal(patched.title, 'Dub');
  assert.equal(confidenceBand(patched), 'verified');
  assert.equal(patched.camelotKey, '8A');
  assert.equal(applyCorrectionToTrack(track, null), track);
});

test('rewrites the corrected track inside every cached page that holds it', () => {
  const pages = new Map([
    [0, [{ id: 1 }, { id: 7, bpm: 100 }]],
    [1, [{ id: 8 }]],
    [2, [{ id: 7, bpm: 100 }]],
  ]);
  const patched = { id: 7, bpm: 122.1 };
  assert.equal(replaceTrackInPages(pages, 7, patched), 2);
  assert.equal(pages.get(0)[1], patched);
  assert.equal(pages.get(2)[0], patched);
  assert.equal(pages.get(1)[0].id, 8);
  assert.equal(replaceTrackInPages(pages, 99, patched), 0);
  assert.equal(replaceTrackInPages(null, 7, patched), 0);
});

test('uses only statically translatable copy inside the editor module', () => {
  const literals = new Set();
  for (const [, , raw] of editorSource.matchAll(/\bt\(\s*(['"])(.*?)\1/gu)) literals.add(raw);
  // Ternary copy (`t(flag ? 'A' : 'B')`) and labels built by the dialog are not covered by the
  // runtime scan above, so the whole module is scanned for German source strings instead.
  for (const [, raw] of editorSource.matchAll(/(['"])((?:[^'"\\]|\\.)*)\1/gu)) {
    if (/[äöüßÄÖÜ]|\b(?:BPM|Grid|Tonart|Camelot|Halbton|Konfidenz|Heuristik|verifiziert|analysiert|Kompatibel)\b/u.test(raw)) {
      literals.add(raw);
    }
  }
  const missing = [...literals].filter(
    (key) => key.length > 1 && !/^[a-z-]+$/u.test(key) && !/^(de|en|es|fr)$/u.test(key),
  );
  for (const key of missing) {
    for (const locale of SUPPORTED_LOCALES) {
      assert.ok(Object.hasOwn(dictionaries[locale], key), `${locale} is missing ${JSON.stringify(key)}`);
    }
  }
});
