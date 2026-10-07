use crate::{audio, database, device_binding, import, mix_engine};
use base64::Engine as _;
use championvibe_licensing_core::{
    decode_public_key_hex, verify_signed_license, Chain, CreateQuoteRequest, LicensePayload, Plan,
    Quote, SignedLicense,
};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, RwLock,
    },
};
use tauri::{AppHandle, Manager, State};
#[cfg(target_os = "android")]
use tauri_plugin_device_identity::DeviceIdentityExt;
use url::Url;

pub struct AppState {
    pub database: Arc<Mutex<Connection>>,
    pub audio: audio::AudioEngine,
    recording: Arc<audio::RecordingHub>,
    pub mix_engine: mix_engine::MixEngine,
    pub import_manager: Arc<import::ImportManager>,
    pub(crate) model_manager: audio::model_manager::ModelManager,
    pub(crate) model_loader: audio::model_loader::ModelLoader,
    signed_license: RwLock<Option<SignedLicense>>,
    device_binding: RwLock<Option<String>>,
    public_key: Option<[u8; 32]>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LicenseStatus {
    pub active: bool,
    pub demo_mode: bool,
    pub plan: Option<Plan>,
    pub expires_at: Option<i64>,
    pub seconds_remaining: Option<i64>,
    pub message: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MasterRecordingStatus {
    #[serde(flatten)]
    pub snapshot: audio::RecordingSnapshot,
    pub message: crate::localization::LocalizedMessage,
}

impl MasterRecordingStatus {
    fn from_snapshot(snapshot: audio::RecordingSnapshot) -> Self {
        let message = crate::localization::recording_status(&snapshot.status_code);
        Self { snapshot, message }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordingInput {
    pub name: String,
    pub file_path: String,
    pub duration_ms: Option<i64>,
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default)]
    pub sample_rate: Option<u32>,
    #[serde(default)]
    pub channel_count: Option<u32>,
    #[serde(default)]
    pub bit_depth: Option<u32>,
    #[serde(default)]
    pub bitrate_bps: Option<u32>,
    #[serde(default)]
    pub file_size_bytes: Option<u64>,
}

impl AppState {
    pub(crate) fn new(
        database: Connection,
        audio: audio::AudioEngine,
        mix_spare_deck: audio::AudioEngine,
        recording: Arc<audio::RecordingHub>,
        app_data_dir: PathBuf,
    ) -> anyhow::Result<Self> {
        let public_key = option_env!("CHAMPIONVIBE_LICENSE_PUBLIC_KEY_HEX")
            .and_then(|value| decode_public_key_hex(value).ok());
        let signed_license = database::load_license(&database)
            .ok()
            .flatten()
            .and_then(|json| serde_json::from_str::<SignedLicense>(&json).ok());
        let database = Arc::new(Mutex::new(database));
        let mix_engine =
            mix_engine::MixEngine::new(Arc::clone(&database), audio.clone(), mix_spare_deck);
        let models_dir = app_data_dir.join("models");
        let model_manager = audio::model_manager::ModelManager::new(models_dir.clone())?;
        let model_loader = audio::model_loader::ModelLoader::new(models_dir)?;
        Ok(Self {
            database,
            audio,
            recording,
            mix_engine,
            import_manager: Arc::new(import::ImportManager::new(app_data_dir)),
            model_manager,
            model_loader,
            signed_license: RwLock::new(signed_license),
            // Resolve lazily on Android through Tauri's native plugin after the
            // WebView is initialized; only the SHA-256 digest is cached.
            device_binding: RwLock::new(None),
            public_key,
        })
    }

    fn current_status(&self, binding: &str) -> LicenseStatus {
        let Some(public_key) = self.public_key.as_ref() else {
            return demo_status("Issuer verification key is not configured in this build.");
        };
        let license = match self.signed_license.read() {
            Ok(license) => license,
            Err(_) => return demo_status("License state is unavailable."),
        };
        let Some(license) = license.as_ref() else {
            return demo_status("No valid license is installed.");
        };
        let now = Utc::now().timestamp();
        if let Err(error) = verify_signed_license(license, public_key, binding, now) {
            return demo_status(&error.to_string());
        }
        LicenseStatus {
            active: true,
            demo_mode: false,
            plan: Some(license.payload.plan),
            expires_at: Some(license.payload.expires_at),
            seconds_remaining: Some(license.payload.expires_at.saturating_sub(now)),
            message: "License active".to_owned(),
        }
    }

    fn require_feature(&self, binding: &str, feature: &str) -> Result<LicensePayload, String> {
        let public_key = self
            .public_key
            .as_ref()
            .ok_or_else(|| "Demo mode: issuer verification key is not configured".to_owned())?;
        let license = self
            .signed_license
            .read()
            .map_err(|_| "license state is unavailable".to_owned())?;
        let license = license
            .as_ref()
            .ok_or_else(|| "Demo mode: no valid license is installed".to_owned())?;
        verify_signed_license(license, public_key, binding, Utc::now().timestamp())
            .map_err(|error| format!("Demo mode: {error:#}"))?;
        if !license.payload.features.iter().any(|item| item == feature) {
            return Err(format!("license does not include the {feature} feature"));
        }
        Ok(license.payload.clone())
    }
}

#[allow(unused_variables)]
fn current_device_binding(state: &AppState, app: &AppHandle) -> Result<String, String> {
    if let Some(binding) = state
        .device_binding
        .read()
        .map_err(|_| "device binding cache lock poisoned".to_owned())?
        .as_ref()
    {
        return Ok(binding.clone());
    }

    #[cfg(any(target_os = "windows", target_os = "linux"))]
    let binding = device_binding::device_binding().map_err(|error| error.to_string())?;

    #[cfg(target_os = "android")]
    let binding = {
        let android_id = app.device_identity().android_id()?;
        device_binding::binding_from_identifier(&android_id).map_err(|error| error.to_string())?
    };

    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "android")))]
    let binding: String =
        return Err("ChampionVibe device binding is implemented for Windows and Android".to_owned());

    #[allow(unreachable_code)]
    {
        let mut cache = state
            .device_binding
            .write()
            .map_err(|_| "device binding cache lock poisoned".to_owned())?;
        Ok(cache.get_or_insert(binding).clone())
    }
}

fn active_stem_profile(state: &AppState, app: &AppHandle) -> Result<u8, String> {
    #[cfg(target_os = "android")]
    {
        let binding = current_device_binding(state, app)?;
        return Ok(if state.current_status(&binding).active {
            4
        } else {
            2
        });
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (state, app);
        Ok(4)
    }
}

fn demo_status(message: &str) -> LicenseStatus {
    LicenseStatus {
        active: false,
        demo_mode: true,
        plan: None,
        expires_at: None,
        seconds_remaining: None,
        message: message.to_owned(),
    }
}

#[tauri::command]
pub fn get_device_binding(state: State<'_, AppState>, app: AppHandle) -> Result<String, String> {
    current_device_binding(&state, &app)
}

