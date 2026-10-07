use crate::audio::{AudioEngine, SampleInfo};
use serde::{Deserialize, Serialize};
use std::{
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};
#[cfg(feature = "local-piper-tts")]
use std::{fs, path::PathBuf};

static DUBPLATE_FILE_ID: AtomicU64 = AtomicU64::new(0);
const MAX_DUBPLATE_TEXT_CHARS: usize = 1_000;
const TARGET_PEAK: f32 = 0.92;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DubplateRequest {
    pub model_path: String,
    pub config_path: String,
    pub text: String,
    pub preset: String,
    pub dj_name: Option<String>,
    pub musical_key: String,
    /// Local emotion cue used to shape the phrase and punctuation before synthesis.
    pub emotion: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DubplateResult {
    pub text: String,
    pub audio_path: String,
    pub transposition_semitones: i32,
    pub emotion: String,
    pub master_target_lufs: f32,
    pub integrated_lufs: Option<f64>,
    pub gain_applied_db: f32,
    pub sample: SampleInfo,
}

pub fn engine_available() -> bool {
    cfg!(feature = "local-piper-tts")
}

pub fn build_patois_text(
    preset: &str,
    custom_text: &str,
    dj_name: Option<&str>,
) -> Result<String, String> {
    let custom_text = custom_text.trim();
    if custom_text.is_empty() || custom_text.chars().count() > MAX_DUBPLATE_TEXT_CHARS {
        return Err(format!(
            "dubplate text must contain between 1 and {MAX_DUBPLATE_TEXT_CHARS} characters"
        ));
    }
    if custom_text.chars().any(char::is_control) {
        return Err("dubplate text contains unsupported control characters".to_owned());
    }
    let dj_name = dj_name.unwrap_or_default().trim();
    if dj_name.chars().count() > 80 || dj_name.chars().any(char::is_control) {
        return Err("DJ name is invalid".to_owned());
    }

    let text = match preset {
        "custom" => custom_text.to_owned(),
        "selector" => {
            let intro = if dj_name.is_empty() {
                "Forward, selecta!"
            } else {
                // The name remains a user-supplied slot; it is not sent anywhere.
                "Forward, selecta!"
            };
            let name = if dj_name.is_empty() {
                String::new()
            } else {
                format!(" Big up {dj_name}!")
            };
            format!("{intro}{name} {custom_text} Big tune, wheel and come again!")
        }
        "riddim" => {
            let name = if dj_name.is_empty() {
                String::new()
            } else {
                format!(" Big up {dj_name}!")
            };
            format!("Listen to di riddim!{name} {custom_text} Hold tight, massive!")
        }
        "wheel" => {
            let name = if dj_name.is_empty() {
                String::new()
            } else {
                format!(" {dj_name},")
            };
            format!("Wheel and come again!{name} {custom_text}")
        }
        _ => return Err("unknown Patois dubplate phrase preset".to_owned()),
    };
    if text.chars().count() > MAX_DUBPLATE_TEXT_CHARS + 160 {
        return Err("expanded dubplate phrase is too long".to_owned());
    }
    Ok(text)
}

fn normalize_emotion(value: Option<&str>) -> Result<String, String> {
    match value
        .unwrap_or("confident")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "energetic" | "hype" => Ok("energetic".to_owned()),
        "calm" | "easy" => Ok("calm".to_owned()),
        "uplifting" | "positive" => Ok("uplifting".to_owned()),
        "conscious" | "serious" => Ok("conscious".to_owned()),
        "confident" | "neutral" | "" => Ok("confident".to_owned()),
        _ => Err(
            "dubplate emotion must be energetic, calm, uplifting, conscious, or confident"
                .to_owned(),
        ),
    }
}

fn apply_emotion_cue(phrase: &str, emotion: &str) -> String {
    match emotion {
        "energetic" => format!("Yo! {phrase}!"),
        "calm" => format!("Easy now... {phrase}"),
        "uplifting" => format!("Rise up, family! {phrase}"),
        "conscious" => format!("Listen close... {phrase}"),
        _ => phrase.to_owned(),
    }
}

pub fn generate(
    request: DubplateRequest,
    output_directory: &Path,
    audio: &AudioEngine,
) -> Result<DubplateResult, String> {
    let phrase = build_patois_text(&request.preset, &request.text, request.dj_name.as_deref())?;
    let emotion = normalize_emotion(request.emotion.as_deref())?;
    let text = apply_emotion_cue(&phrase, &emotion);
    let musical_key = parse_key_pitch_class(&request.musical_key)
        .ok_or_else(|| format!("unsupported musical key: {}", request.musical_key))?;

    #[cfg(not(feature = "local-piper-tts"))]
    {
        let _ = (text, musical_key, emotion, output_directory, audio);
        return Err(
            "local Piper TTS is not enabled in this build; rebuild with --features local-piper-tts after reviewing eSpeak NG and voice-model licensing".to_owned(),
        );
    }

    #[cfg(feature = "local-piper-tts")]
    {
        generate_with_piper(
            text,
            emotion,
            musical_key,
            &request.model_path,
            &request.config_path,
            output_directory,
            audio,
        )
    }
}

