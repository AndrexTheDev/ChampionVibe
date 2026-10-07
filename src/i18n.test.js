import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import {
  dictionaries,
  getLocale,
  resolveLocale,
  setLocale,
  SUPPORTED_LOCALES,
  t,
} from './i18n.js';

test('ships complete, key-aligned static dictionaries for all supported locales', () => {
  assert.deepEqual(SUPPORTED_LOCALES, ['de', 'en', 'es', 'fr']);
  const keys = Object.keys(dictionaries.de).sort();
  for (const locale of SUPPORTED_LOCALES) {
    assert.deepEqual(Object.keys(dictionaries[locale]).sort(), keys, `${locale} dictionary key parity`);
  }
});

test('covers every static HTML text node and translatable attribute in all locales', () => {
  const html = readFileSync(new URL('../index.html', import.meta.url), 'utf8');
  const keys = new Set();
  const decodeEntities = (value) => value
    .replace(/&amp;/gu, '&')
    .replace(/&lt;/gu, '<')
    .replace(/&gt;/gu, '>')
    .replace(/&quot;/gu, '"')
    .replace(/&#39;|&apos;/gu, "'")
    .replace(/&nbsp;/gu, ' ');
  const addKey = (value) => {
    const key = decodeEntities(value).replace(/\s+/gu, ' ').trim();
    if (key) keys.add(key);
  };
  for (const [, value] of html.matchAll(/(?:placeholder|title|aria-label)="([^"]*)"/gu)) addKey(value);
  for (const [, value] of html.matchAll(/>([^<>]+)</gu)) addKey(value);

  for (const locale of SUPPORTED_LOCALES) {
    const missing = [...keys].filter((key) => !Object.hasOwn(dictionaries[locale], key));
    assert.deepEqual(missing, [], `${locale} static HTML translation coverage`);
  }
});

test('all literal runtime translation and toast keys exist in every dictionary', () => {
  const sources = ['./main.js', './ui/grid_editor.js'].map((path) =>
    readFileSync(new URL(path, import.meta.url), 'utf8'),
  );
  const keys = new Set();
  for (const source of sources) {
    for (const [, , key] of source.matchAll(/\bt\(\s*(['"])(.*?)\1/gu)) if (key.length > 1) keys.add(key);
    for (const [, , key] of source.matchAll(/\btoast\(\s*(['"])(.*?)\1/gu)) if (key.length > 1) keys.add(key);
  }
  for (const locale of SUPPORTED_LOCALES) {
    const missing = [...keys].filter((key) => !Object.hasOwn(dictionaries[locale], key));
    assert.deepEqual(missing, [], `${locale} runtime translation coverage`);
  }
});

test('keeps the Auto-Gain, search, analysis-badge, Pitch-Lock and grid-editor copy in all four locales', () => {
  const required = [
    'Auto-Gain',
    'R128 target',
    'Target is applied on both decks with an 8 ms gain ramp. The player shows the estimated per-track offset.',
    'Pitch-Lock / WSOLA',
    'Pitch-locked WSOLA',
    'Varispeed · pitch follows tempo',
    'Mood, Rhythmus, Key, BPM oder LUFS suchen …',
    'Local filters: {filters}',
    'Mood: {value}',
    'Rhythm: {value}',
    'Sad',
    'Melancholic',
    'Dembow',
    'One Drop',
    'Clave',
    'Boom Bap',
    'Grid gespeichert: {bpm} BPM · {key}',
    'Grid gespeichert · live übernommen',
    'Grid-Korrektur fehlgeschlagen: {error}',
    'Grid-Editor ist nur in der Tauri-App verfügbar.',
    'Grid & Tonart korrigieren: {title}',
    'Konfidenz {percent} %',
    'Verifiziert',
    'Nicht analysiert',
    'Keine Tonart analysiert',
    'Kompatibel: {key}',
    'Grid um {ms} ms verschieben',
    'Einen Halbton höher',
    'Einen Halbton tiefer',
    'Heuristik unsicher · BPM {bpm} % · Tonart {key} % · bitte prüfen',
  ];
  for (const locale of SUPPORTED_LOCALES) {
    const missing = required.filter((key) => !Object.hasOwn(dictionaries[locale], key));
    assert.deepEqual(missing, [], `${locale} feature translation coverage`);
  }
});

test('detects the first supported browser-language tag and falls back to English', () => {
  assert.equal(resolveLocale(['es-ES', 'en-US']), 'es');
  assert.equal(resolveLocale(['pt-BR', 'fr-FR']), 'fr');
  assert.equal(resolveLocale(['pt-BR', 'it-IT']), 'en');
});

test('switches locale synchronously and interpolates dynamic labels', () => {
  const initialLocale = getLocale();
  try {
    assert.equal(setLocale('fr'), true);
    assert.equal(getLocale(), 'fr');
    assert.equal(t('Wähle einen Track aus deiner Bibliothek.'), 'Choisissez un morceau dans votre bibliothèque.');
    assert.equal(t('{count} Tracks', { count: 15 }), '15 morceaux');
    assert.equal(setLocale('xx'), false);
  } finally {
    setLocale(initialLocale);
  }
});
