use crate::database::LyricWord;
use anyhow::{Context, Result};
use hound::WavReader;
use std::{
    collections::VecDeque,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

const WHISPER_RATE: u32 = 16_000;
const CHUNK_SECONDS: usize = 30;
const CHUNK_SAMPLES: usize = WHISPER_RATE as usize * CHUNK_SECONDS;
const OVERLAP_SAMPLES: usize = WHISPER_RATE as usize;

pub(crate) struct WhisperTranscript {
    pub text: String,
    pub language: Option<String>,
    pub words: Vec<LyricWord>,
    pub sentiment: f64,
    pub sentiment_label: String,
    pub sentiment_confidence: f64,
}

pub(crate) struct LocalWhisper {
    context: WhisperContext,
    threads: i32,
}

impl LocalWhisper {
    pub(crate) fn load(model_path: &Path, threads: usize) -> Result<Self> {
        let context =
            WhisperContext::new_with_params(model_path, WhisperContextParameters::default())
                .with_context(|| {
                    format!(
                        "could not load local Whisper model {}",
                        model_path.display()
                    )
                })?;
        let model_type = context
            .model_type_readable_str()
            .context("could not identify the selected Whisper model type")?
            .to_owned();
        anyhow::ensure!(
            model_type == "tiny" || model_type == "tiny.en",
            "select the smallest Whisper model (tiny or tiny.en); loaded model is {model_type}"
        );
        Ok(Self {
            context,
            threads: threads.clamp(1, 8) as i32,
        })
    }

    pub(crate) fn transcribe(
        &self,
        path: &Path,
        cancel: &AtomicBool,
        mut progress: impl FnMut(f64),
    ) -> Result<WhisperTranscript> {
        let mut reader = WavReader::open(path)
            .with_context(|| format!("cannot read separated acapella {}", path.display()))?;
        let spec = reader.spec();
        anyhow::ensure!(
            spec.channels == 2 && spec.sample_rate == 44_100 && spec.bits_per_sample == 16,
            "acapella WAV has an unexpected format"
        );
        let total_frames = reader.duration().max(1) as u64;
        let mut state = self
            .context
            .create_state()
            .context("could not create Whisper state")?;
        let mut audio_chunk = Vec::with_capacity(CHUNK_SAMPLES);
        let mut resampler = MonoResampler::new(44_100, WHISPER_RATE);
        let mut sample_iter = reader.samples::<i16>();
        let mut input_samples = 0usize;
        let mut chunk_start_samples = 0usize;
        let mut fresh_samples_in_chunk = 0usize;
        let mut detected_language = None;
        let mut words = Vec::new();

        loop {
            if input_samples % 131_072 == 0 && cancel.load(Ordering::Acquire) {
                anyhow::bail!("import cancelled");
            }
            let Some(left_sample) = sample_iter.next() else {
                break;
            };
            let left = left_sample.context("corrupt acapella PCM data")?;
            let right_sample = sample_iter.next().context("odd number of stereo samples")?;
            let right = right_sample.context("corrupt acapella PCM data")?;
            input_samples += 2;
            let mono = (left as f32 + right as f32) / (2.0 * i16::MAX as f32);
            resampler.push(mono);
            while let Some(sample) = resampler.next_sample(false) {
                audio_chunk.push(sample.clamp(-1.0, 1.0));
                fresh_samples_in_chunk += 1;
                if audio_chunk.len() == CHUNK_SAMPLES {
                    transcribe_chunk(
                        &mut state,
                        &audio_chunk,
                        chunk_start_samples,
                        self.threads,
                        &mut detected_language,
                        &mut words,
                    )?;
                    progress((input_samples as u64 / 2) as f64 / total_frames as f64);
                    let next_chunk_start = chunk_start_samples + CHUNK_SAMPLES - OVERLAP_SAMPLES;
                    let overlap = audio_chunk[CHUNK_SAMPLES - OVERLAP_SAMPLES..].to_vec();
                    audio_chunk.clear();
                    audio_chunk.extend_from_slice(&overlap);
                    chunk_start_samples = next_chunk_start;
                    fresh_samples_in_chunk = 0;
                }
            }
        }
        while let Some(sample) = resampler.next_sample(true) {
            audio_chunk.push(sample.clamp(-1.0, 1.0));
            fresh_samples_in_chunk += 1;
        }
        if cancel.load(Ordering::Acquire) {
            anyhow::bail!("import cancelled");
        }
        if fresh_samples_in_chunk > 0 && (chunk_start_samples == 0 || fresh_samples_in_chunk > 1) {
            transcribe_chunk(
                &mut state,
                &audio_chunk,
                chunk_start_samples,
                self.threads,
                &mut detected_language,
                &mut words,
            )?;
        }
        progress((input_samples as u64 / 2) as f64 / total_frames as f64);

        words.sort_by_key(|word| word.start_ms);
        let text = format_transcript(&words);
        let (sentiment, sentiment_label, sentiment_confidence) = lyric_sentiment(&text);
        Ok(WhisperTranscript {
            text,
            language: detected_language,
            words,
            sentiment,
            sentiment_label,
            sentiment_confidence,
        })
    }
}

fn transcribe_chunk(
    state: &mut whisper_rs::WhisperState,
    audio: &[f32],
    offset_samples: usize,
    threads: i32,
    detected_language: &mut Option<String>,
    output_words: &mut Vec<LyricWord>,
) -> Result<()> {
    if audio.iter().all(|sample| sample.abs() < 1.0e-5) {
        return Ok(());
    }
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_n_threads(threads);
    params.set_language(detected_language.as_deref());
    params.set_detect_language(detected_language.is_none());
    params.set_no_context(true);
    params.set_token_timestamps(true);
    params.set_split_on_word(true);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    params.set_print_special(false);
    params.set_no_speech_thold(0.65);
    state
        .full(params, audio)
        .context("local Whisper transcription failed")?;

    let offset_ms = (offset_samples as i64 * 1_000) / i64::from(WHISPER_RATE);
    let ignore_before_ms = if offset_samples == 0 {
        0
    } else {
        offset_ms + (OVERLAP_SAMPLES as i64 * 1_000) / i64::from(WHISPER_RATE)
    };
    let segment_count = state.full_n_segments();
    if detected_language.is_none() && segment_count > 0 {
        *detected_language =
            whisper_rs::get_lang_str(state.full_lang_id_from_state()).map(str::to_owned);
    }
    for segment_index in 0..segment_count {
        let Some(segment) = state.get_segment(segment_index) else {
            continue;
        };
        if segment.no_speech_probability() > 0.75 {
            continue;
        }
        let segment_start = offset_ms + segment.start_timestamp().max(0) * 10;
        let segment_end = offset_ms + segment.end_timestamp().max(0) * 10;
        let mut groups = Vec::<PendingWord>::new();
        let mut pending = PendingWord::default();
        for token_index in 0..segment.n_tokens() {
            let Some(token) = segment.get_token(token_index) else {
                continue;
            };
            let token_text = token.to_str().unwrap_or("");
            if token_text.is_empty() || token_text.starts_with("<|") {
                continue;
            }
            let starts_new_word = token_text.chars().next().is_some_and(char::is_whitespace);
            if starts_new_word && !pending.text.is_empty() {
                groups.push(std::mem::take(&mut pending));
            }
            if pending.text.is_empty() {
                pending.start_ms = None;
                pending.end_ms = None;
            }
            pending.text.push_str(token_text);
            let timing = token.token_data();
            let token_start = offset_ms + timing.t0.saturating_mul(10);
            let token_end = offset_ms + timing.t1.saturating_mul(10);
            if timing.t1 > timing.t0 && token_start >= 0 {
                pending.start_ms = Some(
                    pending
                        .start_ms
                        .map_or(token_start, |value| value.min(token_start)),
                );
                pending.end_ms = Some(
                    pending
                        .end_ms
                        .map_or(token_end, |value| value.max(token_end)),
                );
            }
            pending.confidence_sum += f64::from(token.token_probability().clamp(0.0, 1.0));
            pending.token_count += 1;
        }
        if !pending.text.is_empty() {
            groups.push(pending);
        }
        let text_weight = groups
            .iter()
            .map(|word| {
                word.text
                    .chars()
                    .filter(|ch| ch.is_alphanumeric())
                    .count()
                    .max(1)
            })
            .sum::<usize>()
            .max(1);
        let mut consumed_weight = 0usize;
        for word in groups {
            let width = word
                .text
                .chars()
                .filter(|ch| ch.is_alphanumeric())
                .count()
                .max(1);
            let segment_duration = segment_end.saturating_sub(segment_start).max(0);
            let fallback_start =
                segment_start + segment_duration * consumed_weight as i64 / text_weight as i64;
            consumed_weight += width;
            let fallback_end =
                segment_start + segment_duration * consumed_weight as i64 / text_weight as i64;
            let (start_ms, end_ms) = match (word.start_ms, word.end_ms) {
                (Some(start), Some(end)) if end > start => {
                    let start = start.max(segment_start).min(segment_end.max(segment_start));
                    let end = end.max(start).min(segment_end.max(start));
                    (start, end)
                }
                _ => (fallback_start, fallback_end.max(fallback_start)),
            };
            if end_ms <= ignore_before_ms {
                continue;
            }
            let text = word.text.trim().to_owned();
            if !text.is_empty() {
                output_words.push(LyricWord {
                    text,
                    start_ms: start_ms.max(ignore_before_ms),
                    end_ms,
                    confidence: if word.token_count == 0 {
                        0.0
                    } else {
                        word.confidence_sum / word.token_count as f64
                    },
                });
            }
        }
    }
    Ok(())
}

fn format_transcript(words: &[LyricWord]) -> String {
    let mut text = String::new();
    for word in words {
        if word.text.is_empty() {
            continue;
        }
        let punctuation = word
            .text
            .chars()
            .next()
            .is_some_and(|character| ",.;:!?)]}»”’".contains(character));
        if !text.is_empty() && !punctuation {
            text.push(' ');
        }
        text.push_str(&word.text);
    }
    text
}

#[derive(Default)]
struct PendingWord {
    text: String,
    start_ms: Option<i64>,
    end_ms: Option<i64>,
    confidence_sum: f64,
    token_count: usize,
}

struct MonoResampler {
    step: f64,
    phase: f64,
    input: VecDeque<f32>,
}

impl MonoResampler {
    fn new(input_rate: u32, output_rate: u32) -> Self {
        Self {
            step: f64::from(input_rate) / f64::from(output_rate),
            phase: 0.0,
            input: VecDeque::with_capacity(4096),
        }
    }

    fn push(&mut self, sample: f32) {
        self.input.push_back(sample);
    }

    fn next_sample(&mut self, flush: bool) -> Option<f32> {
        while self.phase >= 1.0 && !self.input.is_empty() {
            self.input.pop_front();
            self.phase -= 1.0;
        }
        if self.input.len() >= 2 {
            let first = *self.input.front()?;
            let second = *self.input.get(1)?;
            let fraction = self.phase as f32;
            self.phase += self.step;
            Some(first + (second - first) * fraction)
        } else if flush && self.input.len() == 1 {
            self.phase = 0.0;
            self.input.pop_front()
        } else {
            None
        }
    }
}

fn lyric_sentiment(text: &str) -> (f64, String, f64) {
    const POSITIVE: &[&str] = &[
        "love",
        "loved",
        "loving",
        "good",
        "great",
        "joy",
        "happy",
        "hope",
        "free",
        "freedom",
        "light",
        "peace",
        "dream",
        "rise",
        "alive",
        "smile",
        "strong",
        "strength",
        "better",
        "beautiful",
        "victory",
        "respect",
        "grateful",
        "brave",
        "feliz",
        "felicidad",
        "amor",
        "amore",
        "alegria",
        "alegría",
        "esperanza",
        "libre",
        "libertad",
        "paz",
        "vida",
        "sonrie",
        "sonríe",
        "fuerza",
        "mejor",
        "hermoso",
        "hermosa",
        "respeto",
        "gracias",
        "valiente",
    ];
    const NEGATIVE: &[&str] = &[
        "hate", "hated", "pain", "hurt", "die", "death", "kill", "war", "fear", "afraid", "sad",
        "dark", "blood", "angry", "alone", "cry", "broken", "suffer", "wrong", "hell", "lost",
        "despair", "dolor", "herida", "morir", "muerte", "matar", "guerra", "miedo", "triste",
        "oscuro", "sangre", "rabia", "solo", "llora", "roto", "sufrir", "odio", "perdido",
        "desamor", "infierno",
    ];
    const NEGATORS: &[&str] = &[
        "not", "no", "never", "dont", "don't", "nunca", "jamás", "jamas", "sin",
    ];
    const INTENSIFIERS: &[&str] = &["very", "so", "really", "too", "much", "muy", "tan", "tanto"];

    let tokens: Vec<String> = text
        .split(|character: char| !character.is_alphanumeric() && character != '\'')
        .filter(|token| !token.is_empty())
        .map(str::to_lowercase)
        .collect();
    let mut total_score = 0.0f64;
    let mut scored_words = 0usize;
    for (index, token) in tokens.iter().enumerate() {
        let mut value = if POSITIVE.contains(&token.as_str()) {
            1.0
        } else if NEGATIVE.contains(&token.as_str()) {
            -1.0
        } else {
            continue;
        };
        if tokens[index.saturating_sub(3)..index]
            .iter()
            .any(|previous| NEGATORS.contains(&previous.as_str()))
        {
            value = -value;
        }
        if tokens[index.saturating_sub(2)..index]
            .iter()
            .any(|previous| INTENSIFIERS.contains(&previous.as_str()))
        {
            value *= 1.5;
        }
        total_score += value;
        scored_words += 1;
    }
    let score = if scored_words == 0 {
        0.0
    } else {
        (total_score / scored_words as f64).clamp(-1.0, 1.0)
    };
    let label = if score > 0.12 {
        "positive"
    } else if score < -0.12 {
        "negative"
    } else {
        "neutral"
    }
    .to_owned();
    let confidence = if tokens.is_empty() {
        0.0
    } else {
        (scored_words as f64 / tokens.len() as f64)
            .sqrt()
            .clamp(0.0, 1.0)
    };
    (score, label, confidence)
}

#[cfg(test)]
mod tests {
    use super::lyric_sentiment;

    #[test]
    fn local_lexicon_scores_positive_and_negated_lyrics() {
        assert!(lyric_sentiment("I love the light and hope").0 > 0.0);
        assert!(lyric_sentiment("no love, only pain and fear").0 < 0.0);
    }
}