#[tauri::command]
pub fn get_license_status(state: State<'_, AppState>, app: AppHandle) -> LicenseStatus {
    match current_device_binding(&state, &app) {
        Ok(binding) => state.current_status(&binding),
        Err(error) => demo_status(&error),
    }
}

#[tauri::command]
pub fn get_localized_catalog() -> crate::localization::LocalizationCatalog {
    crate::localization::catalog()
}

#[tauri::command]
pub async fn create_payment_quote(
    issuer_url: String,
    chain: Chain,
    plan: Plan,
) -> Result<Quote, String> {
    let url = issuer_endpoint(&issuer_url, "/v1/quotes")?;
    post_json(&url, &CreateQuoteRequest { chain, plan }).await
}

#[tauri::command]
pub async fn activate_license(
    state: State<'_, AppState>,
    app: AppHandle,
    issuer_url: String,
    quote_id: String,
    transaction_hash: String,
) -> Result<LicenseStatus, String> {
    let binding = current_device_binding(&state, &app)?;
    let public_key = state
        .public_key
        .as_ref()
        .ok_or_else(|| "issuer public key is not configured in this build".to_owned())?;
    let url = issuer_endpoint(&issuer_url, "/v1/activate")?;
    let signed: SignedLicense = post_json(
        &url,
        &championvibe_licensing_core::ActivateRequest {
            quote_id,
            transaction_hash,
            device_binding: binding.clone(),
        },
    )
    .await?;

    verify_signed_license(&signed, public_key, &binding, Utc::now().timestamp())
        .map_err(|error| format!("issuer license was rejected: {error:#}"))?;
    let signed_json = serde_json::to_string(&signed).map_err(|error| error.to_string())?;
    {
        let database = state
            .database
            .lock()
            .map_err(|_| "application database lock poisoned".to_owned())?;
        database::store_license(&database, &signed_json).map_err(|error| error.to_string())?;
    }
    *state
        .signed_license
        .write()
        .map_err(|_| "license state lock poisoned".to_owned())? = Some(signed);
    let _ = state.mix_engine.set_stem_profile(4);
    Ok(state.current_status(&binding))
}

#[tauri::command]
pub fn get_recording_formats() -> Vec<audio::RecordingFormatInfo> {
    audio::recording_formats()
}

#[tauri::command]
pub fn authorize_recording(state: State<'_, AppState>, app: AppHandle) -> Result<(), String> {
    // Frontend state is never an authorization boundary; the native start command
    // rechecks both features and arms the callback deadline from the signed expiry.
    let binding = current_device_binding(&state, &app)?;
    state.require_feature(&binding, "record")?;
    state.require_feature(&binding, "save").map(|_| ())
}

#[tauri::command]
pub fn start_master_recording(
    state: State<'_, AppState>,
    app: AppHandle,
    format: Option<String>,
) -> Result<MasterRecordingStatus, String> {
    let binding = current_device_binding(&state, &app)?;
    state.require_feature(&binding, "record")?;
    state.require_feature(&binding, "save")?;
    let recording_format = audio::RecordingFormat::parse(format.as_deref())?;
    let sample_rate = state
        .recording
        .sample_rate()
        .ok_or_else(|| "Load a local track on a mix deck before recording.".to_string())?;
    let path = recording_output_path(&app, recording_format)?;
    // Revalidate both features and convert the exact signed wall-clock expiry to a
    // monotonic deadline immediately before arming capture (no whole-second grace).
    let entitlement = state.require_feature(&binding, "record")?;
    state.require_feature(&binding, "save")?;
    let remaining_ns = exact_license_remaining_ns(entitlement.expires_at)?;
    let deadline_ns = state.recording.clock.now_ns().saturating_add(remaining_ns);
    state
        .recording
        .start(path, recording_format, sample_rate, deadline_ns)
        .map(MasterRecordingStatus::from_snapshot)
}

#[tauri::command]
pub fn get_master_recording_status(
    state: State<'_, AppState>,
) -> Result<MasterRecordingStatus, String> {
    state
        .recording
        .snapshot()
        .map(MasterRecordingStatus::from_snapshot)
}

#[tauri::command]
pub fn stop_master_recording(
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<audio::RecordingFile, String> {
    let binding = current_device_binding(&state, &app)?;
    state.require_feature(&binding, "record")?;
    state.require_feature(&binding, "save")?;
    let file = state.recording.stop()?;

    // Persist only after the Rust-side entitlement is revalidated under the SQLite write lock.
    let database = state
        .database
        .lock()
        .map_err(|_| "application database lock poisoned".to_owned())?;
    let persistence = (|| {
        state.require_feature(&binding, "record")?;
        state.require_feature(&binding, "save")?;
        let duration_ms = (file.frames as u128)
            .saturating_mul(1_000)
            .checked_div(file.sample_rate as u128)
            .unwrap_or(0)
            .min(i64::MAX as u128) as i64;
        let bitrate_bps = file
            .sample_rate
            .saturating_mul(u32::from(file.channels))
            .saturating_mul(u32::from(file.bit_depth));
        database::save_recording_metadata(
            &database,
            &file.name,
            &file.path,
            Some(duration_ms),
            &database::RecordingMetadata {
                format: Some(file.format.clone()),
                sample_rate: Some(file.sample_rate),
                channel_count: Some(u32::from(file.channels)),
                bit_depth: Some(u32::from(file.bit_depth)),
                bitrate_bps: Some(bitrate_bps),
                file_size_bytes: Some(file.size_bytes),
            },
        )
        .map_err(|error| error.to_string())
    })();
    if let Err(error) = persistence {
        drop(database);
        let _ = fs::remove_file(&file.path);
        return Err(error);
    }
    Ok(file)
}

fn exact_license_remaining_ns(expires_at: i64) -> Result<u64, String> {
    let now_ns = Utc::now()
        .timestamp_nanos_opt()
        .map(i128::from)
        .ok_or_else(|| "The system clock is outside the supported license range.".to_string())?;
    let expiry_ns = i128::from(expires_at)
        .checked_mul(1_000_000_000)
        .ok_or_else(|| "The license expiry timestamp is invalid.".to_string())?;
    u64::try_from(expiry_ns - now_ns)
        .map_err(|_| "Pro license expired; recording is unavailable.".to_string())
}

static RECORDING_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[allow(unused_variables)]
fn recording_output_path(
    app: &AppHandle,
    format: audio::RecordingFormat,
) -> Result<PathBuf, String> {
    #[cfg(target_os = "windows")]
    let directory = std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .map(|profile| {
            profile
                .join("Music")
                .join("ChampionVibe")
                .join("Recordings")
        })
        .ok_or_else(|| "Windows user profile directory is unavailable.".to_string())?;

    #[cfg(target_os = "android")]
    let directory = app
        .device_identity()
        .external_files_dir()
        .map(PathBuf::from)?
        .join("ChampionVibe")
        .join("Recordings");

    #[cfg(not(any(target_os = "windows", target_os = "android")))]
    let directory = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("Could not resolve the app data folder: {error}"))?
        .join("Music")
        .join("ChampionVibe")
        .join("Recordings");

    let sequence = RECORDING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let name = format!(
        "ChampionVibe-{}-{sequence:04}.{}",
        Utc::now().format("%Y%m%d-%H%M%S"),
        format.extension()
    );
    Ok(directory.join(name))
}

