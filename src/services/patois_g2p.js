// Lightweight, opt-in orthographic substitutions before the local English G2P pass.
// These practical spelling cues are deliberately limited; they are not a complete or
// authoritative Jamaican Creole grammar or a claim of authentic Patois speech.
export const PATOIS_G2P_RULES = Object.freeze([
  Object.freeze({ english: 'thing', patois: 'ting' }),
  Object.freeze({ english: 'them', patois: 'dem' }),
  Object.freeze({ english: 'brother', patois: 'bredda' }),
  Object.freeze({ english: 'three', patois: 'tree' }),
  Object.freeze({ english: 'girl', patois: 'gyal' }),
  Object.freeze({ english: 'boy', patois: 'bwoy' }),
  Object.freeze({ english: 'make', patois: 'mek' }),
  Object.freeze({ english: 'little', patois: 'likkle' }),
]);

const WORD_RULES = new Map(PATOIS_G2P_RULES.map(({ english, patois }) => [english, patois]));
const WORD_RULE_PATTERN = new RegExp(
  `\\b(${PATOIS_G2P_RULES.map(({ english }) => english).join('|')})\\b`,
  'giu',
);

const EMOTION_NAMES = Object.freeze([
  'energetic',
  'calm',
  'uplifting',
  'conscious',
  'confident',
  'aggressive',
]);
const EMOTION_ALIASES = new Map([
  ['hype', 'energetic'],
  ['easy', 'calm'],
  ['positive', 'uplifting'],
  ['serious', 'conscious'],
  ['neutral', 'confident'],
]);

// Vector layout: [target rate, semitone pitch offset, rising pitch-contour range in semitones].
// The Kokoro graph has a speed input but no native emotion/pitch input. Pitch and contour are
// applied after inference by a small local linear-interpolation varispeed pass.
export const PATOIS_PROSODY_VECTOR_WIDTH = 3;
export const PATOIS_PROSODY_FIELDS = Object.freeze({
  rate: 0,
  pitchSemitones: 1,
  risingContourSemitones: 2,
});
const MAX_LOCAL_AUDIO_SAMPLES = 24_000 * 120;

const EMOTION_PROFILES = Object.freeze({
  energetic: Object.freeze({
    vector: Object.freeze([1.08, 0.75, 0.8]),
    cue: (phrase) => `Yo! ${phrase}!`,
  }),
  calm: Object.freeze({
    vector: Object.freeze([0.94, -0.35, -0.15]),
    cue: (phrase) => `Easy now... ${phrase}`,
  }),
  uplifting: Object.freeze({
    vector: Object.freeze([1.02, 0.35, 0.55]),
    cue: (phrase) => `Rise up, family! ${phrase}`,
  }),
  conscious: Object.freeze({
    vector: Object.freeze([0.96, -0.15, 0.15]),
    cue: (phrase) => `Listen close... ${phrase}`,
  }),
  confident: Object.freeze({
    vector: Object.freeze([1, 0, 0]),
    cue: (phrase) => phrase,
  }),
  aggressive: Object.freeze({
    vector: Object.freeze([1.12, 1.5, 1.2]),
    cue: (phrase) => `Stand firm! ${phrase}!`,
  }),
});

function preserveWordCase(source, replacement) {
  if (source === source.toUpperCase()) return replacement.toUpperCase();
  if (source[0] === source[0].toUpperCase() && source.slice(1) === source.slice(1).toLowerCase()) {
    return replacement[0].toUpperCase() + replacement.slice(1);
  }
  return replacement;
}

/** Apply the listed word-level spelling rules without changing punctuation or surrounding text. */
export function transformPatoisText(text) {
  if (typeof text !== 'string') throw new TypeError('Patois G2P input must be a string.');
  return text.replace(WORD_RULE_PATTERN, (word) => {
    const replacement = WORD_RULES.get(word.toLowerCase());
    return replacement ? preserveWordCase(word, replacement) : word;
  });
}

export function normalizePatoisEmotion(emotion = 'confident') {
  const candidate = String(emotion).trim().toLowerCase();
  const normalized = EMOTION_ALIASES.get(candidate) || candidate || 'confident';
  if (!EMOTION_NAMES.includes(normalized)) {
    throw new Error(`Emotion must be ${EMOTION_NAMES.join(', ')}, or the alias hype.`);
  }
  return normalized;
}

/** A categorical vector retained for UI/diagnostics; Kokoro does not consume it as a latent. */
export function createPatoisEmotionVector(emotion = 'confident') {
  const normalized = normalizePatoisEmotion(emotion);
  const vector = new Float32Array(EMOTION_NAMES.length);
  vector[EMOTION_NAMES.indexOf(normalized)] = 1;
  return vector;
}

