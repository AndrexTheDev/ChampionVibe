use super::StereoFrame;
use hound::{SampleFormat, WavReader};
use serde::Serialize;
use std::{
    collections::HashMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};

const BUILTIN_RATE: u32 = 44_100;
const MAX_SAMPLE_COUNT: usize = 64;
const MAX_SAMPLE_SECONDS: usize = 30;
const MAX_SAMPLE_RATE: u32 = 192_000;
const MAX_BANK_BYTES: usize = 128 * 1024 * 1024;
const USER_SAMPLE_LICENSE: &str = "User-provided · verify your own rights";
const USER_SAMPLE_GROUP: &str = "user";
/// Licence badge for the bundled sampleboard. The drops are original synthesis released by
/// ChampionVibe into the public domain under CC0 1.0, so no third-party master, composition,
/// performance, or vocal right is involved.
const BUNDLED_DROP_LICENSE: &str = "CC0-1.0 · ChampionVibe original synthesis";
const BUNDLED_DROP_GROUP: &str = "drop";
/// Bundled Patois/reggae drop sampleboard.
///
/// Rebuild and verify with `python3 scripts/generate-cc0-reggae-drops.py [--check]`; the committed
/// WAVs are byte-identical to that generator's output and individually hashed in
/// `assets/samples/README.md`. The two `vocal-*` drops are formant-synthesised shouts, not
/// recordings of a singer -- the UI states this so nobody mistakes them for an artist sample.
const BUNDLED_DROPS: &[(&str, &[u8])] = &[
    (
        "airhorn-classic",
        include_bytes!("../../assets/samples/airhorn-classic.wav"),
    ),
    (
        "siren-ragga",
        include_bytes!("../../assets/samples/siren-ragga.wav"),
    ),
    (
        "siren-dub-sweep",
        include_bytes!("../../assets/samples/siren-dub-sweep.wav"),
    ),
    (
        "drum-one-drop",
        include_bytes!("../../assets/samples/drum-one-drop.wav"),
    ),
    (
        "drum-steppers",
        include_bytes!("../../assets/samples/drum-steppers.wav"),
    ),
    (
        "skank-chord",
        include_bytes!("../../assets/samples/skank-chord.wav"),
    ),
    (
        "bass-riddim",
        include_bytes!("../../assets/samples/bass-riddim.wav"),
    ),
    (
        "fx-tape-rewind",
        include_bytes!("../../assets/samples/fx-tape-rewind.wav"),
    ),
    (
        "vocal-hey",
        include_bytes!("../../assets/samples/vocal-hey.wav"),
    ),
    (
        "vocal-pull-up",
        include_bytes!("../../assets/samples/vocal-pull-up.wav"),
    ),
];

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SampleInfo {
    pub id: u32,
    pub name: String,
    pub builtin: bool,
    pub duration_ms: u64,
    /// Provenance string shown in the sample deck (CC0 for bundled drops, user responsibility for
    /// imported WAVs).
    pub license: String,
    /// `drop` for the bundled reggae/Patois sampleboard, `user` for imported folders.
    pub group: String,
}

#[derive(Clone)]
pub struct SampleClip {
    pub id: u32,
    pub name: String,
    pub builtin: bool,
    pub(super) tts_aux: bool,
    pub sample_rate: u32,
    pub license: String,
    pub group: String,
    pub frames: Arc<[StereoFrame]>,
}

impl SampleClip {
    fn info(&self) -> SampleInfo {
        SampleInfo {
            id: self.id,
            name: self.name.clone(),
            builtin: self.builtin,
            duration_ms: (self.frames.len() as u64 * 1_000) / u64::from(self.sample_rate.max(1)),
            license: self.license.clone(),
            group: self.group.clone(),
        }
    }
}

#[derive(Clone)]
pub struct SampleBank {
    clips: Vec<SampleClip>,
    index: HashMap<u32, usize>,
    bytes: usize,
}

impl SampleBank {
    pub fn builtins() -> Self {
        let mut clips = vec![synth_siren(), synth_airhorn(), synth_vinyl_rewind()];
        clips.extend(bundled_drops());
        let bytes = clips
            .iter()
            .map(|clip| clip.frames.len() * std::mem::size_of::<StereoFrame>())
            .sum();
        let index = clips
            .iter()
            .enumerate()
            .map(|(position, clip)| (clip.id, position))
            .collect();
        Self {
            clips,
            index,
            bytes,
        }
    }