#[tauri::command]
pub fn save_recording_metadata(
    state: State<'_, AppState>,
    app: AppHandle,
    input: RecordingInput,
) -> Result<i64, String> {
    let name = input.name.trim();
    let path = input.file_path.trim();
    if name.is_empty() || name.len() > 240 || path.is_empty() || path.len() > 4096 {
        return Err("recording name or path is invalid".to_owned());
    }
    if input.duration_ms.is_some_and(|duration| duration < 0) {
        return Err("recording duration cannot be negative".to_owned());
    }
    if input.format.as_ref().is_some_and(|format| {
        !matches!(
            format.as_str(),
            "wavPcm16" | "wavPcm24" | "flac" | "mp3" | "oggVorbis" | "m4aAac"
        )
    }) {
        return Err("recording format is not recognized".to_owned());
    }
    if input
        .sample_rate
        .is_some_and(|rate| !(8_000..=192_000).contains(&rate))
    {
        return Err("recording sample rate must be between 8,000 and 192,000 Hz".to_owned());
    }
    if input
        .channel_count
        .is_some_and(|channels| !(1..=8).contains(&channels))
    {
        return Err("recording channel count must be between 1 and 8".to_owned());
    }
    if input
        .bit_depth
        .is_some_and(|depth| !matches!(depth, 16 | 24))
    {
        return Err("recording bit depth must be 16 or 24".to_owned());
    }
    if input
        .bitrate_bps
        .is_some_and(|bitrate| bitrate == 0 || bitrate > 100_000_000)
    {
        return Err("recording bitrate is outside the supported range".to_owned());
    }
    if input
        .file_size_bytes
        .is_some_and(|bytes| bytes > i64::MAX as u64)
    {
        return Err("recording file is too large for local metadata".to_owned());
    }
    let binding = current_device_binding(&state, &app)?;
    let database = state
        .database
        .lock()
        .map_err(|_| "application database lock poisoned".to_owned())?;
    // Check after waiting for the database lock, as close as possible to the write.
    state.require_feature(&binding, "record")?;
    state.require_feature(&binding, "save")?;
    let metadata = database::RecordingMetadata {
        format: input.format,
        sample_rate: input.sample_rate,
        channel_count: input.channel_count,
        bit_depth: input.bit_depth,
        bitrate_bps: input.bitrate_bps,
        file_size_bytes: input.file_size_bytes,
    };
    database::save_recording_metadata(&database, name, path, input.duration_ms, &metadata)
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn upsert_track(
    state: State<'_, AppState>,
    input: database::TrackInput,
) -> Result<i64, String> {
    validate_track(&input)?;
    let mut database = state
        .database
        .lock()
        .map_err(|_| "application database lock poisoned".to_owned())?;
    database::upsert_track(&mut database, &input).map_err(|error| error.to_string())
}

#[tauri::command]
pub fn search_tracks(
    state: State<'_, AppState>,
    query: String,
    limit: Option<u32>,
) -> Result<Vec<database::TrackSummary>, String> {
    if query.len() > 256 {
        return Err("search query is too long".to_owned());
    }
    let limit = limit.unwrap_or(100).clamp(1, 250);
    let database = state
        .database
        .lock()
        .map_err(|_| "application database lock poisoned".to_owned())?;
    database::search_tracks(&database, &query, limit).map_err(|error| error.to_string())
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackSearchPage {
    pub tracks: Vec<database::TrackSummary>,
    pub total: u64,
    pub offset: u32,
    pub has_more: bool,
}

#[tauri::command]
pub fn search_tracks_page(
    state: State<'_, AppState>,
    query: String,
    filters: Option<database::TrackSearchFilters>,
    offset: Option<u32>,
    limit: Option<u32>,
) -> Result<TrackSearchPage, String> {
    if query.len() > 256 {
        return Err("search query is too long".to_owned());
    }
    let filters = filters.unwrap_or_default();
    for (name, value, low, high) in [
        ("bpmMin", filters.bpm_min, 40.0, 260.0),
        ("bpmMax", filters.bpm_max, 40.0, 260.0),
        ("lufsMin", filters.lufs_min, -70.0, 10.0),
        ("lufsMax", filters.lufs_max, -70.0, 10.0),
    ] {
        if value.is_some_and(|value| !value.is_finite() || !(low..=high).contains(&value)) {
            return Err(format!("{name} is outside its valid range"));
        }
    }
    if filters
        .bpm_min
        .zip(filters.bpm_max)
        .is_some_and(|(low, high)| low > high)
        || filters
            .lufs_min
            .zip(filters.lufs_max)
            .is_some_and(|(low, high)| low > high)
    {
        return Err("search filter ranges are reversed".to_owned());
    }
    for (name, value, maximum) in [
        ("mood", filters.mood.as_deref(), 32),
        ("rhythm", filters.rhythm.as_deref(), 64),
        ("key", filters.key.as_deref(), 24),
    ] {
        if value.is_some_and(|value| value.len() > maximum) {
            return Err(format!("{name} search filter is too long"));
        }
    }
    let offset = offset.unwrap_or(0);
    let limit = limit.unwrap_or(200).clamp(1, 200);
    let database = state
        .database
        .lock()
        .map_err(|_| "application database lock poisoned".to_owned())?;
    let (tracks, total) =
        database::search_tracks_page_with_filters(&database, &query, &filters, limit, offset)
            .map_err(|error| error.to_string())?;
    let has_more = u64::from(offset).saturating_add(tracks.len() as u64) < total;
    Ok(TrackSearchPage {
        tracks,
        total,
        offset,
        has_more,
    })
}

/// Sends compact little-endian peak and beat-grid data as a binary Tauri response.
/// No JSON conversion or large grid string crosses the WebView bridge.
#[tauri::command]
pub async fn get_track_waveform_packet(
    state: State<'_, AppState>,
    track_id: i64,
    peak_count: Option<u16>,
) -> Result<tauri::ipc::Response, String> {
    let peak_count = usize::from(peak_count.unwrap_or(1024).clamp(128, 2048));
    let peak_count_sql = peak_count as u16;
    let (source_path, duration_ms, bpm, beat_times_seconds, cached_peaks) = {
        let connection = state
            .database
            .lock()
            .map_err(|_| "application database lock poisoned".to_owned())?;
        let stems =
            database::load_stems(&connection, track_id).map_err(|error| error.to_string())?;
        let source_path = stems
            .iter()
            .find(|stem| stem.kind.eq_ignore_ascii_case("instrumental"))
            .or_else(|| {
                stems
                    .iter()
                    .find(|stem| stem.kind.eq_ignore_ascii_case("acapella"))
            })
            .map(|stem| PathBuf::from(&stem.file_path))
            .ok_or_else(|| "track has no local stem to analyze".to_owned())?;
        import::ensure_local_path(&source_path)
            .map_err(|error| format!("waveform stem must be local: {error:#}"))?;
        let duration_ms = connection
            .query_row(
                "SELECT duration_ms FROM tracks WHERE id = ?1",
                [track_id],
                |row| row.get::<_, Option<i64>>(0),
            )
            .optional()
            .map_err(|error| error.to_string())?
            .flatten();
        let (bpm, beat_times_seconds) =
            database::load_beat_grid(&connection, track_id).map_err(|error| error.to_string())?;
        let cached_peaks = database::load_waveform_peak_blob(&connection, track_id, peak_count_sql)
            .map_err(|error| error.to_string())?;
        (
            source_path,
            duration_ms,
            bpm,
            beat_times_seconds,
            cached_peaks,
        )
    };
    if let Some(cached_peaks) = cached_peaks {
        let packet = audio::waveform_packet_from_peak_blob(
            &cached_peaks,
            bpm,
            &beat_times_seconds,
            peak_count,
        )?;
        return Ok(tauri::ipc::Response::new(packet));
    }
    let bytes = tauri::async_runtime::spawn_blocking(move || {
        audio::waveform_packet(
            &source_path,
            duration_ms,
            bpm,
            &beat_times_seconds,
            peak_count,
        )
    })
    .await
    .map_err(|error| format!("waveform worker failed: {error}"))??;
    let peak_end = 16 + peak_count * 2;
    let peaks_blob = bytes
        .get(16..peak_end)
        .ok_or_else(|| "generated waveform packet has an invalid peak section".to_owned())?;
    let connection = state
        .database
        .lock()
        .map_err(|_| "application database lock poisoned".to_owned())?;
    database::save_waveform_peak_blob(&connection, track_id, peak_count_sql, peaks_blob)
        .map_err(|error| error.to_string())?;
    Ok(tauri::ipc::Response::new(bytes))
}

#[tauri::command]
pub fn get_track_manual_tags(
    state: State<'_, AppState>,
    track_id: i64,
) -> Result<Vec<crate::riddim::ManualTagInput>, String> {
    let database = state
        .database
        .lock()
        .map_err(|_| "application database lock poisoned".to_owned())?;
    crate::riddim::load_track_tags(&database, track_id).map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn set_track_manual_tags(
    app: AppHandle,
    track_id: i64,
    tags: Vec<crate::riddim::ManualTagInput>,
) -> Result<crate::riddim::RebuildReport, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        crate::riddim::set_track_tags_in_state(&state, track_id, tags)
            .map_err(|error| format!("could not save manual tags: {error:#}"))
    })
    .await
    .map_err(|error| format!("tag update task failed: {error}"))?
}

