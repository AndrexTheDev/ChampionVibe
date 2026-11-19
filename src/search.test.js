import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { matchesSemanticTrackFilters, parseSemanticTrackQuery } from './search.js';

test('parses mood, rhythm, and tempo from a natural DJ search', () => {
  assert.deepEqual(parseSemanticTrackQuery('sad dembow 95 bpm'), {
    text: '',
    filters: {
      bpmMin: 92,
      bpmMax: 98,
      rhythm: 'Dembow',
      mood: 'sad',
    },
  });
});

test('parses key and LUFS filters while preserving unmatched title or artist terms', () => {
  assert.deepEqual(parseSemanticTrackQuery('soul D minor −14 LUFS'), {
    text: 'soul',
    filters: {
      lufsMin: -15.5,
      lufsMax: -12.5,
      key: 'D minor',
    },
  });
  assert.deepEqual(parseSemanticTrackQuery('BPM 100-104 C# minor'), {
    text: '',
    filters: {
      bpmMin: 100,
      bpmMax: 104,
      key: 'C# minor',
    },
  });
});

test('recognizes localized mood words and relative loudness cues', () => {
  assert.deepEqual(parseSemanticTrackQuery('triste one drop'), {
    text: '',
    filters: { rhythm: 'One Drop', mood: 'sad' },
  });
  assert.deepEqual(parseSemanticTrackQuery('loud happy'), {
    text: '',
    filters: { lufsMin: -12, mood: 'positive' },
  });
  assert.deepEqual(parseSemanticTrackQuery('quiet Dembow'), {
    text: '',
    filters: { lufsMax: -20, rhythm: 'Dembow' },
  });
});

test('semantic filters match every requested metadata dimension and reject mismatches', () => {
  const tracks = [
    {
      title: 'Late Dancehall',
      bpm: 95,
      musicalKey: 'D minor',
      lufsIntegrated: -14.2,
      moodValence: -0.55,
      moodArousal: 0.31,
      moodLabel: 'Melancholic',
      rhythmPattern: 'Dembow',
    },
    {
      title: 'Bright Roots',
      bpm: 118,
      musical_key: 'C major',
      lufs_integrated: -11,
      mood_valence: 0.71,
      mood_arousal: 0.77,
      mood_label: 'Euphoric',
      rhythm_pattern: 'One Drop',
    },
  ];
  const filters = parseSemanticTrackQuery('sad dembow 95 bpm').filters;
  assert.deepEqual(tracks.filter((track) => matchesSemanticTrackFilters(track, filters)).map((track) => track.title), ['Late Dancehall']);
  assert.equal(matchesSemanticTrackFilters(tracks[0], { key: 'D minor', lufsMax: -15 }), false);
  assert.equal(matchesSemanticTrackFilters(tracks[1], { mood: 'positive', rhythm: 'One Drop' }), true);
  assert.equal(matchesSemanticTrackFilters({ moodLabel: 'Chill' }, { mood: 'calm' }), true);
});

test('parsed frontend filters are sent to the native paged SQLite query', () => {
  const main = readFileSync(new URL('./main.js', import.meta.url), 'utf8');
  const commands = readFileSync(new URL('../src-tauri/src/commands.rs', import.meta.url), 'utf8');
  const database = readFileSync(new URL('../src-tauri/src/database.rs', import.meta.url), 'utf8');
  assert.match(main, /filters: state\.trackSearch\.filters/u);
  assert.match(commands, /filters: Option<database::TrackSearchFilters>/u);
  for (const field of ['mood', 'rhythm', 'key', 'bpm_min', 'bpm_max', 'lufs_min', 'lufs_max']) {
    assert.ok(database.includes(`pub ${field}: Option<`), `${field} filter is exposed to SQLite`);
  }
  assert.match(database, /ta\.mood_valence <= -0\.22/u);
  assert.match(database, /ta\.rhythm_pattern LIKE/u);
  assert.match(database, /a\.musical_key LIKE/u);
  assert.match(database, /b\.bpm >= \?6/u);
  assert.match(database, /ta\.lufs_integrated >= \?8/u);
});