    pub fn load_folder(directory: &Path) -> Result<Self, String> {
        let canonical = fs::canonicalize(directory).map_err(|error| {
            format!(
                "cannot resolve sample folder {}: {error}",
                directory.display()
            )
        })?;
        crate::import::ensure_local_path(&canonical)
            .map_err(|error| format!("sample folders must be stored locally: {error:#}"))?;
        if !canonical.is_dir() {
            return Err(format!(
                "sample path is not a folder: {}",
                canonical.display()
            ));
        }

        let mut bank = Self::builtins();
        let mut directories = vec![canonical];
        let mut files = Vec::<PathBuf>::new();
        while let Some(current) = directories.pop() {
            let entries = fs::read_dir(&current).map_err(|error| {
                format!("cannot scan sample folder {}: {error}", current.display())
            })?;
            for entry in entries {
                let entry = entry.map_err(|error| format!("cannot enumerate samples: {error}"))?;
                let file_type = entry.file_type().map_err(|error| {
                    format!("cannot inspect sample {}: {error}", entry.path().display())
                })?;
                if file_type.is_symlink() {
                    continue;
                }
                let path = entry.path();
                if file_type.is_dir() {
                    directories.push(path);
                } else if file_type.is_file()
                    && path
                        .extension()
                        .and_then(|extension| extension.to_str())
                        .is_some_and(|extension| {
                            matches!(extension.to_ascii_lowercase().as_str(), "wav" | "wave")
                        })
                {
                    files.push(path);
                }
            }
        }
        files.sort();
        for path in files {
            if bank.clips.len() >= MAX_SAMPLE_COUNT {
                break;
            }
            bank.add_wav(&path, None, false)?;
        }
        Ok(bank)
    }

    pub fn with_wav(&self, path: &Path, display_name: Option<&str>) -> Result<Self, String> {
        self.with_routed_wav(path, display_name, false)
    }

    pub(super) fn with_tts_wav(
        &self,
        path: &Path,
        display_name: Option<&str>,
    ) -> Result<Self, String> {
        self.with_routed_wav(path, display_name, true)
    }

    fn with_routed_wav(
        &self,
        path: &Path,
        display_name: Option<&str>,
        tts_aux: bool,
    ) -> Result<Self, String> {
        let mut next = self.clone();
        next.add_wav(path, display_name, tts_aux)?;
        Ok(next)
    }

    pub fn infos(&self) -> Vec<SampleInfo> {
        self.clips.iter().map(SampleClip::info).collect()
    }

    pub fn get(&self, id: u32) -> Option<&SampleClip> {
        self.index
            .get(&id)
            .and_then(|position| self.clips.get(*position))
    }

    pub(super) fn shared_frames(&self) -> Vec<Arc<[StereoFrame]>> {
        self.clips
            .iter()
            .map(|clip| Arc::clone(&clip.frames))
            .collect()
    }

    pub fn id_by_name(&self, name: &str) -> Option<u32> {
        self.clips
            .iter()
            .find(|clip| clip.name.eq_ignore_ascii_case(name))
            .map(|clip| clip.id)
    }

    fn add_wav(
        &mut self,
        path: &Path,
        display_name: Option<&str>,
        tts_aux: bool,
    ) -> Result<(), String> {
        if self.clips.len() >= MAX_SAMPLE_COUNT {
            return Err(format!("sample bank is limited to {MAX_SAMPLE_COUNT} WAVs"));
        }
        let canonical = fs::canonicalize(path)
            .map_err(|error| format!("cannot resolve sample {}: {error}", path.display()))?;
        let (sample_rate, frames) = read_wav(&canonical)?;
        let bytes = frames
            .len()
            .saturating_mul(std::mem::size_of::<StereoFrame>());
        if self.bytes.saturating_add(bytes) > MAX_BANK_BYTES {
            return Err(format!(
                "loading {} would exceed the {:.0} MiB in-memory sample limit",
                canonical.display(),
                MAX_BANK_BYTES as f64 / (1024.0 * 1024.0)
            ));
        }

        let fallback = canonical
            .file_stem()
            .and_then(|name| name.to_str())
            .filter(|name| !name.trim().is_empty())
            .unwrap_or("user-sample");
        let fallback = strip_stage_hash_prefix(fallback);
        let name = unique_display_name(display_name.unwrap_or(fallback).trim(), &self.clips);
        let key = canonical.to_string_lossy();
        let id = unique_id(stable_id(&key), &self.clips);
        self.clips.push(SampleClip {
            id,
            name,
            builtin: false,
            tts_aux,
            sample_rate,
            license: USER_SAMPLE_LICENSE.to_owned(),
            group: USER_SAMPLE_GROUP.to_owned(),
            frames: Arc::from(frames.into_boxed_slice()),
        });
        self.index.insert(id, self.clips.len() - 1);
        self.bytes = self.bytes.saturating_add(bytes);
        Ok(())
    }
}