#[tauri::command]
pub async fn rebuild_riddim_families(
    app: AppHandle,
) -> Result<crate::riddim::RebuildReport, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        crate::riddim::rebuild_in_state(&state)
            .map_err(|error| format!("riddim-family rebuild failed: {error:#}"))
    })
    .await
    .map_err(|error| format!("riddim scan task failed: {error}"))?
}

#[tauri::command]
pub fn get_riddim_families(
    state: State<'_, AppState>,
    limit: Option<u32>,
) -> Result<Vec<crate::riddim::RiddimFamilySummary>, String> {
    let database = state
        .database
        .lock()
        .map_err(|_| "application database lock poisoned".to_owned())?;
    crate::riddim::list_families(&database, limit.unwrap_or(50))
        .map_err(|error| format!("could not read riddim families: {error:#}"))
}

#[tauri::command]
pub fn get_track_riddim_family(
    state: State<'_, AppState>,
    track_id: i64,
) -> Result<Option<crate::riddim::TrackRiddimFamily>, String> {
    let database = state
        .database
        .lock()
        .map_err(|_| "application database lock poisoned".to_owned())?;
    crate::riddim::family_for_track(&database, track_id)
        .map_err(|error| format!("could not load track riddim family: {error:#}"))
}

#[tauri::command]
pub fn start_import_job(
    state: State<'_, AppState>,
    app: AppHandle,
    request: import::StartImportRequest,
) -> Result<database::ImportJobSnapshot, String> {
    import::start_job(&app, &state, request)
}

#[tauri::command]
pub fn get_import_jobs(
    state: State<'_, AppState>,
    limit: Option<u32>,
) -> Result<Vec<database::ImportJobSnapshot>, String> {
    import::recent_jobs(&state, limit.unwrap_or(20))
}

#[tauri::command]
pub fn cancel_import_job(
    state: State<'_, AppState>,
    app: AppHandle,
    job_id: i64,
) -> Result<(), String> {
    import::cancel_job(&app, &state, job_id)
}

#[tauri::command]
pub fn pick_android_import_paths(app: AppHandle, directory: bool) -> Result<Vec<String>, String> {
    import::pick_android_import_paths(&app, directory)
}

#[tauri::command]
pub fn pick_android_sample_folder(app: AppHandle) -> Result<Option<String>, String> {
    import::pick_android_sample_folder(&app)
}

#[tauri::command]
pub fn pick_android_model(app: AppHandle, kind: String) -> Result<Option<String>, String> {
    let extension = match kind.as_str() {
        "separator" | "piper-model" => "onnx",
        "whisper" => "bin",
        "piper-config" => "json",
        _ => return Err("unknown local model type".to_owned()),
    };
    import::pick_android_model(&app, extension)
}

#[tauri::command]
pub fn get_model_catalog(
    state: State<'_, AppState>,
) -> Vec<audio::model_manager::ManagedModelInfo> {
    state.model_manager.catalog()
}

#[tauri::command]
pub async fn download_managed_model(
    state: State<'_, AppState>,
    model_id: String,
) -> Result<String, String> {
    state
        .model_manager
        .download(&model_id)
        .await
        .map(|path| path.to_string_lossy().into_owned())
        .map_err(|error| format!("model download failed: {error:#}"))
}

#[tauri::command]
pub fn read_managed_kokoro_asset(
    state: State<'_, AppState>,
    asset: String,
) -> Result<tauri::ipc::Response, String> {
    state
        .model_manager
        .read_kokoro_asset(&asset)
        .map(tauri::ipc::Response::new)
        .map_err(|error| format!("could not read verified Kokoro {asset}: {error:#}"))
}

#[tauri::command]
pub fn validate_local_model(
    state: State<'_, AppState>,
    path: String,
    kind: String,
) -> Result<audio::model_manager::ModelValidation, String> {
    if kind == "htdemucs" {
        let supplied = PathBuf::from(path.trim());
        let model_path = fs::canonicalize(&supplied).map_err(|error| {
            format!(
                "could not resolve selected model {}: {error}",
                supplied.display()
            )
        })?;
        crate::import::ensure_local_path(&model_path)
            .map_err(|error| format!("HT-Demucs models must be local: {error:#}"))?;
        crate::import::separator::validate_htdemucs_model(&model_path)
            .map_err(|error| format!("HT-Demucs model contract is invalid: {error:#}"))?;
        state
            .model_manager
            .verify_path_if_managed(&model_path)
            .map_err(|error| format!("managed model integrity check failed: {error:#}"))?;
        return Ok(audio::model_manager::ModelValidation {
            valid: true,
            kind,
            contract: "HT-Demucs float32 [1, 2, 343980] → [1, 4, 2, 343980]".to_owned(),
            details:
                "local ONNX graph, tensor dtypes, input/output names, and dimensions validated"
                    .to_owned(),
            path: model_path.to_string_lossy().into_owned(),
        });
    }
    state
        .model_manager
        .validate_user_file(&path, &kind)
        .map_err(|error| format!("local model contract is invalid: {error:#}"))
}