/** Return [rate, global pitch shift, phrase-final rising contour] for the selected cue. */
export function createPatoisProsodyVector(emotion = 'confident') {
  const normalized = normalizePatoisEmotion(emotion);
  return Float32Array.from(EMOTION_PROFILES[normalized].vector);
}

/**
 * Apply a bounded varispeed pitch contour locally. This is intentionally lightweight and can
 * alter timbre; it is not a formant-preserving pitch shifter or a model-native emotion control.
 */
export function applyPatoisProsody(samples, prosodyVector) {
  if (!(samples instanceof Float32Array)) {
    throw new TypeError('Patois prosody expects Float32Array PCM.');
  }
  if (prosodyVector?.length !== PATOIS_PROSODY_VECTOR_WIDTH
      || !Array.from(prosodyVector).every(Number.isFinite)) {
    throw new TypeError('Patois prosody vector must contain finite rate, pitch, and contour values.');
  }
  if (!samples.length) return samples;

  const targetRate = prosodyVector[PATOIS_PROSODY_FIELDS.rate];
  const pitchSemitones = prosodyVector[PATOIS_PROSODY_FIELDS.pitchSemitones];
  const contourSemitones = prosodyVector[PATOIS_PROSODY_FIELDS.risingContourSemitones];
  if (targetRate < 0.5 || targetRate > 2 || Math.abs(contourSemitones) > 4) {
    throw new RangeError('Patois rate or intonation is outside the safe local range.');
  }
  const pitchScale = 2 ** (pitchSemitones / 12);
  if (!Number.isFinite(pitchScale) || pitchScale < 0.5 || pitchScale > 2) {
    throw new RangeError('Patois pitch adjustment is outside the safe varispeed range.');
  }
  if (pitchSemitones === 0 && contourSemitones === 0) return samples;

  const outputLength = Math.max(1, Math.round(samples.length / pitchScale));
  if (outputLength > MAX_LOCAL_AUDIO_SAMPLES) {
    throw new RangeError('Patois pitch adjustment exceeds the 120-second local audio limit.');
  }
  const output = new Float32Array(outputLength);
  // Convert the desired end-to-start pitch ratio into a small, smooth linear time warp.
  const contourRatio = 2 ** (contourSemitones / 12);
  const bend = Math.max(-0.08, Math.min(0.08, (contourRatio - 1) / (contourRatio + 1)));
  const lastSourceIndex = samples.length - 1;
  const lastOutputIndex = Math.max(1, outputLength - 1);

  for (let index = 0; index < outputLength; index += 1) {
    const progress = index / lastOutputIndex;
    const warpedProgress = progress + bend * (progress * progress - progress);
    const sourcePosition = Math.max(0, Math.min(lastSourceIndex, warpedProgress * lastSourceIndex));
    const sourceIndex = Math.floor(sourcePosition);
    const nextIndex = Math.min(lastSourceIndex, sourceIndex + 1);
    const fraction = sourcePosition - sourceIndex;
    output[index] = samples[sourceIndex] + (samples[nextIndex] - samples[sourceIndex]) * fraction;
  }
  return output;
}

/** Build the cue, apply spelling rules, and return the non-model-native controls for diagnostics. */
export function buildPatoisPhrase({ text, preset = 'custom', djName = '', emotion = 'confident' } = {}) {
  const content = String(text ?? '').trim();
  const name = String(djName ?? '').trim();
  if (!content || [...content].length > 1_000 || /[\u0000-\u001f\u007f]/u.test(content)) {
    throw new Error('Shoutout text must contain 1–1000 printable characters.');
  }
  if ([...name].length > 80 || /[\u0000-\u001f\u007f]/u.test(name)) {
    throw new Error('DJ name must contain at most 80 printable characters.');
  }

  const normalizedEmotion = normalizePatoisEmotion(emotion);
  const profile = EMOTION_PROFILES[normalizedEmotion];
  const nameCall = name ? ` Big up ${name}!` : '';
  let phrase;
  switch (preset) {
    case 'custom':
      phrase = content;
      break;
    case 'selector':
      phrase = `Forward, selecta!${nameCall} ${content} Big tune, wheel and come again!`;
      break;
    case 'riddim':
      phrase = `Listen to di riddim!${nameCall} ${content} Hold tight, massive!`;
      break;
    case 'wheel':
      phrase = `Wheel and come again!${name ? ` ${name},` : ''} ${content}`;
      break;
    default:
      throw new Error('Unknown local shoutout preset.');
  }

  const transformedText = transformPatoisText(profile.cue(phrase));
  if ([...transformedText].length > 1_160) throw new Error('Expanded shoutout is too long.');
  return {
    text: transformedText,
    emotion: normalizedEmotion,
    emotionVector: createPatoisEmotionVector(normalizedEmotion),
    prosodyVector: createPatoisProsodyVector(normalizedEmotion),
  };
}