fn read_wav(path: &Path) -> Result<(u32, Vec<StereoFrame>), String> {
    let reader = WavReader::open(path)
        .map_err(|error| format!("cannot read WAV sample {}: {error}", path.display()))?;
    read_wav_reader(reader, &path.display().to_string())
}

fn read_wav_from_bytes(bytes: &[u8], label: &str) -> Result<(u32, Vec<StereoFrame>), String> {
    let reader = WavReader::new(std::io::Cursor::new(bytes))
        .map_err(|error| format!("cannot read bundled WAV sample {label}: {error}"))?;
    read_wav_reader(reader, label)
}

fn read_wav_reader<R: Read + std::io::Seek>(
    reader: WavReader<R>,
    label: &str,
) -> Result<(u32, Vec<StereoFrame>), String> {
    let spec = reader.spec();
    if spec.channels == 0
        || spec.channels > 2
        || spec.sample_rate == 0
        || spec.sample_rate > MAX_SAMPLE_RATE
    {
        return Err(format!(
            "sample {label} must be mono or stereo PCM with a sample rate up to {} Hz",
            MAX_SAMPLE_RATE
        ));
    }
    if spec.sample_format == SampleFormat::Int && !(8..=32).contains(&spec.bits_per_sample) {
        return Err(format!(
            "sample {label} uses an unsupported {}-bit PCM format",
            spec.bits_per_sample
        ));
    }
    if spec.sample_format == SampleFormat::Float && spec.bits_per_sample != 32 {
        return Err(format!("sample {label} must use 32-bit float PCM"));
    }

    let max_input_samples = (u64::from(spec.sample_rate)
        .saturating_mul(MAX_SAMPLE_SECONDS as u64)
        .saturating_mul(u64::from(spec.channels))) as usize;
    let samples: Vec<f32> = match spec.sample_format {
        SampleFormat::Int => {
            let divisor = ((1u64 << (spec.bits_per_sample - 1)) - 1).max(1) as f32;
            reader
                .into_samples::<i32>()
                .take(max_input_samples)
                .map(|sample| {
                    sample
                        .map(|sample| (sample as f32 / divisor).clamp(-1.0, 1.0))
                        .map_err(|error| format!("corrupt PCM sample in {label}: {error}"))
                })
                .collect::<Result<Vec<_>, _>>()?
        }
        SampleFormat::Float => reader
            .into_samples::<f32>()
            .take(max_input_samples)
            .map(|sample| {
                sample
                    .map(|sample| {
                        if sample.is_finite() {
                            sample.clamp(-1.0, 1.0)
                        } else {
                            0.0
                        }
                    })
                    .map_err(|error| format!("corrupt float sample in {label}: {error}"))
            })
            .collect::<Result<Vec<_>, _>>()?,
    };

    let mut frames = Vec::with_capacity(samples.len() / usize::from(spec.channels));
    for frame in samples.chunks_exact(usize::from(spec.channels)) {
        let left = frame[0];
        let right = if spec.channels == 2 { frame[1] } else { left };
        frames.push(StereoFrame { left, right });
    }
    if frames.is_empty() {
        return Err(format!("sample {label} contains no complete frames"));
    }
    Ok((spec.sample_rate, frames))
}

fn strip_stage_hash_prefix(name: &str) -> &str {
    let Some((digest, original_name)) = name.split_once('-') else {
        return name;
    };
    if digest.len() == 64
        && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        && !original_name.is_empty()
    {
        original_name
    } else {
        name
    }
}

fn unique_display_name(name: &str, clips: &[SampleClip]) -> String {
    let base = if name.is_empty() { "user-sample" } else { name };
    if !clips
        .iter()
        .any(|clip| clip.name.eq_ignore_ascii_case(base))
    {
        return base.to_owned();
    }
    for suffix in 2..=MAX_SAMPLE_COUNT {
        let candidate = format!("{base}-{suffix}");
        if !clips
            .iter()
            .any(|clip| clip.name.eq_ignore_ascii_case(&candidate))
        {
            return candidate;
        }
    }
    format!("{base}-sample")
}