#[cfg(feature = "local-piper-tts")]
fn generate_with_piper(
    text: String,
    emotion: String,
    target_key: u8,
    model_text: &str,
    config_text: &str,
    output_directory: &Path,
    audio: &AudioEngine,
) -> Result<DubplateResult, String> {
    let model_path = canonical_local_file(model_text, "onnx")?;
    let config_path = canonical_local_file(config_text, "json")?;
    let mut piper = piper_rs::Piper::new(&model_path, &config_path)
        .map_err(|error| format!("could not load the local Piper model: {error}"))?;
    // Piper's ONNX Runtime session uses its CPU provider by default. No model download,
    // cloud request, or DirectML provider is used by this code path.
    let (speech, sample_rate) = piper
        .create(&text, false, None, None, None, None)
        .map_err(|error| format!("local speech synthesis failed: {error}"))?;
    if speech.is_empty() || sample_rate == 0 {
        return Err("the local TTS voice produced no audio".to_owned());
    }

    let (pitched, transposition_semitones) = transpose_to_key(&speech, sample_rate, target_key)
        .ok_or_else(|| "could not estimate a stable voice pitch for key matching".to_owned())?;
    let leveled = normalize_voice_to_r128(pitched, sample_rate, -14.0);
    fs::create_dir_all(output_directory).map_err(|error| {
        format!(
            "could not create dubplate output folder {}: {error}",
            output_directory.display()
        )
    })?;
    let output_path = output_directory.join(next_dubplate_filename());
    write_pcm_wav(&output_path, sample_rate, &leveled.samples)?;
    let sample = audio
        .add_tts_file(&output_path, Some("dubplate"))
        .map_err(|error| {
            format!("dubplate was rendered but could not be loaded into RAM: {error}")
        })?;
    Ok(DubplateResult {
        text,
        audio_path: output_path.to_string_lossy().into_owned(),
        transposition_semitones,
        emotion,
        master_target_lufs: -14.0,
        integrated_lufs: leveled.integrated_lufs,
        gain_applied_db: leveled.gain_db,
        sample,
    })
}

#[cfg(feature = "local-piper-tts")]
// R128-aligned leveling for locally generated voice clips. This helper is kept independent from
// a particular TTS model so future vetted Kokoro/Sherpa adapters use the same master target.
#[derive(Debug)]
struct VoiceLevel {
    samples: Vec<f32>,
    integrated_lufs: Option<f64>,
    gain_db: f32,
}

#[cfg(feature = "local-piper-tts")]
fn normalize_voice_to_r128(
    mut samples: Vec<f32>,
    sample_rate: u32,
    target_lufs: f64,
) -> VoiceLevel {
    let mut analyzer = crate::audio::loudness::LoudnessAnalyzer::new(sample_rate);
    for sample in &samples {
        let sample = if sample.is_finite() { *sample } else { 0.0 };
        // The mono file is duplicated to both channels by the playback graph, so measure the
        // exact dual-mono image that reaches the master bus.
        analyzer.push_stereo(sample, sample);
    }
    let measurement = analyzer.finish();
    let peak_db = measurement.true_peak_dbtp.unwrap_or_else(|| {
        let peak = samples
            .iter()
            .copied()
            .filter(|sample| sample.is_finite())
            .map(f32::abs)
            .fold(0.0_f32, f32::max);
        if peak > 0.0 {
            20.0 * f64::from(peak).log10()
        } else {
            -120.0
        }
    });
    let loudness_offset = measurement
        .integrated_lufs
        .map(|lufs| target_lufs - lufs)
        .unwrap_or(0.0)
        .clamp(-48.0, 24.0);
    // Apply no gain that would exceed the conservative true-peak ceiling. This is static leveling,
    // not a substitute for the master lookahead limiter on the live bus.
    let gain_db = loudness_offset.min(-1.5 - peak_db).clamp(-48.0, 24.0) as f32;
    let gain = 10.0_f32.powf(gain_db / 20.0);
    for sample in &mut samples {
        *sample = if sample.is_finite() {
            *sample * gain
        } else {
            0.0
        };
    }
    VoiceLevel {
        samples,
        integrated_lufs: measurement
            .integrated_lufs
            .map(|lufs| lufs + f64::from(gain_db)),
        gain_db,
    }
}