#[tauri::command]
pub fn import_custom_separator_models(
    state: State<'_, AppState>,
    paths: Vec<String>,
    mode: String,
) -> Result<String, String> {
    state
        .model_manager
        .import_user_separator_models(&paths, &mode)
        .map(|path| path.to_string_lossy().into_owned())
        .map_err(|error| format!("custom MDX-Net import failed: {error:#}"))
}

#[tauri::command]
pub fn get_separator_catalog(
    state: State<'_, AppState>,
) -> Vec<audio::model_loader::ManagedArtifactInfo> {
    state.model_loader.catalog()
}

#[tauri::command]
pub async fn download_default_separator(
    state: State<'_, AppState>,
    model_id: Option<String>,
) -> Result<String, String> {
    let id = model_id.unwrap_or_else(|| audio::model_loader::default_separator_id().to_owned());
    state
        .model_loader
        .download_separator(&id)
        .await
        .map(|path| path.to_string_lossy().into_owned())
        .map_err(|error| format!("separator download failed: {error:#}"))
}

#[tauri::command]
pub fn get_installed_separator(state: State<'_, AppState>) -> Option<String> {
    state
        .model_loader
        .installed_separator_descriptor()
        .map(|path| path.to_string_lossy().into_owned())
}

#[tauri::command]
pub fn list_byom_models(state: State<'_, AppState>) -> Vec<audio::model_loader::ByomModelRecord> {
    state.model_loader.byom_records()
}

#[tauri::command]
pub fn validate_byom_model(
    state: State<'_, AppState>,
    path: String,
    kind: String,
) -> Result<audio::model_loader::ByomContractReport, String> {
    state
        .model_loader
        .validate_byom_file(&path, &kind)
        .map_err(|error| format!("BYOM validation failed: {error:#}"))
}

#[tauri::command]
pub fn import_byom_model(
    state: State<'_, AppState>,
    path: String,
    kind: String,
    acknowledged: bool,
    display_name: Option<String>,
) -> Result<audio::model_loader::ByomModelRecord, String> {
    state
        .model_loader
        .import_byom(&path, &kind, acknowledged, display_name.as_deref())
        .map(|(record, _)| record)
        .map_err(|error| format!("BYOM import failed: {error:#}"))
}

#[tauri::command]
pub fn remove_byom_model(state: State<'_, AppState>, id: String) -> Result<bool, String> {
    state
        .model_loader
        .remove_byom(&id)
        .map_err(|error| format!("BYOM removal failed: {error:#}"))
}

#[tauri::command]
pub fn get_tts_engine_status() -> bool {
    crate::tts::engine_available()
}

#[derive(Clone, Copy)]
struct TtsIpcMetadata {
    sample_rate: u32,
    channels: u8,
    quantization: audio::SampleQuantization,
}

fn parse_tts_ipc_metadata(
    sample_rate: &str,
    channels: &str,
    encoding: &str,
    quantization: &str,
) -> Result<TtsIpcMetadata, String> {
    let sample_rate = sample_rate
        .parse::<u32>()
        .map_err(|_| "TTS IPC sample-rate header must be an integer".to_owned())?;
    if !(8_000..=192_000).contains(&sample_rate) {
        return Err("TTS IPC sample rate must be between 8 kHz and 192 kHz".to_owned());
    }
    let channels = channels
        .parse::<u8>()
        .map_err(|_| "TTS IPC channel-count header must be an integer".to_owned())?;
    if !matches!(channels, 1 | 2) {
        return Err("TTS IPC supports mono or stereo PCM only".to_owned());
    }
    if !encoding.eq_ignore_ascii_case("f32le") {
        return Err(
            "TTS IPC encoding must be interleaved float32 little-endian (f32le)".to_owned(),
        );
    }
    let quantization = match quantization.to_ascii_lowercase().as_str() {
        "beat" => audio::SampleQuantization::Beat,
        "bar" => audio::SampleQuantization::Bar,
        _ => return Err("TTS IPC quantization must be beat or bar".to_owned()),
    };
    Ok(TtsIpcMetadata {
        sample_rate,
        channels,
        quantization,
    })
}

fn required_tts_request_header<'a>(
    request: &'a tauri::ipc::Request<'_>,
    name: &'static str,
) -> Result<&'a str, String> {
    request
        .headers()
        .get(name)
        .ok_or_else(|| format!("missing TTS IPC header `{name}`"))?
        .to_str()
        .map_err(|_| format!("TTS IPC header `{name}` is not valid text"))
}

fn parse_tts_raw_request_metadata(
    request: &tauri::ipc::Request<'_>,
) -> Result<TtsIpcMetadata, String> {
    parse_tts_ipc_metadata(
        required_tts_request_header(request, "x-cv-tts-sample-rate")?,
        required_tts_request_header(request, "x-cv-tts-channels")?,
        required_tts_request_header(request, "x-cv-tts-encoding")?,
        required_tts_request_header(request, "x-cv-tts-quantization")?,
    )
}

fn raw_tts_ipc_body(body: &tauri::ipc::InvokeBody) -> Result<&[u8], String> {
    match body {
        tauri::ipc::InvokeBody::Raw(bytes) => Ok(bytes),
        tauri::ipc::InvokeBody::Json(_) => Err(
            "TTS PCM must use a raw binary Tauri IPC body; JSON number arrays are not accepted"
                .to_owned(),
        ),
    }
}

fn parse_tts_android_base64_payload(
    value: &serde_json::Value,
) -> Result<(Vec<u8>, TtsIpcMetadata), String> {
    let object = value.as_object().ok_or_else(|| {
        "Android TTS fallback expects a JSON object containing base64 PCM".to_owned()
    })?;
    if object.contains_key("pcm") || object.contains_key("samples") {
        return Err("JSON number arrays are not accepted for TTS PCM".to_owned());
    }
    let encoded = object
        .get("pcmBase64")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "Android TTS fallback is missing its base64 PCM string".to_owned())?;
    let max_encoded_bytes = audio::MAX_TTS_PCM_BYTES.saturating_add(2) / 3 * 4;
    if encoded.is_empty() || encoded.len() > max_encoded_bytes {
        return Err(
            "Android base64 TTS payload is empty or exceeds the 64 MiB PCM limit".to_owned(),
        );
    }
    let sample_rate = object
        .get("sampleRate")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| "Android TTS payload has no integer sampleRate".to_owned())?
        .to_string();
    let channels = object
        .get("channels")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| "Android TTS payload has no integer channels".to_owned())?
        .to_string();
    let encoding = object
        .get("encoding")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "Android TTS payload has no encoding".to_owned())?;
    let quantization = object
        .get("quantization")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "Android TTS payload has no quantization".to_owned())?;
    let metadata = parse_tts_ipc_metadata(&sample_rate, &channels, encoding, quantization)?;
    let pcm = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| "Android TTS PCM base64 could not be decoded".to_owned())?;
    if pcm.is_empty() || pcm.len() > audio::MAX_TTS_PCM_BYTES {
        return Err("Android decoded TTS PCM is empty or exceeds the 64 MiB limit".to_owned());
    }
    Ok((pcm, metadata))
}