fn unique_id(mut id: u32, clips: &[SampleClip]) -> u32 {
    while clips.iter().any(|clip| clip.id == id) {
        id = id.wrapping_add(1).max(1);
    }
    id
}

fn stable_id(value: &str) -> u32 {
    let mut hash = 0x811c_9dc5u32;
    for byte in value.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash.max(1)
}

fn make_builtin(name: &str, frames: Vec<StereoFrame>) -> SampleClip {
    SampleClip {
        id: stable_id(&format!("builtin:{name}")),
        name: name.to_owned(),
        builtin: true,
        tts_aux: false,
        sample_rate: BUILTIN_RATE,
        license: BUNDLED_DROP_LICENSE.to_owned(),
        group: BUNDLED_DROP_GROUP.to_owned(),
        frames: Arc::from(frames.into_boxed_slice()),
    }
}

/// Decodes the embedded CC0 drop sampleboard. Failing to decode a bundled WAV is a build-time
/// mistake, so the clip is skipped instead of taking the whole sample deck down.
fn bundled_drops() -> Vec<SampleClip> {
    let mut clips = Vec::with_capacity(BUNDLED_DROPS.len());
    for (name, bytes) in BUNDLED_DROPS {
        let Ok((sample_rate, frames)) = read_wav_from_bytes(bytes, name) else {
            continue;
        };
        if frames.is_empty() {
            continue;
        }
        clips.push(SampleClip {
            id: stable_id(&format!("drop:{name}")),
            name: (*name).to_owned(),
            builtin: true,
            tts_aux: false,
            sample_rate,
            license: BUNDLED_DROP_LICENSE.to_owned(),
            group: BUNDLED_DROP_GROUP.to_owned(),
            frames: Arc::from(frames.into_boxed_slice()),
        });
    }
    clips
}

fn synth_siren() -> SampleClip {
    let length = (BUILTIN_RATE as usize) * 2;
    let mut frames = Vec::with_capacity(length);
    let mut phase = 0.0f32;
    for index in 0..length {
        let time = index as f32 / BUILTIN_RATE as f32;
        let sweep = (time * std::f32::consts::TAU * 1.4).sin();
        let frequency = 620.0 + 300.0 * sweep;
        phase += std::f32::consts::TAU * frequency / BUILTIN_RATE as f32;
        let fade = (1.0 - time / 2.0).clamp(0.0, 1.0).sqrt();
        let sample = ((phase.sin() * 0.72) + ((phase * 2.0).sin() * 0.18)) * fade * 0.52;
        frames.push(StereoFrame {
            left: sample,
            right: (sample * 0.985) + ((phase * 0.5).sin() * 0.012),
        });
    }
    make_builtin("siren", frames)
}

fn synth_airhorn() -> SampleClip {
    let length = (BUILTIN_RATE as usize) * 9 / 10;
    let mut frames = Vec::with_capacity(length);
    let frequencies = [440.0f32, 554.37, 659.25];
    for index in 0..length {
        let time = index as f32 / BUILTIN_RATE as f32;
        let attack = (time * 40.0).clamp(0.0, 1.0);
        let release = ((0.9 - time) * 8.0).clamp(0.0, 1.0);
        let envelope = attack.min(release);
        let mut sample = 0.0;
        for frequency in frequencies {
            sample += (std::f32::consts::TAU * frequency * time).sin() / 3.0;
        }
        sample = (sample * 1.7).tanh() * envelope * 0.66;
        frames.push(StereoFrame {
            left: sample,
            right: sample * 0.96,
        });
    }
    make_builtin("airhorn", frames)
}