#[cfg(feature = "local-piper-tts")]
fn canonical_local_file(path_text: &str, extension: &str) -> Result<PathBuf, String> {
    if path_text.trim().is_empty() || path_text.len() > 4_096 {
        return Err(format!("a local .{extension} model file is required"));
    }
    let supplied = PathBuf::from(path_text.trim());
    let path = fs::canonicalize(&supplied).map_err(|error| {
        format!(
            "cannot resolve local model file {}: {error}",
            supplied.display()
        )
    })?;
    if !path.is_file() {
        return Err(format!("model path is not a file: {}", path.display()));
    }
    crate::import::ensure_local_path(&path)
        .map_err(|error| format!("TTS models must be stored locally: {error:#}"))?;
    let actual = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    if !actual.eq_ignore_ascii_case(extension) {
        return Err(format!("expected a local .{extension} model file"));
    }
    Ok(path)
}

fn next_dubplate_filename() -> String {
    let unix_millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    let sequence = DUBPLATE_FILE_ID.fetch_add(1, Ordering::Relaxed);
    format!(
        "dubplate-{unix_millis}-{}-{sequence}.wav",
        std::process::id()
    )
}

fn write_pcm_wav(path: &Path, sample_rate: u32, samples: &[f32]) -> Result<(), String> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec)
        .map_err(|error| format!("could not create dubplate WAV {}: {error}", path.display()))?;
    for sample in samples {
        let sample = if sample.is_finite() {
            sample.clamp(-1.0, 1.0)
        } else {
            0.0
        };
        writer
            .write_sample((sample * i16::MAX as f32).round() as i16)
            .map_err(|error| format!("could not write dubplate audio: {error}"))?;
    }
    writer
        .finalize()
        .map_err(|error| format!("could not finalize dubplate WAV: {error}"))
}

fn parse_key_pitch_class(value: &str) -> Option<u8> {
    let key = value.trim().split_whitespace().next()?.to_ascii_lowercase();
    let normalized = key.replace("♯", "#").replace("♭", "b");
    match normalized.as_str() {
        "c" | "b#" => Some(0),
        "c#" | "db" => Some(1),
        "d" => Some(2),
        "d#" | "eb" => Some(3),
        "e" | "fb" => Some(4),
        "f" | "e#" => Some(5),
        "f#" | "gb" => Some(6),
        "g" => Some(7),
        "g#" | "ab" => Some(8),
        "a" => Some(9),
        "a#" | "bb" => Some(10),
        "b" | "cb" => Some(11),
        _ => None,
    }
}

#[cfg(feature = "local-piper-tts")]
fn transpose_to_key(samples: &[f32], sample_rate: u32, target_key: u8) -> Option<(Vec<f32>, i32)> {
    let fundamental = estimate_median_f0(samples, sample_rate)?;
    let voice_midi = 69.0 + 12.0 * (fundamental / 440.0).log2();
    let current_pitch_class = voice_midi.round() as i32;
    let difference =
        (i32::from(target_key) - current_pitch_class.rem_euclid(12) + 6).rem_euclid(12) - 6;
    if difference == 0 {
        return Some((normalize_peak(samples.to_vec()), 0));
    }

    // A lightweight sample-rate-style transposition keeps this suitable for CPU/mobile use.
    // It shifts pitch and duration together; it is not a formant-preserving phase vocoder.
    let rate = 2.0f64.powf(f64::from(difference) / 12.0);
    let output_length = ((samples.len() as f64) / rate).round().max(1.0) as usize;
    let mut output = Vec::with_capacity(output_length);
    for index in 0..output_length {
        let source_position = (index as f64 * rate).min((samples.len() - 1) as f64);
        let source_index = source_position.floor() as usize;
        let next_index = source_index.saturating_add(1).min(samples.len() - 1);
        let fraction = (source_position - source_index as f64) as f32;
        let left = samples[source_index];
        let right = samples[next_index];
        output.push(left + (right - left) * fraction);
    }
    Some((normalize_peak(output), difference))
}

#[cfg(feature = "local-piper-tts")]
fn normalize_peak(mut samples: Vec<f32>) -> Vec<f32> {
    let peak = samples
        .iter()
        .copied()
        .filter(|sample| sample.is_finite())
        .map(f32::abs)
        .fold(0.0f32, f32::max);
    if peak > 1.0e-6 {
        let gain = TARGET_PEAK / peak;
        for sample in &mut samples {
            *sample = if sample.is_finite() {
                *sample * gain
            } else {
                0.0
            };
        }
    }
    samples
}

