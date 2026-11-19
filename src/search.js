const MOOD_PATTERNS = Object.freeze([
  { pattern: /(?:^|[^\p{L}\p{N}])(?:sad|sadness|melancholic|melancholy|mélancolique|melancolique|traurig|trist(?:e|esse)?|tristeza)(?=$|[^\p{L}\p{N}])/iu, mood: 'sad' },
  { pattern: /(?:^|[^\p{L}\p{N}])(?:happy|happiness|positive|joyful|alegre|feliz|glücklich|gluecklich|joyeux|joyeuse|heureux|heureuse)(?=$|[^\p{L}\p{N}])/iu, mood: 'positive' },
  { pattern: /(?:^|[^\p{L}\p{N}])(?:calm|chill|relaxed|ruhig|gelassen|calme|tranquille|tranquilo|tranquila)(?=$|[^\p{L}\p{N}])/iu, mood: 'calm' },
  { pattern: /(?:^|[^\p{L}\p{N}])(?:energetic|energy|high[- ]energy|hype|energisch|energiegeladen|énergique|energique|enérgico|energico)(?=$|[^\p{L}\p{N}])/iu, mood: 'energetic' },
  { pattern: /(?:^|[^\p{L}\p{N}])(?:aggressive|aggressiv|agresivo|agresiva|agressif|agressive)(?=$|[^\p{L}\p{N}])/iu, mood: 'aggressive' },
  { pattern: /(?:^|[^\p{L}\p{N}])(?:tense|angespannt|tenso|tensa|tendu|tendue)(?=$|[^\p{L}\p{N}])/iu, mood: 'tense' },
  { pattern: /(?:^|[^\p{L}\p{N}])(?:uplifting|inspiring|aufbauend|inspirador|inspiradora|inspirant|inspirante)(?=$|[^\p{L}\p{N}])/iu, mood: 'uplifting' },
  { pattern: /(?:^|[^\p{L}\p{N}])(?:euphoric|euphorisch|eufórico|euforico|eufórica|euforica|euphorique)(?=$|[^\p{L}\p{N}])/iu, mood: 'euphoric' },
  { pattern: /(?:^|[^\p{L}\p{N}])(?:balanced|ausgeglichen|equilibrado|equilibrada|équilibré|equilibree|équilibrée)(?=$|[^\p{L}\p{N}])/iu, mood: 'balanced' },
]);

const RHYTHM_PATTERNS = Object.freeze([
  { pattern: /\b(?:one[ -]drop)\b/iu, rhythm: 'One Drop' },
  { pattern: /\b(?:boom[ -]bap)\b/iu, rhythm: 'Boom Bap' },
  { pattern: /\b(?:steppers?)\b/iu, rhythm: 'Steppers' },
  { pattern: /\b(?:dembow)\b/iu, rhythm: 'Dembow' },
  { pattern: /\b(?:clave)(?:\s*3\s*[:/]\s*2)?\b/iu, rhythm: 'Clave' },
]);

function consumeFirst(text, pattern) {
  const match = pattern.exec(text);
  if (!match) return { text, match: null };
  return {
    text: `${text.slice(0, match.index)} ${text.slice(match.index + match[0].length)}`,
    match,
  };
}

function optionalNumber(value) {
  if (value === undefined || value === null || value === '') return undefined;
  const number = Number(value);
  return Number.isFinite(number) ? number : undefined;
}

function trackValue(track, camel, snake) {
  return track?.[camel] ?? track?.[snake];
}

function trackNumber(track, camel, snake) {
  return optionalNumber(trackValue(track, camel, snake));
}

function normalizedLabel(value) {
  return String(value || '').trim().toLowerCase();
}

function matchesMood(track, mood) {
  const label = normalizedLabel(trackValue(track, 'moodLabel', 'mood_label'));
  const valence = trackNumber(track, 'moodValence', 'mood_valence');
  const arousal = trackNumber(track, 'moodArousal', 'mood_arousal');
  switch (mood) {
    case 'sad':
      return (valence !== undefined && valence <= -0.22) || ['melancholic', 'tense'].includes(label);
    case 'positive':
      return (valence !== undefined && valence >= 0.22) || ['uplifting', 'euphoric'].includes(label);
    case 'calm':
      return ['calm', 'chill'].includes(label);
    case 'energetic':
      return (arousal !== undefined && arousal >= 0.62) || ['high energy', 'euphoric', 'aggressive'].includes(label);
    case 'aggressive':
      return label === 'aggressive' || (valence !== undefined && valence <= -0.28 && arousal !== undefined && arousal >= 0.62);
    case 'tense':
      return label === 'tense' || (valence !== undefined && valence <= -0.32 && arousal !== undefined && arousal > 0.38);
    case 'uplifting':
      return ['uplifting', 'euphoric'].includes(label) || (valence !== undefined && valence >= 0.35);
    case 'euphoric':
      return label === 'euphoric';
    case 'balanced':
      return label === 'balanced';
    default:
      return label.includes(normalizedLabel(mood));
  }
}