fn synth_vinyl_rewind() -> SampleClip {
    let length = (BUILTIN_RATE as usize) * 4 / 5;
    let mut frames = Vec::with_capacity(length);
    let mut phase = 0.0f32;
    let mut random = 0x734a_82d1u32;
    for index in 0..length {
        let position = index as f32 / length as f32;
        let time = index as f32 / BUILTIN_RATE as f32;
        let envelope = ((1.0 - position) * (1.0 - position) * 1.4).clamp(0.0, 1.0);
        let frequency = 1_900.0 - 1_350.0 * position;
        phase += std::f32::consts::TAU * frequency / BUILTIN_RATE as f32;
        random ^= random << 13;
        random ^= random >> 17;
        random ^= random << 5;
        let noise = (random as i32 as f32) / i32::MAX as f32;
        let flutter = (phase.sin() * 0.48) + (noise * 0.22) + ((time * 31.0).sin() * 0.12);
        let click = if index % (BUILTIN_RATE as usize / 7).max(1) < 18 {
            noise * 0.2
        } else {
            0.0
        };
        let sample = (flutter + click) * envelope * 0.66;
        frames.push(StereoFrame {
            left: sample,
            right: sample * 0.91,
        });
    }
    make_builtin("vinyl-rewind", frames)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn android_stage_digest_is_not_shown_as_part_of_the_sample_name() {
        assert_eq!(
            strip_stage_hash_prefix(&format!("{}-airhorn.wav", "a".repeat(64))),
            "airhorn.wav"
        );
        assert_eq!(strip_stage_hash_prefix("my-airhorn.wav"), "my-airhorn.wav");
    }

    #[test]
    fn builtins_are_predecoded_and_unique() {
        let bank = SampleBank::builtins();
        let infos = bank.infos();
        assert_eq!(infos.len(), 3 + super::BUNDLED_DROPS.len());
        assert_eq!(
            infos.iter().filter(|sample| sample.group == "drop").count(),
            3 + super::BUNDLED_DROPS.len()
        );
        assert!(bank.id_by_name("siren").is_some());
        assert!(bank.id_by_name("airhorn").is_some());
        assert!(bank.id_by_name("vinyl-rewind").is_some());
        assert_eq!(
            infos
                .iter()
                .map(|sample| sample.id)
                .collect::<std::collections::HashSet<_>>()
                .len(),
            infos.len()
        );
        assert!(infos.iter().all(|sample| bank.get(sample.id).is_some()));
        assert!(bank.clips.iter().all(|clip| !clip.frames.is_empty()));
    }

    #[test]
    fn bundled_drops_are_cc0_and_labelled_as_original_synthesis() {
        let bank = SampleBank::builtins();
        for (name, _) in super::BUNDLED_DROPS {
            let id = bank
                .id_by_name(name)
                .unwrap_or_else(|| panic!("bundled drop {name} must be loaded"));
            let clip = bank.get(id).expect("bundled drop resolves");
            assert_eq!(clip.license, super::BUNDLED_DROP_LICENSE);
            assert_eq!(clip.group, "drop");
            assert!(clip.license.contains("CC0-1.0"));
            assert!(!clip.frames.is_empty());
        }
        // The vocal drops must exist and stay advertised as synthesised shouts.
        assert!(bank.id_by_name("vocal-hey").is_some());
        assert!(bank.id_by_name("vocal-pull-up").is_some());
        assert!(bank.id_by_name("airhorn-classic").is_some());
        assert!(bank.id_by_name("siren-ragga").is_some());
        assert!(bank.id_by_name("drum-one-drop").is_some());
        assert!(bank.id_by_name("skank-chord").is_some());
    }

    #[test]
    fn imported_wavs_keep_the_user_provenance_badge() {
        let path = std::env::temp_dir().join(format!(
            "championvibe-user-sample-{}-{}.wav",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time is after Unix epoch")
                .as_nanos()
        ));
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 44_100,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).expect("create test sample");
        for value in [0_i16, 1_000, -1_000, 0] {
            writer.write_sample(value).expect("write test sample");
        }
        writer.finalize().expect("finalize test sample");
        let bank = SampleBank::builtins()
            .with_wav(&path, Some("my-drop"))
            .expect("load user sample");
        let clip = bank.clips.last().expect("user sample exists");
        assert_eq!(clip.group, "user");
        assert_eq!(clip.license, super::USER_SAMPLE_LICENSE);
        std::fs::remove_file(path).expect("remove test sample");
    }

    #[test]
    fn dubplates_are_tagged_for_the_dedicated_tts_master_input() {
        let path = std::env::temp_dir().join(format!(
            "championvibe-tts-route-test-{}-{}.wav",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time is after Unix epoch")
                .as_nanos()
        ));
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).expect("create test sample");
        for value in [0_i16, 2_000, -2_000, 0] {
            writer.write_sample(value).expect("write test sample");
        }
        writer.finalize().expect("finalize test sample");

        let sample_bank = SampleBank::builtins()
            .with_wav(&path, Some("airhorn-copy"))
            .expect("load a normal sample");
        assert!(
            !sample_bank
                .clips
                .last()
                .expect("user sample exists")
                .tts_aux
        );
        let tts_bank = SampleBank::builtins()
            .with_tts_wav(&path, Some("dubplate"))
            .expect("load a TTS dubplate");
        assert!(tts_bank.clips.last().expect("TTS clip exists").tts_aux);
        std::fs::remove_file(path).expect("remove test sample");
    }
}
