import de from './locales/de.json' with { type: 'json' };
import en from './locales/en.json' with { type: 'json' };
import es from './locales/es.json' with { type: 'json' };
import fr from './locales/fr.json' with { type: 'json' };

export const SUPPORTED_LOCALES = Object.freeze(['de', 'en', 'es', 'fr']);
export const dictionaries = Object.freeze({ de, en, es, fr });
export const LANGUAGE_STORAGE_KEY = 'championvibe.locale';

export function resolveLocale(preferredLanguages = []) {
  for (const candidate of preferredLanguages) {
    if (typeof candidate !== 'string') continue;
    const locale = candidate.trim().toLowerCase().split(/[-_]/u, 1)[0];
    if (SUPPORTED_LOCALES.includes(locale)) return locale;
  }
  return 'en';
}

function storedLocale() {
  try {
    const value = globalThis.localStorage?.getItem(LANGUAGE_STORAGE_KEY);
    return value && SUPPORTED_LOCALES.includes(value) ? value : null;
  } catch {
    return null;
  }
}

const browserLanguages = globalThis.navigator?.languages?.length
  ? globalThis.navigator.languages
  : [globalThis.navigator?.language];
let currentLocale = storedLocale() ?? resolveLocale(browserLanguages);
const textSource = new WeakMap();
const attributeSource = new WeakMap();

export function getLocale() {
  return currentLocale;
}

export function t(key, values = undefined) {
  const source = String(key ?? '');
  const dictionary = dictionaries[currentLocale];
  const template = dictionary[source] ?? dictionaries.en[source] ?? source;
  if (!values) return template;
  return template.replace(/\{([\w.-]+)\}/gu, (match, name) => {
    const value = values[name];
    return value === undefined || value === null ? match : String(value);
  });
}

function isDictionaryKey(value) {
  return Object.hasOwn(dictionaries.de, value) || Object.hasOwn(dictionaries.en, value);
}

function translatedText(value, source) {
  return `${source.leading}${t(source.key)}${source.trailing}`;
}

export function applyStaticTranslations(root = globalThis.document) {
  if (!root?.createTreeWalker && !root?.ownerDocument?.createTreeWalker) return;
  const doc = root.nodeType === 9 ? root : root.ownerDocument;
  const showText = doc?.defaultView?.NodeFilter?.SHOW_TEXT ?? 4;
  const walker = doc.createTreeWalker(root, showText);
  let node;
  while ((node = walker.nextNode())) {
    let source = textSource.get(node);
    if (!source) {
      const value = node.nodeValue ?? '';
      const match = /^(\s*)([\s\S]*?)(\s*)$/u.exec(value);
      const key = (match?.[2] ?? '').replace(/\s+/gu, ' ').trim();
      if (!key || !isDictionaryKey(key)) continue;
      source = { key, leading: match[1], trailing: match[3] };
      textSource.set(node, source);
    }
    node.nodeValue = translatedText(node.nodeValue, source);
  }

  const attributes = ['title', 'aria-label', 'placeholder'];
  const elements = [];
  if (root.nodeType === 1) elements.push(root);
  elements.push(...root.querySelectorAll?.('*') ?? []);
  for (const element of elements) {
    let sources = attributeSource.get(element);
    if (!sources) {
      sources = new Map();
      attributeSource.set(element, sources);
    }
    for (const attribute of attributes) {
      const source = sources.get(attribute);
      if (source) {
        element.setAttribute(attribute, t(source));
        continue;
      }
      const value = element.getAttribute(attribute);
      if (value && isDictionaryKey(value)) {
        sources.set(attribute, value);
        element.setAttribute(attribute, t(value));
      }
    }
  }
}

export function setLocale(locale) {
  const normalized = String(locale ?? '').trim().toLowerCase().split(/[-_]/u, 1)[0];
  if (!SUPPORTED_LOCALES.includes(normalized) || normalized === currentLocale) return false;
  currentLocale = normalized;
  try {
    globalThis.localStorage?.setItem(LANGUAGE_STORAGE_KEY, currentLocale);
  } catch {
    // Language switching still works for this session when storage is unavailable.
  }
  if (globalThis.document) {
    document.documentElement.lang = currentLocale;
    applyStaticTranslations(document);
    const picker = document.querySelector('#language-select');
    if (picker) picker.value = currentLocale;
  }
  if (globalThis.window && typeof globalThis.CustomEvent === 'function') {
    window.dispatchEvent(new CustomEvent('championvibe:locale-change', { detail: { locale: currentLocale } }));
  }
  return true;
}

if (globalThis.document) {
  document.documentElement.lang = currentLocale;
}