#[tauri::command]
pub fn push_tts_audio_buffer(
    state: State<'_, AppState>,
    request: tauri::ipc::Request<'_>,
) -> Result<audio::TtsAudioSchedule, String> {
    let engine = state.mix_engine.active_audio_engine();
    match request.body() {
        tauri::ipc::InvokeBody::Raw(_) => {
            let pcm = raw_tts_ipc_body(request.body())?;
            let metadata = parse_tts_raw_request_metadata(&request)?;
            engine.push_tts_audio_buffer(
                pcm,
                metadata.sample_rate,
                metadata.channels,
                metadata.quantization,
            )
        }
        tauri::ipc::InvokeBody::Json(value) if cfg!(target_os = "android") => {
            let (pcm, metadata) = parse_tts_android_base64_payload(value)?;
            engine.push_tts_audio_buffer(
                &pcm,
                metadata.sample_rate,
                metadata.channels,
                metadata.quantization,
            )
        }
        tauri::ipc::InvokeBody::Json(_) => {
            Err("TTS PCM requires a raw binary Tauri IPC request on this platform".to_owned())
        }
    }
}

#[tauri::command]
pub async fn generate_dubplate(
    state: State<'_, AppState>,
    app: AppHandle,
    input: crate::tts::DubplateRequest,
) -> Result<crate::tts::DubplateResult, String> {
    let engine = state.mix_engine.active_audio_engine();
    let output_directory = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("could not resolve dubplate storage: {error}"))?
        .join("dubplates");
    tauri::async_runtime::spawn_blocking(move || {
        crate::tts::generate(input, &output_directory, &engine)
    })
    .await
    .map_err(|error| format!("dubplate synthesis task failed: {error}"))?
}

#[tauri::command]
#[allow(unused_variables)]
pub async fn load_track_audio(
    state: State<'_, AppState>,
    app: AppHandle,
    track_id: i64,
) -> Result<audio::AudioStatus, String> {
    let (stems, bpm, beat_times_seconds, integrated_lufs) = {
        let connection = state
            .database
            .lock()
            .map_err(|_| "application database lock poisoned".to_owned())?;
        let stems =
            database::load_stems(&connection, track_id).map_err(|error| error.to_string())?;
        let (bpm, beat_times_seconds) =
            database::load_beat_grid(&connection, track_id).map_err(|error| error.to_string())?;
        let integrated_lufs = database::load_integrated_lufs(&connection, track_id)
            .map_err(|error| error.to_string())?;
        (stems, bpm, beat_times_seconds, integrated_lufs)
    };
    let requested_stems = active_stem_profile(&state, &app)?;
    let active_stems = profile_for_available_stems(&stems, requested_stems)?;
    let stems = stem_files_for_profile(stems, usize::from(active_stems))?;
    let _ = state.mix_engine.stop();
    state.mix_engine.set_stem_profile(active_stems)?;
    state.audio.set_active_stem_profile(active_stems)?;
    let grid = audio::BeatGrid {
        beat_times_seconds,
        bpm,
    };

    #[cfg(target_os = "android")]
    app.device_identity().initialize_audio_context()?;

    let engine = state.audio.clone();
    tauri::async_runtime::spawn_blocking(move || {
        engine.load_with_loudness(stems, grid, integrated_lufs)
    })
    .await
    .map_err(|error| format!("audio preparation task failed: {error}"))?
}

#[tauri::command]
pub async fn play_audio(state: State<'_, AppState>) -> Result<audio::AudioStatus, String> {
    let engine = state.mix_engine.active_audio_engine();
    tauri::async_runtime::spawn_blocking(move || engine.play())
        .await
        .map_err(|error| format!("audio control task failed: {error}"))?
}

#[tauri::command]
pub async fn pause_audio(state: State<'_, AppState>) -> Result<audio::AudioStatus, String> {
    let engine = state.mix_engine.active_audio_engine();
    tauri::async_runtime::spawn_blocking(move || engine.pause())
        .await
        .map_err(|error| format!("audio control task failed: {error}"))?
}

#[tauri::command]
pub async fn stop_audio(state: State<'_, AppState>) -> Result<audio::AudioStatus, String> {
    let _ = state.mix_engine.stop();
    let engine = state.audio.clone();
    tauri::async_runtime::spawn_blocking(move || engine.stop())
        .await
        .map_err(|error| format!("audio control task failed: {error}"))?
}

#[tauri::command]
pub fn get_audio_status(state: State<'_, AppState>) -> audio::AudioStatus {
    state.mix_engine.audio_status()
}

#[tauri::command]
pub fn set_audio_controls(
    state: State<'_, AppState>,
    update: audio::AudioControlUpdate,
) -> Result<audio::AudioStatus, String> {
    state
        .mix_engine
        .active_audio_engine()
        .update_controls(update)
}

#[tauri::command]
pub fn set_deck_audio_controls(
    state: State<'_, AppState>,
    deck: u8,
    update: audio::AudioControlUpdate,
) -> Result<audio::AudioStatus, String> {
    state.mix_engine.set_deck_controls(deck, update)
}

#[tauri::command]
pub fn get_sample_catalog(state: State<'_, AppState>) -> Vec<audio::SampleInfo> {
    state.mix_engine.sample_catalog()
}

#[tauri::command]
pub async fn load_sample_folder(
    state: State<'_, AppState>,
    path: String,
) -> Result<Vec<audio::SampleInfo>, String> {
    if path.trim().is_empty() || path.len() > 4096 {
        return Err("sample folder path is invalid".to_owned());
    }
    let engine = state.mix_engine.clone();
    let path = PathBuf::from(path.trim());
    tauri::async_runtime::spawn_blocking(move || engine.load_sample_folder(&path))
        .await
        .map_err(|error| format!("sample loading task failed: {error}"))?
}

#[tauri::command]
pub async fn trigger_sample(
    state: State<'_, AppState>,
    sample_id: u32,
    quantization: audio::SampleQuantization,
) -> Result<audio::AudioStatus, String> {
    let engine = state.mix_engine.active_audio_engine();
    tauri::async_runtime::spawn_blocking(move || engine.trigger_sample(sample_id, quantization))
        .await
        .map_err(|error| format!("sample trigger task failed: {error}"))?
}