#[cfg(feature = "local-piper-tts")]
fn estimate_median_f0(samples: &[f32], sample_rate: u32) -> Option<f32> {
    if samples.len() < (sample_rate as usize / 8) || sample_rate < 8_000 {
        return None;
    }
    let frame_size = (sample_rate as usize * 32 / 1_000).max(128);
    let hop_size = (sample_rate as usize * 12 / 1_000).max(64);
    let min_lag = (sample_rate as usize / 400).max(1);
    let max_lag = (sample_rate as usize / 65)
        .max(min_lag + 1)
        .min(frame_size / 2);
    let mut pitches = Vec::new();
    let mut start = 0usize;
    while start + frame_size <= samples.len() {
        let window = &samples[start..start + frame_size];
        let energy = window.iter().map(|sample| sample * sample).sum::<f32>() / frame_size as f32;
        if energy > 1.0e-5 {
            let mut correlations = Vec::with_capacity(max_lag - min_lag + 1);
            let mut best_lag = min_lag;
            let mut best_score = f32::MIN;
            for lag in min_lag..=max_lag {
                let mut correlation = 0.0f32;
                let mut left_energy = 0.0f32;
                let mut right_energy = 0.0f32;
                for index in 0..(frame_size - lag) {
                    let left = window[index];
                    let right = window[index + lag];
                    correlation += left * right;
                    left_energy += left * left;
                    right_energy += right * right;
                }
                let score = correlation / (left_energy * right_energy).sqrt().max(1.0e-9);
                correlations.push((lag, score));
                if score > best_score {
                    best_score = score;
                    best_lag = lag;
                }
            }
            // Select the shortest strong correlation peak so octave/subharmonic errors are reduced.
            let selected = correlations
                .iter()
                .find(|(lag, score)| {
                    *score >= 0.62
                        && (*lag == min_lag
                            || correlations
                                .iter()
                                .find(|(candidate, _)| candidate == &(*lag - 1))
                                .map_or(true, |(_, previous)| score >= previous))
                })
                .map(|(lag, _)| *lag)
                .unwrap_or(best_lag);
            let normalized_best = correlations
                .iter()
                .find(|(lag, _)| *lag == selected)
                .map(|(_, score)| *score)
                .unwrap_or(best_score);
            if normalized_best >= 0.3 {
                pitches.push(sample_rate as f32 / selected as f32);
            }
        }
        start = start.saturating_add(hop_size);
    }
    if pitches.is_empty() {
        return None;
    }
    pitches.sort_by(|left, right| left.total_cmp(right));
    Some(pitches[pitches.len() / 2])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patois_templates_keep_custom_block_and_name() {
        let result =
            build_patois_text("wheel", "Forward the tune", Some("DJ K")).expect("valid phrase");
        assert!(result.contains("Forward the tune"));
        assert!(result.contains("DJ K"));
        assert!(build_patois_text("unknown", "hello", None).is_err());
    }

    #[test]
    fn emotion_cues_are_validated_and_shape_local_phrase_prosody() {
        let phrase = build_patois_text("wheel", "Forward the tune", Some("DJ K")).unwrap();
        assert!(
            apply_emotion_cue(&phrase, &normalize_emotion(Some("energetic")).unwrap())
                .starts_with("Yo!")
        );
        assert!(
            apply_emotion_cue(&phrase, &normalize_emotion(Some("calm")).unwrap())
                .starts_with("Easy now...")
        );
        assert!(normalize_emotion(Some("cloud")).is_err());
    }

    #[cfg(feature = "local-piper-tts")]
    #[test]
    fn local_voice_leveling_targets_r128_without_exceeding_the_true_peak_ceiling() {
        let sample_rate = 48_000;
        let samples = (0..sample_rate * 3)
            .map(|index| {
                (std::f64::consts::TAU * 220.0 * f64::from(index) / f64::from(sample_rate)).sin()
                    as f32
                    * 0.01
            })
            .collect::<Vec<_>>();
        let leveled = normalize_voice_to_r128(samples, sample_rate, -14.0);
        assert!(leveled
            .integrated_lufs
            .is_some_and(|lufs| (lufs + 14.0).abs() < 0.2));
        assert!(leveled.gain_db > 0.0 && leveled.gain_db < 40.0);
    }

    #[test]
    fn musical_key_names_accept_sharps_and_flats() {
        assert_eq!(parse_key_pitch_class("C# minor"), Some(1));
        assert_eq!(parse_key_pitch_class("Bb major"), Some(10));
        assert_eq!(parse_key_pitch_class("A minor"), Some(9));
        assert_eq!(parse_key_pitch_class("H"), None);
    }
}