/** Parse natural-language DJ library queries into an indexed text term and native SQL filters. */
export function parseSemanticTrackQuery(input = '') {
  let remaining = String(input ?? '').slice(0, 256);
  const filters = {};

  let parsed = consumeFirst(remaining, /\b(?:bpm\s*[:=]?\s*(\d{2,3})(?:\s*(?:-|–|to)\s*(\d{2,3}))?|((?:\d{2,3}))(?:\s*(?:-|–|to)\s*(\d{2,3}))?\s*bpm)\b/iu);
  if (parsed.match) {
    const first = Number(parsed.match[1] ?? parsed.match[3]);
    const second = optionalNumber(parsed.match[2] ?? parsed.match[4]);
    const low = second === undefined ? first - 3 : Math.min(first, second);
    const high = second === undefined ? first + 3 : Math.max(first, second);
    if (low >= 40 && high <= 260) {
      filters.bpmMin = low;
      filters.bpmMax = high;
      remaining = parsed.text;
    }
  }

  parsed = consumeFirst(remaining, /(?<![\p{L}\p{N}])(?:lufs?\s*[:=]?\s*([-−]?\d{1,2}(?:\.\d+)?)|([-−]?\d{1,2}(?:\.\d+)?)\s*lufs?)(?![\p{L}\p{N}])/iu);
  if (parsed.match) {
    const value = Number((parsed.match[1] ?? parsed.match[2]).replace('−', '-'));
    if (value >= -70 && value <= 10) {
      filters.lufsMin = Math.max(-70, value - 1.5);
      filters.lufsMax = Math.min(10, value + 1.5);
      remaining = parsed.text;
    }
  }

  parsed = consumeFirst(remaining, /\b([A-G])([#b])?\s*(major|maj|minor|min|m)\b/iu);
  if (parsed.match) {
    const mode = /^(?:maj|major)$/iu.test(parsed.match[3]) ? 'major' : 'minor';
    filters.key = `${parsed.match[1].toUpperCase()}${parsed.match[2] || ''} ${mode}`;
    remaining = parsed.text;
  }

  parsed = consumeFirst(remaining, /\b(?:loud|louder|loudness|lauter|forte|haut|alto)\b/iu);
  if (parsed.match) {
    filters.lufsMin = -12;
    remaining = parsed.text;
  } else {
    parsed = consumeFirst(remaining, /\b(?:quiet|soft|leise|doucement|suave)\b/iu);
    if (parsed.match) {
      filters.lufsMax = -20;
      remaining = parsed.text;
    }
  }

  for (const entry of RHYTHM_PATTERNS) {
    parsed = consumeFirst(remaining, entry.pattern);
    if (parsed.match) {
      filters.rhythm = entry.rhythm;
      remaining = parsed.text;
      break;
    }
  }

  for (const entry of MOOD_PATTERNS) {
    parsed = consumeFirst(remaining, entry.pattern);
    if (parsed.match) {
      filters.mood = entry.mood;
      remaining = parsed.text;
      break;
    }
  }

  const text = remaining
    .replace(/[;,]+/gu, ' ')
    .replace(/\s+/gu, ' ')
    .trim();
  return { text, filters };
}

/** Frontend mirror of the semantic fields used by SQLite; also useful for validating search results. */
export function matchesSemanticTrackFilters(track, filters = {}) {
  if (filters.mood && !matchesMood(track, filters.mood)) return false;
  if (filters.rhythm) {
    const rhythm = normalizedLabel(trackValue(track, 'rhythmPattern', 'rhythm_pattern'));
    const style = normalizedLabel(trackValue(track, 'styleGenre', 'style_genre'));
    if (!rhythm.includes(normalizedLabel(filters.rhythm)) && !style.includes(normalizedLabel(filters.rhythm))) return false;
  }
  if (filters.key) {
    const musicalKey = normalizedLabel(trackValue(track, 'musicalKey', 'musical_key'));
    if (!musicalKey.includes(normalizedLabel(filters.key))) return false;
  }
  const bpm = trackNumber(track, 'bpm', 'bpm');
  if (filters.bpmMin !== undefined && (bpm === undefined || bpm < filters.bpmMin)) return false;
  if (filters.bpmMax !== undefined && (bpm === undefined || bpm > filters.bpmMax)) return false;
  const lufs = trackNumber(track, 'lufsIntegrated', 'lufs_integrated');
  if (filters.lufsMin !== undefined && (lufs === undefined || lufs < filters.lufsMin)) return false;
  if (filters.lufsMax !== undefined && (lufs === undefined || lufs > filters.lufsMax)) return false;
  return true;
}
