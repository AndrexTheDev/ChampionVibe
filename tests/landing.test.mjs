import assert from 'node:assert/strict';
import { existsSync, readFileSync } from 'node:fs';
import test from 'node:test';

const landing = new URL('../landing/', import.meta.url);
const html = readFileSync(new URL('index.html', landing), 'utf8');
const css = readFileSync(new URL('style.css', landing), 'utf8');

test('the English landing page has a destination for every internal link', () => {
  assert.match(html, /<html lang="en">/u);
  const ids = new Set(Array.from(html.matchAll(/\bid="([^"]+)"/gu), ([, id]) => id));
  const links = Array.from(html.matchAll(/\bhref="#([^"]+)"/gu), ([, href]) => href);
  assert.ok(links.length > 0);
  for (const href of links) assert.ok(ids.has(href), `Missing section #${href}`);
});

test('the landing page serves its own fonts and images and links to official releases', () => {
  const assets = Array.from(`${html}\n${css}`.matchAll(/"(\/(?:media|fonts)\/[^'"]+)"|'(\/(?:media|fonts)\/[^'"]+)'/gu), (match) => match[1] ?? match[2]);
  assert.ok(assets.length >= 5);
  for (const asset of assets) assert.ok(existsSync(new URL(`public${asset}`, landing)), `Missing asset ${asset}`);
  assert.match(html, /https:\/\/github\.com\/AndrexTheDev\/ChampionVibe\/releases/u);
  assert.doesNotMatch(html, /<script[^>]+src="https?:/u);
});
