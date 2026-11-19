import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

const main = readFileSync(new URL('./main.js', import.meta.url), 'utf8');
const styles = readFileSync(new URL('./styles.css', import.meta.url), 'utf8');

test('the 50k-track library retains a bounded virtual row pool of at most 15 nodes', () => {
  assert.match(main, /const PAGE_SIZE = 200/u);
  assert.match(main, /const TRACK_ROW_HEIGHT = 66/u);
  assert.match(main, /Math\.min\(15, visibleCount \+ TRACK_OVERSCAN \* 2\)/u);
  assert.match(styles, /\.track-row\.virtual-track-row\s*\{[^}]*height:\s*66px/isu);
});

test('waveform peaks are requested only when playback starts', () => {
  assert.match(main, /async function loadWaveformOnPlay\(track\)/u);
  assert.match(main, /displayAudioStatus\(await invoke\('play_audio'\)\);\s*void loadWaveformOnPlay\(track\)/u);
  assert.doesNotMatch(main, /fetchTrackPage[\s\S]{0,1200}get_track_waveform_packet/u);
});