#[tauri::command]
pub async fn wheel_and_come_again(
    state: State<'_, AppState>,
    sample_id: Option<u32>,
) -> Result<audio::AudioStatus, String> {
    let engine = state.mix_engine.active_audio_engine();
    tauri::async_runtime::spawn_blocking(move || engine.rewind(sample_id))
        .await
        .map_err(|error| format!("rewind task failed: {error}"))?
}

#[tauri::command]
pub fn set_auto_rewind(state: State<'_, AppState>, enabled: bool) -> audio::AudioStatus {
    state
        .mix_engine
        .active_audio_engine()
        .set_auto_rewind(enabled)
}

#[tauri::command]
pub fn start_mix_engine(
    state: State<'_, AppState>,
    app: AppHandle,
    track_id: i64,
    preset: mix_engine::MixPreset,
) -> Result<mix_engine::MixStatus, String> {
    state
        .mix_engine
        .set_stem_profile(active_stem_profile(&state, &app)?)?;
    #[cfg(target_os = "android")]
    app.device_identity().initialize_audio_context()?;
    state.mix_engine.start(track_id, preset)
}

#[tauri::command]
pub fn stop_mix_engine(state: State<'_, AppState>) -> Result<mix_engine::MixStatus, String> {
    state.mix_engine.stop()
}

#[tauri::command]
pub fn set_mix_engine_preset(
    state: State<'_, AppState>,
    preset: mix_engine::MixPreset,
) -> Result<mix_engine::MixStatus, String> {
    state.mix_engine.set_preset(preset)
}

#[tauri::command]
pub fn set_mix_engine_intensity(
    state: State<'_, AppState>,
    intensity: Option<u8>,
) -> Result<mix_engine::MixStatus, String> {
    state.mix_engine.set_intensity(intensity)
}

#[tauri::command]
pub fn force_next_track(state: State<'_, AppState>) -> Result<mix_engine::MixStatus, String> {
    state.mix_engine.force_next()
}

#[tauri::command]
pub fn get_mix_engine_status(state: State<'_, AppState>) -> mix_engine::MixStatus {
    state.mix_engine.status()
}

#[tauri::command]
pub fn get_sample_directory(app: AppHandle) -> Result<String, String> {
    let directory = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("cannot resolve app data folder: {error}"))?
        .join("samples");
    std::fs::create_dir_all(&directory).map_err(|error| {
        format!(
            "cannot create sample folder {}: {error}",
            directory.display()
        )
    })?;
    Ok(directory.to_string_lossy().into_owned())
}

fn stem_files_from_rows(rows: Vec<database::StemPath>) -> Result<audio::StemFiles, String> {
    let profile = profile_for_available_stems(&rows, 4)?;
    stem_files_for_profile(rows, usize::from(profile))
}

fn profile_for_available_stems(rows: &[database::StemPath], requested: u8) -> Result<u8, String> {
    if !matches!(requested, 2 | 4) {
        return Err("active stem profile must be 2 or 4".to_owned());
    }
    let kinds = rows
        .iter()
        .map(|row| row.kind.trim().to_ascii_lowercase())
        .collect::<std::collections::HashSet<_>>();
    let has_bass = kinds.contains("bass");
    let has_drums = kinds.contains("drums");
    if requested == 4 && has_bass != has_drums {
        return Err("four-stem tracks must contain both bass and drums, or neither".to_owned());
    }
    if requested == 4 && kinds.contains("acapella") && kinds.contains("instrumental") && !has_bass {
        Ok(2)
    } else {
        Ok(requested)
    }
}

fn stem_files_for_profile(
    rows: Vec<database::StemPath>,
    active_stems: usize,
) -> Result<audio::StemFiles, String> {
    if !matches!(active_stems, 2 | 4) {
        return Err("active stem profile must be 2 or 4".to_owned());
    }
    let mut paths = HashMap::<String, PathBuf>::with_capacity(4);
    for row in rows {
        let kind = row.kind.trim().to_ascii_lowercase();
        if !matches!(
            kind.as_str(),
            "acapella" | "instrumental" | "bass" | "drums"
        ) || (active_stems == 2 && !matches!(kind.as_str(), "acapella" | "instrumental"))
        {
            continue;
        }
        let path = PathBuf::from(row.file_path);
        crate::import::ensure_local_path(&path)
            .map_err(|error| format!("{kind} stems must be stored locally: {error:#}"))?;
        if paths.insert(kind.clone(), path).is_some() {
            return Err(format!("duplicate {kind} stem path on the track"));
        }
    }

    let mut take = |kind: &str| {
        paths
            .remove(kind)
            .ok_or_else(|| format!("track is missing its required {kind} stem"))
    };
    let acapella = take("acapella")?;
    let instrumental = take("instrumental")?;
    let (bass, drums) = if active_stems == 4 {
        (take("bass")?, take("drums")?)
    } else {
        // create_session skips these lanes in the Android Demo profile; duplicate
        // local paths keep the shared four-lane engine shape without extra I/O.
        (instrumental.clone(), instrumental.clone())
    };
    Ok(audio::StemFiles {
        acapella,
        instrumental,
        bass,
        drums,
    })
}

fn validate_track(input: &database::TrackInput) -> Result<(), String> {
    if input.file_path.trim().is_empty() || input.file_path.len() > 4096 {
        return Err("track file path is invalid".to_owned());
    }
    if input.title.trim().is_empty() || input.title.len() > 512 || input.artist.len() > 512 {
        return Err("track title or artist is invalid".to_owned());
    }
    if input.duration_ms.is_some_and(|duration| duration < 0) {
        return Err("track duration cannot be negative".to_owned());
    }
    if input
        .codec
        .as_ref()
        .is_some_and(|codec| codec.trim().is_empty() || codec.len() > 64)
    {
        return Err("audio codec name is invalid".to_owned());
    }
    if input
        .bitrate_bps
        .is_some_and(|bitrate| bitrate == 0 || bitrate > 100_000_000)
    {
        return Err("audio bitrate must be between 1 and 100,000,000 bits per second".to_owned());
    }
    if input
        .sample_rate
        .is_some_and(|rate| !(8_000..=768_000).contains(&rate))
    {
        return Err("audio sample rate must be between 8,000 and 768,000 Hz".to_owned());
    }
    if input
        .channel_count
        .is_some_and(|channels| !(1..=64).contains(&channels))
    {
        return Err("audio channel count must be between 1 and 64".to_owned());
    }
    if input
        .tags_json
        .as_ref()
        .is_some_and(|json| json.len() > 1_048_576)
    {
        return Err("audio metadata tags are too large".to_owned());
    }
    if let Some(json) = &input.tags_json {
        serde_json::from_str::<std::collections::BTreeMap<String, Vec<String>>>(json)
            .map_err(|_| "audio metadata tags must be a JSON object of string lists".to_owned())?;
    }
    if input
        .bpm
        .is_some_and(|bpm| !bpm.is_finite() || !(20.0..=400.0).contains(&bpm))
    {
        return Err("BPM must be a finite value between 20 and 400".to_owned());
    }
    if input
        .sentiment
        .is_some_and(|score| !score.is_finite() || !(-1.0..=1.0).contains(&score))
    {
        return Err("sentiment must be a finite value between -1 and 1".to_owned());
    }
    if input
        .beat_grid_json
        .as_ref()
        .is_some_and(|json| json.len() > 2_000_000)
    {
        return Err("beat grid is too large".to_owned());
    }
    if let Some(json) = &input.beat_grid_json {
        serde_json::from_str::<serde_json::Value>(json)
            .map_err(|_| "beat grid must be valid JSON".to_owned())?;
    }
    if input.stems.len() > 32 {
        return Err("a track can contain at most 32 stem paths".to_owned());
    }
    for stem in &input.stems {
        if stem.kind.trim().is_empty()
            || stem.kind.len() > 48
            || stem.file_path.trim().is_empty()
            || stem.file_path.len() > 4096
        {
            return Err("stem kind or file path is invalid".to_owned());
        }
    }
    Ok(())
}

