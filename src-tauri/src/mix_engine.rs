use crate::{
    audio::dj_engine::{plan_transition, transition_score, MirProfile, TransitionStyle},
    audio::{self, AudioControlUpdate},
    database, import,
};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    sync::{mpsc, Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

const MIX_POLL_INTERVAL: Duration = Duration::from_millis(40);
const DEFAULT_MASTER_GAIN: f32 = 0.75;
const MAX_MIX_CANDIDATES: usize = 96;
const RECENT_TRACK_MEMORY: usize = 8;
const CHORUS_MIN_REPEAT_GAP_MS: i64 = 12_000;
const CHORUS_MIN_START_MS: u64 = 8_000;
const MIN_CROSSFADE_MS: u64 = 500;
const MAX_CROSSFADE_MS: u64 = 45_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MixPreset {
    SmoothSelector,
    ClubHype,
    AggressiveClash,
    FreestyleMaestro,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MixStyle {
    DeepRoots,
    ClubHype,
    AggressiveClash,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum TransitionKind {
    LongCrossfade,
    HardChorusCut,
    SemanticRewindCut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum TempoMode {
    HalfTime,
    Normal,
    DoubleTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum MixPhase {
    Stopped,
    Loading,
    Playing,
    Paused,
    WaitingForRewind,
    Transitioning,
    Error,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MixDecision {
    pub preset: MixPreset,
    pub style: MixStyle,
    pub intensity_override: Option<u8>,
    pub transition: TransitionKind,
    pub source_track_id: i64,
    pub next_track_id: i64,
    pub source_bpm: Option<f64>,
    pub incoming_bpm: Option<f64>,
    pub matched_bpm: Option<f64>,
    pub tempo_mode: TempoMode,
    pub playback_rate: f32,
    pub tempo_error_percent: Option<f64>,
    pub transition_at_ms: Option<u64>,
    pub fade_duration_ms: u64,
    pub incoming_cue_ms: u64,
    pub chorus_cut_ms: Option<u64>,
    pub semantic_phrase: Option<String>,
    pub semantic_match: Option<String>,
    pub semantic_match_words: usize,
    pub same_riddim_family: bool,
    pub mir_compatibility: f64,
    pub mood_compatibility: Option<f64>,
    pub rhythm_compatibility: Option<f64>,
    pub key_compatibility: Option<f64>,
    pub loudness_delta_db: Option<f64>,
    pub ideal_drop_cue_ms: u64,
    pub incoming_curve_exponent: f32,
    pub bass_handoff_start: f32,
    pub bass_handoff_end: f32,
    pub eq_sweep: bool,
    pub bass_swap: bool,
    pub spring_auto_dub: bool,
    pub auto_rewind: bool,
    pub fader_kill_switches: bool,
    pub rationale: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MixStatus {
    pub active: bool,
    pub phase: MixPhase,
    pub preset: Option<MixPreset>,
    pub style: Option<MixStyle>,
    pub intensity_override: Option<u8>,
    pub current_track_id: Option<i64>,
    pub current_title: Option<String>,
    pub next_track_id: Option<i64>,
    pub next_title: Option<String>,
    pub position_ms: Option<u64>,
    pub transition_progress: f32,
    pub active_deck: Option<usize>,
    pub decision: Option<MixDecision>,
    pub error: Option<String>,
}

impl Default for MixStatus {
    fn default() -> Self {
        Self {
            active: false,
            phase: MixPhase::Stopped,
            preset: None,
            style: None,
            intensity_override: None,
            current_track_id: None,
            current_title: None,
            next_track_id: None,
            next_title: None,
            position_ms: None,
            transition_progress: 0.0,
            active_deck: None,
            decision: None,
            error: None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct TempoMatch {
    mode: TempoMode,
    playback_rate: f32,
    matched_bpm: Option<f64>,
    relative_error: Option<f64>,
}

#[derive(Debug, Clone)]
struct WordPlayMatch {
    cue_ms: u64,
    matched_words: Vec<String>,
    phrase: String,
}

#[derive(Debug, Clone)]
struct PreparedTransition {
    next: database::MixTrackRecord,
    decision: MixDecision,
}

#[derive(Clone, Copy)]
enum RunnerPhase {
    Playing,
    WaitingForRewind { since: Instant },
    Transitioning { since: Instant },
}

struct ActiveMix {
    preset: MixPreset,
    pending_preset: Option<MixPreset>,
    intensity_override: Option<u8>,
    pending_intensity_override: Option<Option<u8>>,
    style: MixStyle,
    active_deck: usize,
    current: database::MixTrackRecord,
    prepared: PreparedTransition,
    recent_track_ids: VecDeque<i64>,
    last_position_ms: u64,
    phase: RunnerPhase,
}

enum MixCommand {
    Start { track_id: i64, preset: MixPreset },
    Stop(mpsc::SyncSender<MixStatus>),
    ForceNext(mpsc::SyncSender<Result<MixStatus, String>>),
    SetPreset(MixPreset),
    SetIntensity(Option<u8>),
}

#[derive(Clone)]
pub struct MixEngine {
    commands: mpsc::Sender<MixCommand>,
    status: Arc<Mutex<MixStatus>>,
    decks: [audio::AudioEngine; 2],
}

impl MixEngine {
    pub fn new(
        database: Arc<Mutex<Connection>>,
        first_deck: audio::AudioEngine,
        second_deck: audio::AudioEngine,
    ) -> Self {
        let status = Arc::new(Mutex::new(MixStatus::default()));
        let (commands, receiver) = mpsc::channel();
        let worker_status = Arc::clone(&status);
        let worker_decks = [first_deck.clone(), second_deck.clone()];
        let worker_result = thread::Builder::new()
            .name("championvibe-mix-engine".to_owned())
            .spawn(move || mix_worker(receiver, database, worker_decks, worker_status));
        if let Err(error) = worker_result {
            if let Ok(mut current) = status.lock() {
                current.phase = MixPhase::Error;
                current.error = Some(format!("could not start the local mix worker: {error}"));
            }
        }
        Self {
            commands,
            status,
            decks: [first_deck, second_deck],
        }
    }

    pub fn start(&self, track_id: i64, preset: MixPreset) -> Result<MixStatus, String> {
        if track_id <= 0 {
            return Err("a positive local track id is required".to_owned());
        }
        self.commands
            .send(MixCommand::Start { track_id, preset })
            .map_err(|_| "the local mix worker is unavailable".to_owned())?;
        let intensity_override = self.status().intensity_override;
        if let Ok(mut status) = self.status.lock() {
            *status = MixStatus {
                phase: MixPhase::Loading,
                preset: Some(preset),
                intensity_override,
                current_track_id: Some(track_id),
                ..MixStatus::default()
            };
        }
        Ok(self.status())
    }

    pub fn stop(&self) -> Result<MixStatus, String> {
        let (reply, response) = mpsc::sync_channel(1);
        self.commands
            .send(MixCommand::Stop(reply))
            .map_err(|_| "the local mix worker is unavailable".to_owned())?;
        response
            .recv_timeout(Duration::from_secs(30))
            .map_err(|error| format!("the mix worker did not stop cleanly: {error}"))
    }

    pub fn force_next(&self) -> Result<MixStatus, String> {
        let (reply, response) = mpsc::sync_channel(1);
        self.commands
            .send(MixCommand::ForceNext(reply))
            .map_err(|_| "the local mix worker is unavailable".to_owned())?;
        response
            .recv_timeout(Duration::from_secs(30))
            .map_err(|error| format!("the mix worker did not force the next track: {error}"))?
    }

    pub fn set_intensity(&self, intensity: Option<u8>) -> Result<MixStatus, String> {
        if intensity.is_some_and(|value| !(1..=4).contains(&value)) {
            return Err("mix intensity must be between 1 and 4".to_owned());
        }
        self.commands
            .send(MixCommand::SetIntensity(intensity))
            .map_err(|_| "the local mix worker is unavailable".to_owned())?;
        if let Ok(mut status) = self.status.lock() {
            status.intensity_override = intensity;
        }
        Ok(self.status())
    }

    pub fn set_preset(&self, preset: MixPreset) -> Result<MixStatus, String> {
        self.commands
            .send(MixCommand::SetPreset(preset))
            .map_err(|_| "the local mix worker is unavailable".to_owned())?;
        if let Ok(mut status) = self.status.lock() {
            status.preset = Some(preset);
        }
        Ok(self.status())
    }

    pub fn status(&self) -> MixStatus {
        self.status
            .lock()
            .map(|status| status.clone())
            .unwrap_or_else(|_| MixStatus {
                phase: MixPhase::Error,
                error: Some("mix status lock is poisoned".to_owned()),
                ..MixStatus::default()
            })
    }

    pub fn active_audio_engine(&self) -> audio::AudioEngine {
        let active = self.status().active_deck.unwrap_or(0).min(1);
        self.decks[active].clone()
    }

    pub fn set_deck_controls(
        &self,
        deck: u8,
        update: audio::AudioControlUpdate,
    ) -> Result<audio::AudioStatus, String> {
        let deck_index = usize::from(deck);
        let engine = self
            .decks
            .get(deck_index)
            .ok_or_else(|| "deck must be A (0) or B (1)".to_owned())?;
        engine.update_controls(update)
    }

    /// Configure which of the four aligned stem lanes are audible on each deck.
    /// Android Demo uses acapella + instrumental; Pro and desktop use all four.
    pub fn set_stem_profile(&self, active_stems: u8) -> Result<(), String> {
        if !matches!(active_stems, 2 | 4) {
            return Err("active stem profile must be 2 or 4".to_owned());
        }
        for deck in &self.decks {
            deck.set_active_stem_profile(active_stems)?;
        }
        Ok(())
    }

    pub fn audio_status(&self) -> audio::AudioStatus {
        self.active_audio_engine().status()
    }

    pub fn sample_catalog(&self) -> Vec<audio::SampleInfo> {
        self.active_audio_engine().sample_catalog()
    }

    pub fn load_sample_folder(
        &self,
        path: &std::path::Path,
    ) -> Result<Vec<audio::SampleInfo>, String> {
        let first = self.decks[0].load_sample_folder(path)?;
        self.decks[1].load_sample_folder(path)?;
        Ok(first)
    }
}

fn mix_worker(
    receiver: mpsc::Receiver<MixCommand>,
    database: Arc<Mutex<Connection>>,
    decks: [audio::AudioEngine; 2],
    shared_status: Arc<Mutex<MixStatus>>,
) {
    let mut active_mix: Option<ActiveMix> = None;
    let mut intensity_override: Option<u8> = None;
    loop {
        match receiver.recv_timeout(MIX_POLL_INTERVAL) {
            Ok(MixCommand::Start { track_id, preset }) => {
                stop_decks(&decks);
                active_mix = None;
                publish(
                    &shared_status,
                    MixStatus {
                        phase: MixPhase::Loading,
                        preset: Some(preset),
                        intensity_override,
                        current_track_id: Some(track_id),
                        ..MixStatus::default()
                    },
                );
                match start_active_mix(
                    track_id,
                    preset,
                    &database,
                    &decks,
                    &shared_status,
                    VecDeque::new(),
                    intensity_override,
                ) {
                    Ok(run) => active_mix = Some(run),
                    Err(error) => {
                        stop_decks(&decks);
                        publish(
                            &shared_status,
                            MixStatus {
                                phase: MixPhase::Error,
                                preset: Some(preset),
                                intensity_override,
                                current_track_id: Some(track_id),
                                error: Some(error),
                                ..MixStatus::default()
                            },
                        );
                    }
                }
            }
            Ok(MixCommand::Stop(reply)) => {
                stop_decks(&decks);
                active_mix = None;
                let status = MixStatus {
                    intensity_override,
                    ..MixStatus::default()
                };
                publish(&shared_status, status.clone());
                let _ = reply.send(status);
            }
            Ok(MixCommand::ForceNext(reply)) => {
                let result = if let Some(run) = active_mix.as_mut() {
                    force_next_transition(run, &database, &decks, &shared_status)
                } else {
                    Err("start the mix engine before forcing the next track".to_owned())
                };
                let response = match result {
                    Ok(()) => Ok(shared_status
                        .lock()
                        .map(|status| status.clone())
                        .unwrap_or_else(|_| MixStatus {
                            phase: MixPhase::Error,
                            error: Some("mix status lock is poisoned".to_owned()),
                            ..MixStatus::default()
                        })),
                    Err(error) => {
                        stop_decks(&decks);
                        active_mix = None;
                        set_error(&shared_status, error.clone());
                        Err(error)
                    }
                };
                let _ = reply.send(response);
            }
            Ok(MixCommand::SetIntensity(value)) => {
                intensity_override = value;
                let result = if let Some(run) = active_mix.as_mut() {
                    if matches!(&run.phase, RunnerPhase::Transitioning { .. }) {
                        run.pending_intensity_override = Some(value);
                        if let Ok(mut status) = shared_status.lock() {
                            status.intensity_override = value;
                        }
                        Ok(())
                    } else {
                        let active_preset = run.preset;
                        replace_prepared_transition(
                            run,
                            active_preset,
                            value,
                            &database,
                            &decks,
                            &shared_status,
                        )
                    }
                } else {
                    if let Ok(mut status) = shared_status.lock() {
                        status.intensity_override = value;
                    }
                    Ok(())
                };
                if let Err(error) = result {
                    stop_decks(&decks);
                    active_mix = None;
                    set_error(&shared_status, error);
                }
            }
            Ok(MixCommand::SetPreset(preset)) => {
                let result = if let Some(run) = active_mix.as_mut() {
                    if matches!(&run.phase, RunnerPhase::Transitioning { .. }) {
                        run.pending_preset = Some(preset);
                        if let Ok(mut status) = shared_status.lock() {
                            status.preset = Some(preset);
                        }
                        Ok(())
                    } else {
                        let intensity_override = run.intensity_override;
                        replace_prepared_transition(
                            run,
                            preset,
                            intensity_override,
                            &database,
                            &decks,
                            &shared_status,
                        )
                    }
                } else {
                    if let Ok(mut status) = shared_status.lock() {
                        status.preset = Some(preset);
                    }
                    Ok(())
                };
                if let Err(error) = result {
                    stop_decks(&decks);
                    active_mix = None;
                    set_error(&shared_status, error);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }

        if let Some(run) = active_mix.as_mut() {
            if let Err(error) = tick_active_mix(run, &database, &decks, &shared_status) {
                stop_decks(&decks);
                set_error(&shared_status, error);
                active_mix = None;
            }
        }
    }
    stop_decks(&decks);
}

fn start_active_mix(
    track_id: i64,
    preset: MixPreset,
    database: &Arc<Mutex<Connection>>,
    decks: &[audio::AudioEngine; 2],
    shared_status: &Arc<Mutex<MixStatus>>,
    recent_track_ids: VecDeque<i64>,
    intensity_override: Option<u8>,
) -> Result<ActiveMix, String> {
    let current = {
        let connection = database
            .lock()
            .map_err(|_| "application database lock is poisoned".to_owned())?;
        database::load_mix_track(&connection, track_id)
            .map_err(|error| format!("could not read current mix metadata: {error:#}"))?
            .ok_or_else(|| format!("track {track_id} is not in the local library"))?
    };
    let active_stem_count = usize::from(decks[0].active_stem_count());
    let current_stems = track_stem_files_for_profile(&current, active_stem_count)?;

    reset_deck(&decks[0], DEFAULT_MASTER_GAIN, 1.0)?;
    decks[0]
        .load_at_with_loudness(
            current_stems,
            track_beat_grid(&current),
            0,
            current.integrated_lufs,
        )
        .map_err(|error| format!("could not load current local track: {error}"))?;
    decks[0]
        .play()
        .map_err(|error| format!("could not start current track: {error}"))?;

    let prepared = prepare_next(
        &current,
        preset,
        None,
        1.0,
        &recent_track_ids,
        database,
        &decks[1],
        intensity_override,
    )?;
    let style = prepared.decision.style;
    let active = ActiveMix {
        preset,
        pending_preset: None,
        intensity_override,
        pending_intensity_override: None,
        style,
        active_deck: 0,
        current: current.clone(),
        prepared,
        recent_track_ids,
        last_position_ms: 0,
        phase: RunnerPhase::Playing,
    };
    decks[0].set_auto_rewind(style == MixStyle::AggressiveClash);
    publish_status(&active, decks, shared_status, MixPhase::Playing, 0.0);
    Ok(active)
}

fn prepare_next(
    current: &database::MixTrackRecord,
    preset: MixPreset,
    previous_style: Option<MixStyle>,
    source_playback_rate: f32,
    recent_track_ids: &VecDeque<i64>,
    database: &Arc<Mutex<Connection>>,
    incoming_deck: &audio::AudioEngine,
    intensity_override: Option<u8>,
) -> Result<PreparedTransition, String> {
    let active_stem_count = usize::from(incoming_deck.active_stem_count());
    let expected_style = effective_style(preset, current, None, previous_style, intensity_override);
    let semantic_tokens = if expected_style == MixStyle::AggressiveClash {
        semantic_tail_tokens(current)
    } else {
        Vec::new()
    };
    let candidates = {
        let connection = database
            .lock()
            .map_err(|_| "application database lock is poisoned".to_owned())?;
        let ids = database::mix_candidate_ids(
            &connection,
            current.id,
            current.riddim_family_id,
            current
                .bpm
                .map(|bpm| bpm * f64::from(source_playback_rate.clamp(0.8, 1.2))),
            &semantic_tokens,
            MAX_MIX_CANDIDATES,
        )
        .map_err(|error| format!("could not search local mix candidates: {error:#}"))?;
        let mut tracks = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(track) = database::load_mix_track(&connection, id)
                .map_err(|error| format!("could not load local mix candidate {id}: {error:#}"))?
            {
                if track_stem_files_for_profile(&track, active_stem_count).is_ok() {
                    tracks.push(track);
                }
            }
        }
        tracks
    };
    let (next, word_play) = choose_candidate_for_profile(
        current,
        &candidates,
        preset,
        expected_style,
        source_playback_rate,
        recent_track_ids,
        intensity_override,
        active_stem_count,
    )
    .ok_or_else(|| {
        format!("no other playable local track with {active_stem_count} active stems is available")
    })?;
    let style = effective_style(
        preset,
        current,
        Some(&next),
        previous_style,
        intensity_override,
    );
    let tempo = calculate_tempo_match(current.bpm, source_playback_rate, next.bpm);
    let same_riddim = shares_riddim(current, &next);
    let decision = build_decision(
        preset,
        style,
        current,
        &next,
        tempo,
        word_play,
        same_riddim,
        source_playback_rate,
        intensity_override,
    );

    reset_deck(incoming_deck, 0.0, tempo.playback_rate)?;
    incoming_deck.set_auto_rewind(false);
    incoming_deck
        .load_at_with_loudness(
            track_stem_files_for_profile(&next, active_stem_count)?,
            track_beat_grid(&next),
            decision.incoming_cue_ms,
            next.integrated_lufs,
        )
        .map_err(|error| format!("could not prebuffer next local track: {error}"))?;
    Ok(PreparedTransition { next, decision })
}

fn replace_prepared_transition(
    run: &mut ActiveMix,
    preset: MixPreset,
    intensity_override: Option<u8>,
    database: &Arc<Mutex<Connection>>,
    decks: &[audio::AudioEngine; 2],
    shared_status: &Arc<Mutex<MixStatus>>,
) -> Result<(), String> {
    if matches!(run.phase, RunnerPhase::Transitioning { .. }) {
        return Err("the preset can be changed after the current transition completes".to_owned());
    }
    let next_deck = 1 - run.active_deck;
    decks[next_deck]
        .stop()
        .map_err(|error| format!("could not cancel the prepared deck: {error}"))?;
    let source_rate = decks[run.active_deck].status().playback_rate;
    let prepared = prepare_next(
        &run.current,
        preset,
        Some(run.style),
        source_rate,
        &run.recent_track_ids,
        database,
        &decks[next_deck],
        intensity_override,
    )?;
    decks[run.active_deck]
        .update_controls(AudioControlUpdate {
            kill_bass: Some(false),
            kill_mid: Some(false),
            kill_high: Some(false),
            ..AudioControlUpdate::default()
        })
        .map_err(|error| format!("could not clear old preset fader state: {error}"))?;
    run.preset = preset;
    run.intensity_override = intensity_override;
    run.pending_intensity_override = None;
    run.style = prepared.decision.style;
    run.prepared = prepared;
    run.phase = RunnerPhase::Playing;
    decks[run.active_deck].set_auto_rewind(run.style == MixStyle::AggressiveClash);
    publish_status(run, decks, shared_status, MixPhase::Playing, 0.0);
    Ok(())
}

fn force_next_transition(
    run: &mut ActiveMix,
    database: &Arc<Mutex<Connection>>,
    decks: &[audio::AudioEngine; 2],
    shared_status: &Arc<Mutex<MixStatus>>,
) -> Result<(), String> {
    if matches!(run.phase, RunnerPhase::Transitioning { .. }) {
        return Ok(());
    }
    match run.prepared.decision.transition {
        TransitionKind::LongCrossfade => begin_crossfade(run, decks, shared_status),
        TransitionKind::HardChorusCut => hard_cut(run, database, decks, shared_status),
        TransitionKind::SemanticRewindCut => {
            let outgoing = run.active_deck;
            decks[outgoing]
                .update_controls(AudioControlUpdate {
                    kill_bass: Some(true),
                    kill_mid: Some(true),
                    kill_high: Some(true),
                    ..AudioControlUpdate::default()
                })
                .map_err(|error| format!("could not arm manual clash fader kills: {error}"))?;
            decks[outgoing].set_auto_rewind(false);
            decks[outgoing]
                .rewind(None)
                .map_err(|error| format!("could not execute manual clash rewind: {error}"))?;
            begin_crossfade(run, decks, shared_status)
        }
    }
}

fn tick_active_mix(
    run: &mut ActiveMix,
    database: &Arc<Mutex<Connection>>,
    decks: &[audio::AudioEngine; 2],
    shared_status: &Arc<Mutex<MixStatus>>,
) -> Result<(), String> {
    let current_deck = run.active_deck;
    let next_deck = 1 - current_deck;
    let current_audio = decks[current_deck].status();
    if current_audio.failed {
        return Err(current_audio
            .last_error
            .unwrap_or_else(|| "native audio deck failed".to_owned()));
    }
    if matches!(run.phase, RunnerPhase::Transitioning { .. }) {
        let incoming_audio = decks[next_deck].status();
        if incoming_audio.failed {
            return Err(incoming_audio
                .last_error
                .unwrap_or_else(|| "incoming native audio deck failed".to_owned()));
        }
    }
    if !current_audio.playing && !current_audio.finished {
        publish_status(run, decks, shared_status, MixPhase::Paused, 0.0);
        return Ok(());
    }
    let current_ms = audio_position_ms(&current_audio).unwrap_or(run.last_position_ms);

    match run.phase {
        RunnerPhase::Playing => {
            if run.style == MixStyle::AggressiveClash {
                let arm_at = run.prepared.decision.transition_at_ms;
                if arm_at.is_some_and(|time| current_ms >= time) {
                    decks[current_deck]
                        .update_controls(AudioControlUpdate {
                            kill_bass: Some(true),
                            kill_mid: Some(true),
                            kill_high: Some(true),
                            ..AudioControlUpdate::default()
                        })
                        .map_err(|error| format!("could not arm clash fader kills: {error}"))?;
                    decks[current_deck].set_auto_rewind(false);
                    decks[current_deck]
                        .rewind(None)
                        .map_err(|error| format!("could not execute clash Auto-Rewind: {error}"))?;
                    begin_crossfade(run, decks, shared_status)?;
                    return Ok(());
                }
                if current_ms + 250 < run.last_position_ms
                    && run.last_position_ms > CHORUS_MIN_START_MS
                {
                    decks[current_deck].set_auto_rewind(false);
                    begin_crossfade(run, decks, shared_status)?;
                    return Ok(());
                } else if current_audio.finished {
                    let since = match run.phase {
                        RunnerPhase::WaitingForRewind { since } => since,
                        _ => Instant::now(),
                    };
                    run.phase = RunnerPhase::WaitingForRewind { since };
                    publish_status(run, decks, shared_status, MixPhase::WaitingForRewind, 0.0);
                    return Ok(());
                }
            } else if current_audio.finished
                || run
                    .prepared
                    .decision
                    .transition_at_ms
                    .is_some_and(|time| current_ms >= time)
            {
                if run.prepared.decision.transition == TransitionKind::HardChorusCut {
                    hard_cut(run, database, decks, shared_status)?;
                    return Ok(());
                }
                begin_crossfade(run, decks, shared_status)?;
                return Ok(());
            }
            run.last_position_ms = current_ms;
            publish_status(run, decks, shared_status, MixPhase::Playing, 0.0);
        }
        RunnerPhase::WaitingForRewind { since } => {
            if current_ms + 250 < run.last_position_ms {
                decks[current_deck].set_auto_rewind(false);
                begin_crossfade(run, decks, shared_status)?;
                return Ok(());
            } else if since.elapsed() > Duration::from_secs(3) {
                // A badly behaved decoder/device must not trap the set in the rewind state.
                decks[current_deck].set_auto_rewind(false);
                begin_crossfade(run, decks, shared_status)?;
                return Ok(());
            } else {
                run.last_position_ms = current_ms;
                publish_status(run, decks, shared_status, MixPhase::WaitingForRewind, 0.0);
            }
        }
        RunnerPhase::Transitioning { since } => {
            let duration = Duration::from_millis(run.prepared.decision.fade_duration_ms.max(1));
            let progress =
                (since.elapsed().as_secs_f64() / duration.as_secs_f64()).clamp(0.0, 1.0) as f32;
            apply_crossfade_gains(run, decks, progress)?;
            publish_status(run, decks, shared_status, MixPhase::Transitioning, progress);
            if progress >= 1.0 {
                finish_crossfade(run, database, decks, shared_status)?;
            }
        }
    }
    Ok(())
}

fn begin_crossfade(
    run: &mut ActiveMix,
    decks: &[audio::AudioEngine; 2],
    shared_status: &Arc<Mutex<MixStatus>>,
) -> Result<(), String> {
    let outgoing = run.active_deck;
    let incoming = 1 - outgoing;
    decks[outgoing].set_auto_rewind(false);
    if run.prepared.decision.fader_kill_switches {
        decks[outgoing]
            .update_controls(AudioControlUpdate {
                kill_bass: Some(true),
                kill_mid: Some(true),
                kill_high: Some(true),
                ..AudioControlUpdate::default()
            })
            .map_err(|error| format!("could not engage clash fader kill switches: {error}"))?;
    }
    decks[incoming]
        .update_controls(AudioControlUpdate {
            master_gain: Some(0.0),
            kill_bass: Some(false),
            kill_mid: Some(false),
            kill_high: Some(false),
            ..AudioControlUpdate::default()
        })
        .map_err(|error| format!("could not set incoming crossfade gain: {error}"))?;
    decks[incoming]
        .play()
        .map_err(|error| format!("could not start the incoming deck: {error}"))?;
    run.phase = RunnerPhase::Transitioning {
        since: Instant::now(),
    };
    publish_status(run, decks, shared_status, MixPhase::Transitioning, 0.0);
    Ok(())
}

fn hard_cut(
    run: &mut ActiveMix,
    database: &Arc<Mutex<Connection>>,
    decks: &[audio::AudioEngine; 2],
    shared_status: &Arc<Mutex<MixStatus>>,
) -> Result<(), String> {
    let outgoing = run.active_deck;
    let incoming = 1 - outgoing;
    decks[outgoing].set_auto_rewind(false);
    decks[outgoing]
        .stop()
        .map_err(|error| format!("could not stop at the chorus cut: {error}"))?;
    decks[incoming]
        .update_controls(AudioControlUpdate {
            master_gain: Some(DEFAULT_MASTER_GAIN),
            stem_gains: Some([1.0; 4]),
            kill_bass: Some(false),
            kill_mid: Some(false),
            kill_high: Some(false),
            ..AudioControlUpdate::default()
        })
        .map_err(|error| format!("could not set the incoming chorus level: {error}"))?;
    decks[incoming]
        .play()
        .map_err(|error| format!("could not start the next track at its chorus cue: {error}"))?;
    advance_to_prepared_track(run, database, decks, shared_status, incoming)?;
    Ok(())
}

fn apply_crossfade_gains(
    run: &ActiveMix,
    decks: &[audio::AudioEngine; 2],
    progress: f32,
) -> Result<(), String> {
    let outgoing = run.active_deck;
    let incoming = 1 - outgoing;
    let decision = &run.prepared.decision;
    let progress = progress.clamp(0.0, 1.0);
    let shaped_progress = progress.powf(decision.incoming_curve_exponent.clamp(0.75, 1.3));
    let angle = shaped_progress * std::f32::consts::FRAC_PI_2;
    let outgoing_gain = (DEFAULT_MASTER_GAIN * angle.cos()).max(0.0);
    let incoming_gain = (DEFAULT_MASTER_GAIN * angle.sin()).max(0.0);
    let (outgoing_stems, incoming_stems) = if decision.bass_swap {
        let span = (decision.bass_handoff_end - decision.bass_handoff_start).max(0.01);
        let swap = ((progress - decision.bass_handoff_start) / span).clamp(0.0, 1.0);
        ([1.0, 1.0, 1.0 - swap, 1.0], [1.0, 1.0, swap, 1.0])
    } else {
        ([1.0; 4], [1.0; 4])
    };
    let outgoing_low_pass = if decision.eq_sweep {
        Some((20_000.0 * (500.0_f32 / 20_000.0).powf(progress)).clamp(500.0, 20_000.0))
    } else {
        None
    };
    let incoming_high_pass = if decision.eq_sweep {
        Some(500.0 * (30.0_f32 / 500.0).powf(progress))
    } else {
        None
    };
    let elapsed_ms = run.prepared.decision.fade_duration_ms as f32 * progress;
    let beat_ms = run
        .current
        .bpm
        .filter(|bpm| bpm.is_finite() && *bpm > 0.0)
        .map(|bpm| 60_000.0 / (bpm as f32 * decks[outgoing].status().playback_rate.max(0.8)))
        .unwrap_or(500.0);
    let dub_pulse = (elapsed_ms / beat_ms.max(1.0) * std::f32::consts::PI)
        .sin()
        .abs();
    let reverb = if run.prepared.decision.spring_auto_dub {
        0.22 + dub_pulse * 0.14
    } else {
        0.0
    };
    let tape = if run.prepared.decision.spring_auto_dub {
        0.10 + dub_pulse * 0.10
    } else {
        0.0
    };

    decks[outgoing]
        .update_controls(AudioControlUpdate {
            master_gain: Some(outgoing_gain),
            stem_gains: Some(outgoing_stems),
            low_pass_enabled: Some(decision.eq_sweep),
            low_pass_hz: outgoing_low_pass,
            spring_mix: Some(reverb),
            spring_decay: Some(0.84),
            tape_mix: Some(tape),
            tape_feedback: Some(0.42),
            tape_delay_ms: Some(beat_ms.clamp(80.0, 1_000.0)),
            ..AudioControlUpdate::default()
        })
        .map_err(|error| format!("could not update outgoing mix deck: {error}"))?;
    decks[incoming]
        .update_controls(AudioControlUpdate {
            master_gain: Some(incoming_gain),
            stem_gains: Some(incoming_stems),
            high_pass_enabled: Some(decision.eq_sweep),
            high_pass_hz: incoming_high_pass,
            spring_mix: Some(if run.prepared.decision.spring_auto_dub {
                reverb * 0.45
            } else {
                0.0
            }),
            spring_decay: Some(0.82),
            ..AudioControlUpdate::default()
        })
        .map_err(|error| format!("could not update incoming mix deck: {error}"))?;
    Ok(())
}

fn finish_crossfade(
    run: &mut ActiveMix,
    database: &Arc<Mutex<Connection>>,
    decks: &[audio::AudioEngine; 2],
    shared_status: &Arc<Mutex<MixStatus>>,
) -> Result<(), String> {
    let incoming = 1 - run.active_deck;
    let outgoing = run.active_deck;
    decks[outgoing]
        .stop()
        .map_err(|error| format!("could not retire the outgoing audio deck: {error}"))?;
    decks[incoming]
        .update_controls(AudioControlUpdate {
            master_gain: Some(DEFAULT_MASTER_GAIN),
            stem_gains: Some([1.0; 4]),
            high_pass_enabled: Some(false),
            high_pass_hz: Some(30.0),
            low_pass_enabled: Some(false),
            low_pass_hz: Some(18_000.0),
            spring_mix: Some(0.0),
            spring_decay: Some(0.78),
            tape_mix: Some(0.0),
            tape_feedback: Some(0.35),
            kill_bass: Some(false),
            kill_mid: Some(false),
            kill_high: Some(false),
            ..AudioControlUpdate::default()
        })
        .map_err(|error| format!("could not restore incoming deck controls: {error}"))?;
    advance_to_prepared_track(run, database, decks, shared_status, incoming)
}

fn advance_to_prepared_track(
    run: &mut ActiveMix,
    database: &Arc<Mutex<Connection>>,
    decks: &[audio::AudioEngine; 2],
    shared_status: &Arc<Mutex<MixStatus>>,
    new_active_deck: usize,
) -> Result<(), String> {
    if let Some(preset) = run.pending_preset.take() {
        run.preset = preset;
    }
    if let Some(intensity_override) = run.pending_intensity_override.take() {
        run.intensity_override = intensity_override;
    }
    let old_track_id = run.current.id;
    run.recent_track_ids.push_front(old_track_id);
    run.recent_track_ids.truncate(RECENT_TRACK_MEMORY);
    run.current = run.prepared.next.clone();
    run.style = run.prepared.decision.style;
    run.active_deck = new_active_deck;
    run.last_position_ms = run.prepared.decision.incoming_cue_ms;
    run.phase = RunnerPhase::Playing;
    decks[new_active_deck].set_auto_rewind(run.style == MixStyle::AggressiveClash);

    let next_deck = 1 - new_active_deck;
    let source_rate = decks[new_active_deck].status().playback_rate;
    let prepared = prepare_next(
        &run.current,
        run.preset,
        Some(run.style),
        source_rate,
        &run.recent_track_ids,
        database,
        &decks[next_deck],
        run.intensity_override,
    )?;
    run.prepared = prepared;
    publish_status(run, decks, shared_status, MixPhase::Playing, 0.0);
    Ok(())
}

fn publish_status(
    run: &ActiveMix,
    decks: &[audio::AudioEngine; 2],
    shared_status: &Arc<Mutex<MixStatus>>,
    phase: MixPhase,
    progress: f32,
) {
    let position_ms = audio_position_ms(&decks[run.active_deck].status());
    publish(
        shared_status,
        MixStatus {
            active: true,
            phase,
            preset: Some(run.pending_preset.unwrap_or(run.preset)),
            style: Some(run.style),
            intensity_override: run
                .pending_intensity_override
                .unwrap_or(run.intensity_override),
            current_track_id: Some(run.current.id),
            current_title: Some(run.current.title.clone()),
            next_track_id: Some(run.prepared.next.id),
            next_title: Some(run.prepared.next.title.clone()),
            position_ms,
            transition_progress: progress,
            active_deck: Some(run.active_deck),
            decision: Some(run.prepared.decision.clone()),
            error: None,
        },
    );
}

fn publish(status: &Arc<Mutex<MixStatus>>, value: MixStatus) {
    if let Ok(mut current) = status.lock() {
        *current = value;
    }
}

fn set_error(status: &Arc<Mutex<MixStatus>>, error: String) {
    if let Ok(mut current) = status.lock() {
        current.active = false;
        current.phase = MixPhase::Error;
        current.error = Some(error);
    }
}

fn stop_decks(decks: &[audio::AudioEngine; 2]) {
    for deck in decks {
        deck.set_auto_rewind(false);
        let _ = deck.stop();
        let _ = reset_deck(deck, DEFAULT_MASTER_GAIN, 1.0);
    }
}

fn reset_deck(deck: &audio::AudioEngine, gain: f32, playback_rate: f32) -> Result<(), String> {
    deck.update_controls(AudioControlUpdate {
        master_gain: Some(gain),
        playback_rate: Some(playback_rate.clamp(0.8, 1.2)),
        stem_gains: Some([1.0; 4]),
        high_pass_enabled: Some(false),
        low_pass_enabled: Some(false),
        bass_gain: Some(1.0),
        mid_gain: Some(1.0),
        high_gain: Some(1.0),
        kill_bass: Some(false),
        kill_mid: Some(false),
        kill_high: Some(false),
        tape_mix: Some(0.0),
        tape_feedback: Some(0.35),
        spring_mix: Some(0.0),
        spring_decay: Some(0.78),
        ..AudioControlUpdate::default()
    })?;
    Ok(())
}

fn mir_profile(track: &database::MixTrackRecord) -> MirProfile<'_> {
    MirProfile {
        bpm: track.bpm,
        musical_key: track.musical_key.as_deref(),
        integrated_lufs: track.integrated_lufs,
        mood_valence: track.mood_valence,
        mood_arousal: track.mood_arousal,
        mood_label: track.mood_label.as_deref(),
        rhythm_pattern: track.rhythm_pattern.as_deref(),
        rhythm_confidence: track.rhythm_confidence,
    }
}

fn route_style(
    preset: MixPreset,
    current: &database::MixTrackRecord,
    incoming: Option<&database::MixTrackRecord>,
    previous: Option<MixStyle>,
) -> MixStyle {
    match preset {
        MixPreset::SmoothSelector => MixStyle::DeepRoots,
        MixPreset::ClubHype => MixStyle::ClubHype,
        MixPreset::AggressiveClash => MixStyle::AggressiveClash,
        MixPreset::FreestyleMaestro => classify_maestro_style(current, incoming, previous),
    }
}

fn effective_style(
    preset: MixPreset,
    current: &database::MixTrackRecord,
    incoming: Option<&database::MixTrackRecord>,
    previous: Option<MixStyle>,
    intensity_override: Option<u8>,
) -> MixStyle {
    intensity_override
        .map(intensity_to_style)
        .unwrap_or_else(|| route_style(preset, current, incoming, previous))
}

fn intensity_to_style(intensity: u8) -> MixStyle {
    match intensity {
        1 => MixStyle::DeepRoots,
        2 => MixStyle::ClubHype,
        3 | 4 => MixStyle::AggressiveClash,
        _ => MixStyle::DeepRoots,
    }
}

fn classify_maestro_style(
    current: &database::MixTrackRecord,
    incoming: Option<&database::MixTrackRecord>,
    previous: Option<MixStyle>,
) -> MixStyle {
    if let Some(style) = acoustic_style(current.acoustic_tag.as_deref()) {
        return style;
    }
    if let Some(incoming) = incoming {
        if let Some(style) = acoustic_style(incoming.acoustic_tag.as_deref()) {
            // Prefer the playing deck's metadata when it is explicit; this branch
            // is only reached when its tag is missing or unrecognized.
            return style;
        }
    }
    let mut tracks = vec![current];
    if let Some(incoming) = incoming {
        tracks.push(incoming);
    }
    for preferred in [
        MixStyle::AggressiveClash,
        MixStyle::ClubHype,
        MixStyle::DeepRoots,
    ] {
        for track in &tracks {
            if manual_tag_style(&track.tags) == Some(preferred) {
                return preferred;
            }
        }
    }

    let mood_tracks = [
        Some((current.mood_valence, current.mood_arousal, 0.65)),
        incoming.map(|track| (track.mood_valence, track.mood_arousal, 0.35)),
    ];
    let mood_totals = mood_tracks
        .into_iter()
        .flatten()
        .filter_map(|(valence, arousal, weight)| Some((valence?, arousal?, weight)))
        .fold((0.0, 0.0, 0.0), |(v, a, w), (next_v, next_a, next_w)| {
            (v + next_v * next_w, a + next_a * next_w, w + next_w)
        });
    if mood_totals.2 > 0.0 {
        let valence = mood_totals.0 / mood_totals.2;
        let arousal = mood_totals.1 / mood_totals.2;
        if arousal >= 0.72 && valence <= -0.25 {
            return MixStyle::AggressiveClash;
        }
        if arousal >= 0.62 && valence >= 0.20 {
            return MixStyle::ClubHype;
        }
        if arousal <= 0.32 {
            return MixStyle::DeepRoots;
        }
    }

    let scores = [
        current.sentiment.map(|score| (score, 0.65)),
        incoming.and_then(|track| track.sentiment.map(|score| (score, 0.35))),
    ];
    let weighted = scores
        .into_iter()
        .flatten()
        .map(|(score, weight)| (score.clamp(-1.0, 1.0), weight))
        .fold((0.0, 0.0), |(sum, weights), (score, weight)| {
            (sum + score * weight, weights + weight)
        });
    if weighted.1 > 0.0 {
        let sentiment = weighted.0 / weighted.1;
        if sentiment <= -0.35 {
            return MixStyle::AggressiveClash;
        }
        let energy = current
            .energy_density
            .into_iter()
            .chain(incoming.and_then(|track| track.energy_density))
            .filter(|value| value.is_finite())
            .fold(0.0_f64, f64::max);
        if sentiment >= 0.30 && energy >= 0.42 {
            return MixStyle::ClubHype;
        }
    }
    if current
        .sentiment_label
        .as_deref()
        .is_some_and(|label| label.eq_ignore_ascii_case("negative"))
    {
        return MixStyle::AggressiveClash;
    }
    if current
        .sentiment_label
        .as_deref()
        .is_some_and(|label| label.eq_ignore_ascii_case("positive"))
        && current.energy_density.is_some_and(|energy| energy >= 0.42)
    {
        return MixStyle::ClubHype;
    }
    if current.energy_density.is_some_and(|energy| energy <= 0.36) {
        return MixStyle::DeepRoots;
    }
    previous.unwrap_or(MixStyle::DeepRoots)
}

fn acoustic_style(tag: Option<&str>) -> Option<MixStyle> {
    match tag?.trim().to_ascii_lowercase().as_str() {
        "aggressiv" | "aggressive" | "clash" => Some(MixStyle::AggressiveClash),
        "hype" | "club" => Some(MixStyle::ClubHype),
        "deep" | "conscious" | "roots" => Some(MixStyle::DeepRoots),
        _ => None,
    }
}

fn manual_tag_style(tags: &[(String, String)]) -> Option<MixStyle> {
    let has_any = |needles: &[&str]| {
        tags.iter().any(|(_, tag)| {
            let tag = tag.to_ascii_lowercase();
            needles.iter().any(|needle| tag.contains(needle))
        })
    };
    if has_any(&["aggressiv", "aggressive", "clash", "battle", "war"]) {
        Some(MixStyle::AggressiveClash)
    } else if has_any(&["hype", "club", "dance", "party"]) {
        Some(MixStyle::ClubHype)
    } else if has_any(&["deep", "roots", "conscious", "dub"]) {
        Some(MixStyle::DeepRoots)
    } else {
        None
    }
}

fn calculate_tempo_match(
    source_bpm: Option<f64>,
    source_playback_rate: f32,
    incoming_bpm: Option<f64>,
) -> TempoMatch {
    let source_bpm = source_bpm
        .filter(|bpm| bpm.is_finite() && (20.0..=400.0).contains(bpm))
        .map(|bpm| bpm * f64::from(source_playback_rate.clamp(0.8, 1.2)));
    let incoming_bpm = incoming_bpm.filter(|bpm| bpm.is_finite() && (20.0..=400.0).contains(bpm));
    let Some((source_bpm, incoming_bpm)) = source_bpm.zip(incoming_bpm) else {
        return TempoMatch {
            mode: TempoMode::Normal,
            playback_rate: 1.0,
            matched_bpm: incoming_bpm,
            relative_error: None,
        };
    };

    // Half-/double-time are beat interpretations, while WSOLA applies only a gentle ±20% speed
    // change. This keeps rhythm matching broad without changing the voice/instrument pitch.
    [
        (0.5_f64, TempoMode::HalfTime),
        (1.0, TempoMode::Normal),
        (2.0, TempoMode::DoubleTime),
    ]
    .into_iter()
    .map(|(beat_factor, mode)| {
        let interpreted_bpm = incoming_bpm * beat_factor;
        let playback_rate = (source_bpm / interpreted_bpm).clamp(0.8, 1.2) as f32;
        let matched_bpm = interpreted_bpm * f64::from(playback_rate);
        let relative_error = ((matched_bpm - source_bpm) / source_bpm).abs();
        let preference = if mode == TempoMode::Normal {
            0.0
        } else {
            1.0e-9
        };
        (
            relative_error + preference,
            playback_rate,
            mode,
            matched_bpm,
            relative_error,
        )
    })
    .min_by(|left, right| left.0.total_cmp(&right.0))
    .map(
        |(_, playback_rate, mode, matched_bpm, relative_error)| TempoMatch {
            mode,
            playback_rate,
            matched_bpm: Some(matched_bpm),
            relative_error: Some(relative_error),
        },
    )
    .unwrap_or(TempoMatch {
        mode: TempoMode::Normal,
        playback_rate: 1.0,
        matched_bpm: Some(incoming_bpm),
        relative_error: None,
    })
}

fn choose_candidate_for_profile(
    current: &database::MixTrackRecord,
    candidates: &[database::MixTrackRecord],
    preset: MixPreset,
    style: MixStyle,
    source_playback_rate: f32,
    recent_track_ids: &VecDeque<i64>,
    intensity_override: Option<u8>,
    active_stem_count: usize,
) -> Option<(database::MixTrackRecord, Option<WordPlayMatch>)> {
    let same_riddim_exists = style == MixStyle::ClubHype
        && candidates
            .iter()
            .any(|candidate| shares_riddim(current, candidate));
    let scored = candidates
        .iter()
        .filter(|candidate| {
            candidate.id != current.id
                && track_stem_files_for_profile(candidate, active_stem_count).is_ok()
        })
        .filter_map(|candidate| {
            let same_riddim = shares_riddim(current, candidate);
            if same_riddim_exists && !same_riddim {
                return None;
            }
            let word_play = find_word_play(current, candidate);
            let tempo = calculate_tempo_match(current.bpm, source_playback_rate, candidate.bpm);
            let mut score = 0.0;
            if same_riddim {
                score += if style == MixStyle::ClubHype {
                    100.0
                } else {
                    22.0
                };
            }
            if intensity_override.is_none()
                && preset != MixPreset::FreestyleMaestro
                && route_style(MixPreset::FreestyleMaestro, candidate, None, None) == style
            {
                score += 14.0;
            }
            if let Some(error) = tempo.relative_error {
                score -= error * 45.0;
            }
            score += key_compatibility(
                current.musical_key.as_deref(),
                candidate.musical_key.as_deref(),
            ) * 6.0;
            score += transition_score(mir_profile(current), mir_profile(candidate)) * 12.0;
            if style == MixStyle::AggressiveClash {
                if let Some(word_play) = &word_play {
                    score += word_play.matched_words.len() as f64 * 24.0;
                }
            }
            if recent_track_ids.contains(&candidate.id) {
                score -= 80.0;
            }
            if current.artist.eq_ignore_ascii_case(&candidate.artist) {
                score -= 1.0;
            }
            Some((score, candidate.id, candidate.clone(), word_play))
        })
        .collect::<Vec<_>>();

    let has_semantic_route = style == MixStyle::AggressiveClash
        && scored
            .iter()
            .any(|(_, _, _, word_play)| word_play.is_some());
    scored
        .into_iter()
        .filter(|(_, _, _, word_play)| !has_semantic_route || word_play.is_some())
        .max_by(|left, right| {
            left.0
                .total_cmp(&right.0)
                .then_with(|| right.1.cmp(&left.1))
        })
        .map(|(_, _, track, word_play)| (track, word_play))
}

fn build_decision(
    preset: MixPreset,
    style: MixStyle,
    current: &database::MixTrackRecord,
    next: &database::MixTrackRecord,
    tempo: TempoMatch,
    word_play: Option<WordPlayMatch>,
    same_riddim_family: bool,
    source_playback_rate: f32,
    intensity_override: Option<u8>,
) -> MixDecision {
    let effective_bpm = current
        .bpm
        .filter(|bpm| bpm.is_finite() && *bpm > 0.0)
        .map(|bpm| bpm * f64::from(source_playback_rate.clamp(0.8, 1.2)));
    let one_bar_ms = effective_bpm
        .map(|bpm| (4.0 * 60_000.0 / bpm).round() as u64)
        .unwrap_or(2_000)
        .clamp(500, 20_000);
    let duration = current
        .duration_ms
        .and_then(|value| u64::try_from(value).ok());
    let chorus_cut = find_chorus_markers(current)
        .into_iter()
        .find(|time| *time >= CHORUS_MIN_START_MS);
    let source_semantic_phrase = semantic_tail_tokens(current).join(" ");
    let incoming_chorus = find_chorus_markers(next)
        .into_iter()
        .find(|time| *time >= CHORUS_MIN_START_MS);
    let incoming_beats = track_beat_grid(next).beat_times_seconds;
    let transition_style = match style {
        MixStyle::DeepRoots => TransitionStyle::DeepRoots,
        MixStyle::ClubHype => TransitionStyle::ClubHype,
        MixStyle::AggressiveClash => TransitionStyle::AggressiveClash,
    };
    let mir_plan = plan_transition(
        mir_profile(current),
        mir_profile(next),
        transition_style,
        same_riddim_family,
        &incoming_beats,
        incoming_chorus,
    );

    let (transition, transition_at_ms, fade_duration_ms, mut incoming_cue_ms, rationale) =
        match style {
            MixStyle::DeepRoots => {
                let fade = crossfade_duration_ms(
                    current.bpm,
                    source_playback_rate,
                    mir_plan.crossfade_bars,
                );
                let requested_start = duration.map(|duration| duration.saturating_sub(fade));
                let start = requested_start.map(|time| snap_to_previous_bar(current, time));
                (
                    TransitionKind::LongCrossfade,
                    start,
                    fade,
                    mir_plan.ideal_drop_cue_ms,
                    if same_riddim_family {
                        format!("Deep/Roots: {}-bar MIR-scored equal-power crossfade; same riddim family, mood, rhythm, key and loudness metadata guide the bass hand-off and local Auto-Dub.", mir_plan.crossfade_bars)
                    } else {
                        format!("Deep/Roots: {}-bar MIR-scored equal-power crossfade; local mood, rhythm, key and loudness metadata guide the EQ sweep and drop cue.", mir_plan.crossfade_bars)
                    },
                )
            }
            MixStyle::ClubHype => {
                let cut = chorus_cut
                    .or_else(|| duration.map(|duration| duration.saturating_sub(one_bar_ms)))
                    .map(|time| snap_to_previous_bar(current, time));
                let cue = mir_plan.ideal_drop_cue_ms;
                (
                    TransitionKind::HardChorusCut,
                    cut,
                    0,
                    cue,
                    if same_riddim_family {
                        "Club/Hype: hard cut at the next repeated-lyric chorus; incoming track is kept in the same automatic/manual riddim family.".to_owned()
                    } else {
                        "Club/Hype: hard cut at the next repeated-lyric chorus; no matching riddim family was available, so tempo/key compatibility is used.".to_owned()
                    },
                )
            }
            MixStyle::AggressiveClash => {
                let arm = duration.map(|duration| duration.saturating_sub(one_bar_ms));
                let cue = word_play
                    .as_ref()
                    .map(|matched| matched.cue_ms)
                    .unwrap_or(mir_plan.ideal_drop_cue_ms);
                (
                    TransitionKind::SemanticRewindCut,
                    arm,
                    crossfade_duration_ms(
                        current.bpm,
                        source_playback_rate,
                        if intensity_override == Some(4) {
                            1
                        } else {
                            mir_plan.crossfade_bars
                        },
                    ),
                    cue,
                    if word_play.is_some() {
                        "Aggressiv/Clash: local Whisper-word match selects and cues the incoming phrase; Auto-Rewind arms the hand-off and all three isolator kill switches drop the outgoing fader.".to_owned()
                    } else {
                        "Aggressiv/Clash: no Whisper word-play match was found; use the best local tempo/key candidate, Auto-Rewind, and fader kill switches.".to_owned()
                    },
                )
            }
        };
    incoming_cue_ms = incoming_cue_ms.min(86_400_000);
    if let Some(next_duration) = next
        .duration_ms
        .and_then(|duration| u64::try_from(duration).ok())
    {
        if next_duration == 0 || incoming_cue_ms >= next_duration {
            incoming_cue_ms = 0;
        }
    }

    MixDecision {
        preset,
        style,
        intensity_override,
        transition,
        source_track_id: current.id,
        next_track_id: next.id,
        source_bpm: effective_bpm,
        incoming_bpm: next.bpm,
        matched_bpm: tempo.matched_bpm,
        tempo_mode: tempo.mode,
        playback_rate: tempo.playback_rate,
        tempo_error_percent: tempo.relative_error.map(|error| error * 100.0),
        transition_at_ms,
        fade_duration_ms,
        incoming_cue_ms,
        chorus_cut_ms: chorus_cut,
        semantic_phrase: if source_semantic_phrase.is_empty() {
            None
        } else {
            Some(source_semantic_phrase)
        },
        semantic_match: word_play.as_ref().map(|matched| matched.phrase.clone()),
        semantic_match_words: word_play
            .as_ref()
            .map(|matched| matched.matched_words.len())
            .unwrap_or(0),
        same_riddim_family,
        mir_compatibility: mir_plan.compatibility,
        mood_compatibility: mir_plan.mood_compatibility,
        rhythm_compatibility: mir_plan.rhythm_compatibility,
        key_compatibility: mir_plan.key_compatibility,
        loudness_delta_db: mir_plan.loudness_delta_db,
        ideal_drop_cue_ms: incoming_cue_ms,
        incoming_curve_exponent: mir_plan.incoming_curve_exponent,
        bass_handoff_start: mir_plan.bass_handoff_start,
        bass_handoff_end: mir_plan.bass_handoff_end,
        eq_sweep: mir_plan.eq_sweep,
        bass_swap: mir_plan.bass_swap,
        spring_auto_dub: style == MixStyle::DeepRoots,
        auto_rewind: style == MixStyle::AggressiveClash,
        fader_kill_switches: style == MixStyle::AggressiveClash,
        rationale,
    }
}

fn crossfade_duration_ms(bpm: Option<f64>, playback_rate: f32, bars: u32) -> u64 {
    let duration = bpm
        .filter(|value| value.is_finite() && *value > 0.0)
        .map(|value| {
            f64::from(bars) * 4.0 * 60_000.0 / (value * f64::from(playback_rate.clamp(0.8, 1.2)))
        })
        .unwrap_or(f64::from(bars) * 2_000.0);
    (duration.round() as u64).clamp(MIN_CROSSFADE_MS, MAX_CROSSFADE_MS)
}

fn find_chorus_markers(track: &database::MixTrackRecord) -> Vec<u64> {
    let normalized = track
        .words
        .iter()
        .filter(|word| word.confidence >= 0.20)
        .filter_map(|word| {
            let token = normalize_word(&word.text)?;
            Some((
                token,
                word.start_ms.max(0) as u64,
                word.end_ms.max(0) as u64,
            ))
        })
        .collect::<Vec<_>>();
    let phrase_size = 4;
    let mut occurrences: HashMap<String, Vec<u64>> = HashMap::new();
    for window in normalized.windows(phrase_size) {
        if window
            .windows(2)
            .any(|pair| pair[1].1.saturating_sub(pair[0].2) > 1_200)
        {
            continue;
        }
        let phrase = window
            .iter()
            .map(|(token, _, _)| token.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        occurrences.entry(phrase).or_default().push(window[0].1);
    }
    let mut markers = occurrences
        .into_values()
        .filter(|times| {
            times.len() >= 2
                && times.iter().any(|first| {
                    times.iter().any(|second| {
                        second.saturating_sub(*first) >= CHORUS_MIN_REPEAT_GAP_MS as u64
                    })
                })
        })
        .flatten()
        .collect::<Vec<_>>();
    markers.sort_unstable();
    markers.dedup();
    markers
}

fn snap_to_previous_bar(track: &database::MixTrackRecord, requested_ms: u64) -> u64 {
    let beats = parse_beat_grid(track);
    let bar = beats
        .iter()
        .enumerate()
        .filter(|(_, beat_ms)| **beat_ms <= requested_ms)
        .filter(|(index, _)| index % 4 == 0)
        .map(|(_, beat_ms)| *beat_ms)
        .last();
    bar.unwrap_or_else(|| {
        let effective_bpm = track
            .bpm
            .filter(|bpm| bpm.is_finite() && *bpm > 0.0)
            .unwrap_or(120.0);
        let bar_ms = (4.0 * 60_000.0 / effective_bpm).round() as u64;
        if bar_ms == 0 {
            requested_ms
        } else {
            requested_ms / bar_ms * bar_ms
        }
    })
}

fn parse_beat_grid(track: &database::MixTrackRecord) -> Vec<u64> {
    track
        .beat_grid_json
        .as_deref()
        .and_then(|json| serde_json::from_str::<Vec<f64>>(json).ok())
        .unwrap_or_default()
        .into_iter()
        .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
        .map(|seconds| (seconds * 1_000.0).round() as u64)
        .collect()
}

fn semantic_tail_tokens(track: &database::MixTrackRecord) -> Vec<String> {
    let duration = track.duration_ms.unwrap_or(i64::MAX);
    let mut tail = track
        .words
        .iter()
        .filter(|word| word.confidence >= 0.20)
        .filter(|word| word.start_ms >= duration.saturating_sub(25_000))
        .filter_map(|word| normalize_word(&word.text))
        .filter(|token| !is_semantic_stop_word(token))
        .collect::<Vec<_>>();
    if tail.is_empty() {
        tail = track
            .words
            .iter()
            .rev()
            .filter(|word| word.confidence >= 0.20)
            .filter_map(|word| normalize_word(&word.text))
            .filter(|token| !is_semantic_stop_word(token))
            .take(12)
            .collect::<Vec<_>>();
        tail.reverse();
    }
    let mut unique = Vec::new();
    for token in tail.into_iter().rev() {
        if !unique.contains(&token) {
            unique.push(token);
        }
        if unique.len() == 6 {
            break;
        }
    }
    unique.reverse();
    unique
}

fn is_semantic_stop_word(token: &str) -> bool {
    matches!(
        token,
        "a" | "an"
            | "and"
            | "are"
            | "as"
            | "at"
            | "be"
            | "been"
            | "but"
            | "by"
            | "for"
            | "from"
            | "he"
            | "her"
            | "him"
            | "his"
            | "i"
            | "in"
            | "is"
            | "it"
            | "its"
            | "me"
            | "my"
            | "of"
            | "on"
            | "or"
            | "our"
            | "she"
            | "the"
            | "their"
            | "them"
            | "they"
            | "this"
            | "that"
            | "to"
            | "was"
            | "we"
            | "were"
            | "with"
            | "you"
            | "your"
    )
}

fn normalize_word(value: &str) -> Option<String> {
    let token = value
        .trim()
        .to_lowercase()
        .chars()
        .filter(|character| character.is_alphanumeric() || *character == '\'')
        .collect::<String>();
    (token.chars().count() >= 2).then_some(token)
}

fn find_word_play(
    current: &database::MixTrackRecord,
    candidate: &database::MixTrackRecord,
) -> Option<WordPlayMatch> {
    let cue = semantic_tail_tokens(current);
    if cue.is_empty() {
        return None;
    }
    let cue_set = cue.iter().cloned().collect::<HashSet<_>>();
    let normalized = candidate
        .words
        .iter()
        .filter(|word| word.confidence >= 0.20)
        .filter_map(|word| {
            normalize_word(&word.text).map(|token| (token, word.start_ms.max(0) as u64))
        })
        .collect::<Vec<_>>();
    if normalized.is_empty() {
        return None;
    }
    let window_size = cue.len().max(5).min(10);
    let mut best: Option<(usize, u64, Vec<String>)> = None;
    for start in 0..normalized.len() {
        let terms = normalized
            .iter()
            .skip(start)
            .take(window_size)
            .map(|(token, _)| token)
            .filter(|token| cue_set.contains(*token))
            .cloned()
            .collect::<HashSet<_>>();
        if terms.is_empty() {
            continue;
        }
        let time = normalized
            .iter()
            .skip(start)
            .take(window_size)
            .find_map(|(token, time)| cue_set.contains(token).then_some(*time))
            .unwrap_or(normalized[start].1);
        let mut matched = terms.into_iter().collect::<Vec<_>>();
        matched.sort();
        if best.as_ref().map_or(true, |(count, best_time, _)| {
            matched.len() > *count || (matched.len() == *count && time < *best_time)
        }) {
            best = Some((matched.len(), time, matched));
        }
    }
    let (_count, cue_ms, matched_words) = best?;
    let phrase = normalized
        .iter()
        .filter(|(_, time)| *time >= cue_ms)
        .take(window_size)
        .map(|(token, _)| token.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    Some(WordPlayMatch {
        cue_ms,
        matched_words,
        phrase,
    })
}

fn shares_riddim(left: &database::MixTrackRecord, right: &database::MixTrackRecord) -> bool {
    if left.riddim_family_id.is_some() && left.riddim_family_id == right.riddim_family_id {
        return true;
    }
    let left_tags = left
        .tags
        .iter()
        .filter(|(namespace, _)| namespace.eq_ignore_ascii_case("riddim"))
        .map(|(_, tag)| tag.trim().to_lowercase())
        .collect::<HashSet<_>>();
    right.tags.iter().any(|(namespace, tag)| {
        namespace.eq_ignore_ascii_case("riddim") && left_tags.contains(&tag.trim().to_lowercase())
    })
}

fn key_compatibility(left: Option<&str>, right: Option<&str>) -> f64 {
    let (Some(left), Some(right)) = (
        parse_key(left.unwrap_or_default()),
        parse_key(right.unwrap_or_default()),
    ) else {
        return 0.0;
    };
    let semitones = (left.0 as i8 - right.0 as i8)
        .unsigned_abs()
        .min(12 - (left.0 as i8 - right.0 as i8).unsigned_abs()) as u8;
    if left == right {
        1.0
    } else if left.0 == right.0 {
        0.8
    } else if left.1 != right.1 && ((left.0 + 3) % 12 == right.0 || (right.0 + 3) % 12 == left.0) {
        0.65
    } else {
        let circle = [0_u8, 7, 2, 9, 4, 11, 6, 1, 8, 3, 10, 5];
        let left_circle = circle
            .iter()
            .position(|pitch| *pitch == left.0)
            .unwrap_or(0) as u8;
        let right_circle = circle
            .iter()
            .position(|pitch| *pitch == right.0)
            .unwrap_or(0) as u8;
        let circle_distance = left_circle
            .abs_diff(right_circle)
            .min(12_u8 - left_circle.abs_diff(right_circle));
        if left.1 == right.1 && circle_distance == 1 {
            0.5
        } else if semitones == 1 {
            0.25
        } else {
            0.0
        }
    }
}

fn parse_key(value: &str) -> Option<(u8, bool)> {
    let compact = value
        .trim()
        .to_ascii_lowercase()
        .replace('♯', "#")
        .replace('♭', "b");
    if compact.is_empty() {
        return None;
    }
    let is_minor = compact.contains("minor") || compact.ends_with('m');
    let chars = compact.chars().collect::<Vec<_>>();
    let note_index = chars
        .iter()
        .position(|character| ('a'..='g').contains(character))?;
    let note = chars[note_index].to_ascii_uppercase();
    let mut accidental = None;
    if let Some(next) = chars.get(note_index + 1) {
        if *next == '#' || *next == 'b' {
            accidental = Some(*next);
        }
    }
    let pitch = match note {
        'C' => 0_i16,
        'D' => 2,
        'E' => 4,
        'F' => 5,
        'G' => 7,
        'A' => 9,
        'B' => 11,
        _ => return None,
    } + match accidental {
        Some('#') => 1,
        Some('b') => -1,
        _ => 0,
    };
    Some((pitch.rem_euclid(12) as u8, is_minor))
}

fn track_stem_files_for_profile(
    track: &database::MixTrackRecord,
    active_stem_count: usize,
) -> Result<audio::StemFiles, String> {
    if !matches!(active_stem_count, 2 | 4) {
        return Err("active stem profile must be 2 or 4".to_owned());
    }
    let mut paths = HashMap::<String, PathBuf>::with_capacity(4);
    for stem in &track.stems {
        let kind = stem.kind.trim().to_ascii_lowercase();
        if !matches!(
            kind.as_str(),
            "acapella" | "instrumental" | "bass" | "drums"
        ) || (active_stem_count == 2 && !matches!(kind.as_str(), "acapella" | "instrumental"))
        {
            continue;
        }
        let path = PathBuf::from(&stem.file_path);
        import::ensure_local_path(&path)
            .map_err(|error| format!("{kind} stem must be stored locally: {error:#}"))?;
        if !path.is_file() {
            return Err(format!("local {kind} stem is missing: {}", path.display()));
        }
        if paths.insert(kind.clone(), path).is_some() {
            return Err(format!(
                "track {} has duplicate {kind} stem paths",
                track.id
            ));
        }
    }
    let mut take = |kind: &str| {
        paths
            .remove(kind)
            .ok_or_else(|| format!("track {} is missing its {kind} stem", track.id))
    };
    let acapella = take("acapella")?;
    let instrumental = take("instrumental")?;
    let (bass, drums) = if active_stem_count == 4 {
        (take("bass")?, take("drums")?)
    } else {
        (instrumental.clone(), instrumental.clone())
    };
    Ok(audio::StemFiles {
        acapella,
        instrumental,
        bass,
        drums,
    })
}

fn track_beat_grid(track: &database::MixTrackRecord) -> audio::BeatGrid {
    audio::BeatGrid {
        beat_times_seconds: track
            .beat_grid_json
            .as_deref()
            .and_then(|json| serde_json::from_str::<Vec<f64>>(json).ok())
            .unwrap_or_default()
            .into_iter()
            .filter(|beat| beat.is_finite() && *beat >= 0.0)
            .collect(),
        bpm: track.bpm,
    }
}

fn audio_position_ms(status: &audio::AudioStatus) -> Option<u64> {
    if status.sample_rate == 0 {
        return None;
    }
    let seconds = status.position_frames as f64 * f64::from(status.playback_rate)
        / f64::from(status.sample_rate);
    Some((seconds.max(0.0) * 1_000.0).round().min(u64::MAX as f64) as u64)
}

#[cfg(test)]
mod tests {
    use super::{
        build_decision, calculate_tempo_match, classify_maestro_style, find_chorus_markers,
        find_word_play, intensity_to_style, route_style, track_stem_files_for_profile, MixPreset,
        MixStyle, TempoMode, TransitionKind,
    };
    use crate::database::{LyricWord, MixTrackRecord, StemPath};

    fn track(id: i64) -> MixTrackRecord {
        MixTrackRecord {
            id,
            file_path: format!("track-{id}.wav"),
            title: format!("Track {id}"),
            artist: "Artist".to_owned(),
            duration_ms: Some(180_000),
            bpm: Some(120.0),
            beat_grid_json: None,
            sentiment: None,
            sentiment_label: None,
            acoustic_tag: None,
            energy_density: None,
            musical_key: None,
            riddim_family_id: None,
            integrated_lufs: None,
            mood_valence: None,
            mood_arousal: None,
            mood_label: None,
            rhythm_pattern: None,
            rhythm_confidence: None,
            tags: Vec::new(),
            stems: Vec::new(),
            words: Vec::new(),
        }
    }

    fn word(text: &str, start_ms: i64) -> LyricWord {
        LyricWord {
            text: text.to_owned(),
            start_ms,
            end_ms: start_ms + 250,
            confidence: 0.95,
        }
    }

    #[test]
    fn two_stem_profile_does_not_require_unused_bass_or_drums_files() {
        use std::{
            fs,
            time::{SystemTime, UNIX_EPOCH},
        };

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "championvibe-two-stem-test-{}-{nonce}",
            std::process::id(),
        ));
        fs::create_dir_all(&directory).unwrap();
        let acapella = directory.join("acapella.wav");
        let instrumental = directory.join("instrumental.wav");
        fs::write(&acapella, b"local acapella").unwrap();
        fs::write(&instrumental, b"local instrumental").unwrap();

        let mut current = track(1);
        current.stems = vec![
            StemPath {
                kind: "acapella".to_owned(),
                file_path: acapella.to_string_lossy().into_owned(),
            },
            StemPath {
                kind: "instrumental".to_owned(),
                file_path: instrumental.to_string_lossy().into_owned(),
            },
        ];
        let files = track_stem_files_for_profile(&current, 2).unwrap();
        assert_eq!(files.acapella, acapella);
        assert_eq!(files.instrumental, instrumental);
        assert_eq!(files.bass, instrumental);
        assert_eq!(files.drums, instrumental);
        assert!(track_stem_files_for_profile(&current, 4).is_err());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn preset_one_two_three_and_four_take_the_declared_routes() {
        let mut current = track(1);
        current.acoustic_tag = Some("Deep".to_owned());
        assert_eq!(
            route_style(MixPreset::SmoothSelector, &current, None, None),
            MixStyle::DeepRoots
        );
        assert_eq!(
            route_style(MixPreset::ClubHype, &current, None, None),
            MixStyle::ClubHype
        );
        assert_eq!(
            route_style(MixPreset::AggressiveClash, &current, None, None),
            MixStyle::AggressiveClash
        );
        assert_eq!(
            route_style(MixPreset::FreestyleMaestro, &current, None, None),
            MixStyle::DeepRoots
        );
        assert_eq!(intensity_to_style(1), MixStyle::DeepRoots);
        assert_eq!(intensity_to_style(2), MixStyle::ClubHype);
        assert_eq!(intensity_to_style(3), MixStyle::AggressiveClash);
        assert_eq!(intensity_to_style(4), MixStyle::AggressiveClash);
    }

    #[test]
    fn maestro_classifies_explicit_tags_then_sentiment_energy_then_hysteresis() {
        let mut current = track(1);
        current.acoustic_tag = Some("Aggressiv".to_owned());
        current.sentiment = Some(0.8);
        assert_eq!(
            classify_maestro_style(&current, None, None),
            MixStyle::AggressiveClash
        );

        current.acoustic_tag = Some("Hype".to_owned());
        assert_eq!(
            classify_maestro_style(&current, None, None),
            MixStyle::ClubHype
        );
        current.acoustic_tag = Some("Conscious".to_owned());
        assert_eq!(
            classify_maestro_style(&current, None, None),
            MixStyle::DeepRoots
        );

        current.acoustic_tag = None;
        current.sentiment = Some(-0.7);
        assert_eq!(
            classify_maestro_style(&current, None, None),
            MixStyle::AggressiveClash
        );
        current.sentiment = Some(0.6);
        current.energy_density = Some(0.8);
        assert_eq!(
            classify_maestro_style(&current, None, None),
            MixStyle::ClubHype
        );
        current.sentiment = None;
        current.energy_density = None;
        assert_eq!(
            classify_maestro_style(&current, None, Some(MixStyle::AggressiveClash)),
            MixStyle::AggressiveClash
        );
        assert_eq!(
            classify_maestro_style(&current, None, None),
            MixStyle::DeepRoots
        );

        current.sentiment_label = Some("negative".to_owned());
        assert_eq!(
            classify_maestro_style(&current, None, None),
            MixStyle::AggressiveClash
        );
        current.sentiment_label = Some("positive".to_owned());
        current.energy_density = Some(0.5);
        assert_eq!(
            classify_maestro_style(&current, None, None),
            MixStyle::ClubHype
        );
        current.sentiment_label = None;
        current.energy_density = Some(0.2);
        assert_eq!(
            classify_maestro_style(&current, None, None),
            MixStyle::DeepRoots
        );
    }

    #[test]
    fn maestro_uses_manual_tags_and_incoming_track_when_source_metadata_is_missing() {
        let mut current = track(1);
        current
            .tags
            .push(("genre".to_owned(), "Roots Reggae".to_owned()));
        assert_eq!(
            classify_maestro_style(&current, None, None),
            MixStyle::DeepRoots
        );
        current.tags.clear();
        current
            .tags
            .push(("custom".to_owned(), "Clash battle".to_owned()));
        assert_eq!(
            classify_maestro_style(&current, None, None),
            MixStyle::AggressiveClash
        );
        current.tags.clear();
        current
            .tags
            .push(("genre".to_owned(), "Club Hype".to_owned()));
        assert_eq!(
            classify_maestro_style(&current, None, None),
            MixStyle::ClubHype
        );
        current.tags.push(("genre".to_owned(), "Roots".to_owned()));
        current
            .tags
            .push(("custom".to_owned(), "Aggressiv Clash".to_owned()));
        assert_eq!(
            classify_maestro_style(&current, None, None),
            MixStyle::AggressiveClash
        );
        current.tags.clear();
        let mut incoming = track(2);
        incoming.acoustic_tag = Some("Hype".to_owned());
        assert_eq!(
            classify_maestro_style(&current, Some(&incoming), None),
            MixStyle::ClubHype
        );
    }

    #[test]
    fn tempo_math_matches_half_and_double_time_without_exceeding_supported_rates() {
        let half = calculate_tempo_match(Some(120.0), 1.0, Some(240.0));
        assert_eq!(half.mode, TempoMode::HalfTime);
        assert!((half.playback_rate - 1.0).abs() < f32::EPSILON);
        assert!((half.matched_bpm.unwrap() - 120.0).abs() < f64::EPSILON);

        let double = calculate_tempo_match(Some(120.0), 1.0, Some(60.0));
        assert_eq!(double.mode, TempoMode::DoubleTime);
        assert!((double.playback_rate - 1.0).abs() < f32::EPSILON);
        assert!((double.matched_bpm.unwrap() - 120.0).abs() < f64::EPSILON);

        let normal = calculate_tempo_match(Some(100.0), 1.0, Some(101.0));
        assert_eq!(normal.mode, TempoMode::Normal);
        assert!((normal.playback_rate - 100.0 / 101.0).abs() < 1.0e-6);
        let missing = calculate_tempo_match(None, 1.0, Some(128.0));
        assert_eq!(missing.mode, TempoMode::Normal);
        assert_eq!(missing.playback_rate, 1.0);
    }

    #[test]
    fn every_mix_style_builds_its_full_transition_policy() {
        let mut current = track(1);
        let mut next = track(2);
        current.words = [
            ("we", 10_000),
            ("are", 10_200),
            ("roots", 10_400),
            ("rocking", 10_600),
            ("all", 10_800),
            ("night", 11_000),
            ("we", 35_000),
            ("are", 35_200),
            ("roots", 35_400),
            ("rocking", 35_600),
            ("all", 35_800),
            ("night", 36_000),
        ]
        .into_iter()
        .map(|(text, time)| word(text, time))
        .collect();
        next.words = current.words.clone();
        let tempo = calculate_tempo_match(current.bpm, 1.0, next.bpm);

        let deep = build_decision(
            MixPreset::SmoothSelector,
            MixStyle::DeepRoots,
            &current,
            &next,
            tempo,
            None,
            true,
            1.0,
            None,
        );
        assert_eq!(deep.transition, TransitionKind::LongCrossfade);
        assert_eq!(deep.fade_duration_ms, 16_000);
        assert!(deep.bass_swap && deep.spring_auto_dub);
        assert!(!deep.auto_rewind && !deep.fader_kill_switches);

        let club = build_decision(
            MixPreset::ClubHype,
            MixStyle::ClubHype,
            &current,
            &next,
            tempo,
            None,
            true,
            1.0,
            None,
        );
        assert_eq!(club.transition, TransitionKind::HardChorusCut);
        assert_eq!(club.transition_at_ms, Some(10_000));
        assert_eq!(club.incoming_cue_ms, 10_000);
        assert!(club.same_riddim_family);
        assert_eq!(club.fade_duration_ms, 0);

        current.words.clear();
        let no_chorus = build_decision(
            MixPreset::ClubHype,
            MixStyle::ClubHype,
            &current,
            &next,
            tempo,
            None,
            false,
            1.0,
            None,
        );
        assert_eq!(no_chorus.transition_at_ms, Some(178_000));
        assert_eq!(no_chorus.incoming_cue_ms, 10_000);

        current.duration_ms = None;
        let unknown_length = build_decision(
            MixPreset::SmoothSelector,
            MixStyle::DeepRoots,
            &current,
            &next,
            tempo,
            None,
            false,
            1.0,
            None,
        );
        assert_eq!(unknown_length.transition_at_ms, None);
        current.duration_ms = Some(180_000);

        current.words = ["fire", "burning", "tonight"]
            .into_iter()
            .enumerate()
            .map(|(index, text)| word(text, 170_000 + index as i64 * 200))
            .collect();
        next.words = ["intro", "fire", "burning", "tonight"]
            .into_iter()
            .enumerate()
            .map(|(index, text)| word(text, index as i64 * 400))
            .collect();
        let word_play = find_word_play(&current, &next);
        let clash = build_decision(
            MixPreset::AggressiveClash,
            MixStyle::AggressiveClash,
            &current,
            &next,
            tempo,
            word_play,
            false,
            1.0,
            None,
        );
        assert_eq!(clash.transition, TransitionKind::SemanticRewindCut);
        assert_eq!(clash.incoming_cue_ms, 400);
        assert!(clash.auto_rewind && clash.fader_kill_switches);
        assert!(!clash.bass_swap && !clash.spring_auto_dub);

        let maximum = build_decision(
            MixPreset::FreestyleMaestro,
            MixStyle::AggressiveClash,
            &current,
            &next,
            tempo,
            None,
            false,
            1.0,
            Some(4),
        );
        assert_eq!(maximum.intensity_override, Some(4));
        assert_eq!(maximum.fade_duration_ms, 2_000);
    }

    #[test]
    fn repeated_four_word_phrases_form_chorus_cues() {
        let mut current = track(1);
        current.words = [
            ("we", 10_000),
            ("are", 10_200),
            ("roots", 10_400),
            ("rocking", 10_600),
            ("all", 10_800),
            ("night", 11_000),
            ("we", 35_000),
            ("are", 35_200),
            ("roots", 35_400),
            ("rocking", 35_600),
            ("all", 35_800),
            ("night", 36_000),
        ]
        .into_iter()
        .map(|(text, time)| word(text, time))
        .collect();
        let markers = find_chorus_markers(&current);
        assert!(markers.contains(&10_000));
        assert!(markers.contains(&10_200));
        assert!(markers.contains(&35_000));
    }

    #[test]
    fn word_play_selects_a_local_whisper_timestamp_for_a_shared_phrase() {
        let mut current = track(1);
        current.words = ["fire", "burning", "tonight"]
            .into_iter()
            .enumerate()
            .map(|(index, text)| word(text, 170_000 + index as i64 * 200))
            .collect();
        let mut next = track(2);
        next.words = ["intro", "fire", "burning", "tonight", "again"]
            .into_iter()
            .enumerate()
            .map(|(index, text)| word(text, index as i64 * 400))
            .collect();
        let matched = find_word_play(&current, &next).unwrap();
        assert!(matched.cue_ms <= 400);
        assert!(matched.matched_words.contains(&"fire".to_owned()));
        assert!(matched.phrase.contains("fire"));
    }
}