async fn post_json<TRequest, TResponse>(url: &str, request: &TRequest) -> Result<TResponse, String>
where
    TRequest: Serialize + ?Sized,
    TResponse: DeserializeOwned,
{
    let response = reqwest::Client::new()
        .post(url)
        .json(request)
        .send()
        .await
        .map_err(|error| format!("issuer request failed: {error}"))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(format!("issuer returned {status}: {body}"));
    }
    response
        .json::<TResponse>()
        .await
        .map_err(|error| format!("issuer response was invalid: {error}"))
}

fn issuer_endpoint(base: &str, path: &str) -> Result<String, String> {
    let mut url =
        Url::parse(base.trim()).map_err(|error| format!("invalid issuer URL: {error}"))?;
    let allowed_scheme = url.scheme() == "https"
        || (url.scheme() == "http"
            && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1")));
    if !allowed_scheme || url.host_str().is_none() {
        return Err("issuer URL must use HTTPS (HTTP is allowed only for localhost)".to_owned());
    }
    url.set_path(path);
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.to_string())
}

#[cfg(test)]
mod audio_command_tests {
    use super::{
        audio, database, parse_tts_android_base64_payload, parse_tts_ipc_metadata,
        raw_tts_ipc_body, stem_files_for_profile, stem_files_from_rows,
    };
    use base64::Engine as _;
    use std::path::PathBuf;

    fn stem(kind: &str, path: &str) -> database::StemPath {
        database::StemPath {
            kind: kind.to_owned(),
            file_path: path.to_owned(),
        }
    }

    fn local_test_path(name: &str) -> String {
        std::env::temp_dir()
            .join(name)
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn binary_tts_ipc_requires_float32_little_endian_and_beat_or_bar_quantization() {
        let metadata = parse_tts_ipc_metadata("24000", "1", "f32le", "bar").unwrap();
        assert_eq!(metadata.sample_rate, 24_000);
        assert_eq!(metadata.channels, 1);
        assert!(matches!(
            metadata.quantization,
            audio::SampleQuantization::Bar
        ));
        assert!(parse_tts_ipc_metadata("24000", "1", "json", "bar").is_err());
        assert!(parse_tts_ipc_metadata("24000", "1", "f32le", "off").is_err());
        assert!(parse_tts_ipc_metadata("24000", "3", "f32le", "beat").is_err());
    }

    #[test]
    fn tts_raw_ipc_accepts_binary_bodies_and_rejects_json_number_arrays() {
        let bytes = [0x00, 0x00, 0x80, 0x3e];
        let body = tauri::ipc::InvokeBody::Raw(bytes.to_vec());
        assert_eq!(raw_tts_ipc_body(&body).unwrap(), &bytes[..]);
        let json = tauri::ipc::InvokeBody::Json(serde_json::json!([0.25]));
        assert!(raw_tts_ipc_body(&json).is_err());
    }

    #[test]
    fn android_base64_tts_fallback_preserves_binary_pcm_without_number_arrays() {
        let mut pcm = Vec::new();
        pcm.extend_from_slice(&0.25_f32.to_le_bytes());
        pcm.extend_from_slice(&(-0.5_f32).to_le_bytes());
        let payload = serde_json::json!({
            "pcmBase64": base64::engine::general_purpose::STANDARD.encode(&pcm),
            "sampleRate": 24_000,
            "channels": 1,
            "encoding": "f32le",
            "quantization": "beat"
        });
        let (decoded, metadata) = parse_tts_android_base64_payload(&payload).unwrap();
        assert_eq!(decoded, pcm);
        assert_eq!(metadata.sample_rate, 24_000);
        assert!(matches!(
            metadata.quantization,
            audio::SampleQuantization::Beat
        ));
        let frames =
            audio::decode_tts_pcm_f32le(&decoded, metadata.sample_rate, metadata.channels).unwrap();
        assert_eq!(frames[0].left, 0.25);
        assert_eq!(frames[1].right, -0.5);

        let number_array = serde_json::json!({ "pcm": [0.25, -0.5], "sampleRate": 24_000 });
        assert!(parse_tts_android_base64_payload(&number_array).is_err());
    }

    #[test]
    fn stem_rows_are_mapped_to_the_fixed_mix_order() {
        let drums = local_test_path("championvibe-test-drums.wav");
        let bass = local_test_path("championvibe-test-bass.wav");
        let instrumental = local_test_path("championvibe-test-instrumental.wav");
        let acapella = local_test_path("championvibe-test-acapella.wav");
        let files = stem_files_from_rows(vec![
            stem("drums", &drums),
            stem("BASS", &bass),
            stem("instrumental", &instrumental),
            stem("acapella", &acapella),
            stem("other", "ignored.wav"),
        ])
        .unwrap();

        assert_eq!(files.acapella, PathBuf::from(acapella));
        assert_eq!(files.instrumental, PathBuf::from(instrumental));
        assert_eq!(files.bass, PathBuf::from(bass));
        assert_eq!(files.drums, PathBuf::from(drums));
    }

    #[test]
    fn android_demo_profile_requires_only_acapella_and_instrumental_rows() {
        let acapella = local_test_path("championvibe-demo-acapella.wav");
        let instrumental = local_test_path("championvibe-demo-instrumental.wav");
        let files = stem_files_for_profile(
            vec![
                stem("acapella", &acapella),
                stem("instrumental", &instrumental),
                stem("bass", ""),
                stem("drums", ""),
            ],
            2,
        )
        .unwrap();
        assert_eq!(files.acapella, PathBuf::from(acapella.clone()));
        assert_eq!(files.instrumental, PathBuf::from(instrumental.clone()));
        assert_eq!(files.bass, PathBuf::from(instrumental.clone()));
        assert_eq!(files.drums, PathBuf::from(instrumental.clone()));
        assert!(stem_files_for_profile(vec![stem("acapella", &acapella)], 4).is_err());
    }

    #[test]
    fn missing_stem_is_rejected_before_audio_threads_start() {
        let error = stem_files_from_rows(vec![
            stem("acapella", "a.wav"),
            stem("instrumental", "i.wav"),
            stem("bass", "b.wav"),
        ])
        .unwrap_err();
        assert!(error.contains("drums"));
    }
}
