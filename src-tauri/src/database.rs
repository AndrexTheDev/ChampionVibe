use crate::localization;
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::{path::Path, time::SystemTime};

#[derive(Debug, Clone, Deserialize)]
pub struct StemPath {
    pub kind: String,
    pub file_path: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TrackInput {
    pub file_path: String,
    pub title: String,
    pub artist: String,
    pub duration_ms: Option<i64>,
    #[serde(default)]
    pub codec: Option<String>,
    #[serde(default)]
    pub bitrate_bps: Option<u64>,
    #[serde(default)]
    pub sample_rate: Option<u32>,
    #[serde(default)]
    pub channel_count: Option<u32>,
    #[serde(default)]
    pub tags_json: Option<String>,
    pub bpm: Option<f64>,
    pub beat_grid_json: Option<String>,
    pub sentiment: Option<f64>,
    #[serde(default)]
    pub stems: Vec<StemPath>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackSummary {
    pub id: i64,
    pub file_path: String,
    pub title: String,
    pub artist: String,
    pub duration_ms: Option<i64>,
    pub codec: Option<String>,
    pub bitrate_bps: Option<u64>,
    pub sample_rate: Option<u32>,
    pub channel_count: Option<u32>,
    pub tags_json: String,
    pub bpm: Option<f64>,
    pub sentiment: Option<f64>,
    pub musical_key: Option<String>,
    pub acoustic_tag: Option<String>,
    pub lufs_integrated: Option<f64>,
    pub true_peak_dbtp: Option<f64>,
    pub gain_offset_db: Option<f64>,
    pub mood_valence: Option<f64>,
    pub mood_arousal: Option<f64>,
    pub mood_label: Option<String>,
    pub rhythm_pattern: Option<String>,
    pub style_genre: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TrackSearchFilters {
    pub mood: Option<String>,
    pub rhythm: Option<String>,
    pub key: Option<String>,
    pub bpm_min: Option<f64>,
    pub bpm_max: Option<f64>,
    pub lufs_min: Option<f64>,
    pub lufs_max: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct MixTrackRecord {
    pub id: i64,
    pub file_path: String,
    pub title: String,
    pub artist: String,
    pub duration_ms: Option<i64>,
    pub bpm: Option<f64>,
    pub beat_grid_json: Option<String>,
    pub sentiment: Option<f64>,
    pub sentiment_label: Option<String>,
    pub acoustic_tag: Option<String>,
    pub energy_density: Option<f64>,
    pub musical_key: Option<String>,
    pub riddim_family_id: Option<i64>,
    pub integrated_lufs: Option<f64>,
    pub mood_valence: Option<f64>,
    pub mood_arousal: Option<f64>,
    pub mood_label: Option<String>,
    pub rhythm_pattern: Option<String>,
    pub rhythm_confidence: Option<f64>,
    pub tags: Vec<(String, String)>,
    pub stems: Vec<StemPath>,
    pub words: Vec<LyricWord>,
}

#[derive(Debug, Clone)]
pub struct LyricWord {
    pub text: String,
    pub start_ms: i64,
    pub end_ms: i64,
    pub confidence: f64,
}

#[derive(Debug, Clone)]
pub struct ImportResult {
    pub source_path: String,
    pub title: String,
    pub artist: String,
    pub duration_ms: i64,
    pub source_sample_rate: u32,
    pub source_channels: u32,
    pub source_bitrate_bps: Option<u64>,
    pub codec: String,
    pub tags_json: String,
    pub bpm: Option<f64>,
    pub bpm_confidence: f64,
    pub beat_grid_json: String,
    pub musical_key: Option<String>,
    pub key_confidence: f64,
    pub energy_density: f64,
    pub energy_dbfs: f64,
    pub dynamic_range_db: f64,
    pub onset_density: f64,
    pub spectral_centroid_hz: f64,
    pub low_band_ratio: f64,
    pub mid_band_ratio: f64,
    pub high_band_ratio: f64,
    pub acoustic_tag: String,
    pub acoustic_scores_json: String,
    pub lufs_integrated: Option<f64>,
    pub true_peak_dbtp: Option<f64>,
    pub gain_offset_db: Option<f64>,
    pub mood_valence: f64,
    pub mood_arousal: f64,
    pub mood_label: String,
    pub rhythm_pattern: String,
    pub rhythm_confidence: f64,
    pub style_genre: String,
    pub vector_blob: Vec<u8>,
    pub vector_kind: String,
    pub transcript: String,
    pub transcript_language: Option<String>,
    pub lyric_sentiment: f64,
    pub lyric_sentiment_label: String,
    pub lyric_sentiment_confidence: f64,
    pub words: Vec<LyricWord>,
    pub stems: Vec<StemPath>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportJobSnapshot {
    pub id: i64,
    pub status: String,
    pub status_message: localization::LocalizedMessage,
    pub total_items: i64,
    pub queued_items: i64,
    pub running_items: i64,
    pub successful_items: i64,
    pub failed_items: i64,
    pub cancelled_items: i64,
    pub current_path: Option<String>,
    pub current_stage: Option<String>,
    pub current_stage_message: Option<localization::LocalizedMessage>,
    pub current_progress: f64,
    pub error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone)]
pub struct ImportItem {
    pub id: i64,
    pub job_id: i64,
    pub source_path: String,
}

#[derive(Debug, Clone, Default)]
pub struct RecordingMetadata {
    pub format: Option<String>,
    pub sample_rate: Option<u32>,
    pub channel_count: Option<u32>,
    pub bit_depth: Option<u32>,
    pub bitrate_bps: Option<u32>,
    pub file_size_bytes: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct ImportJobConfig {
    pub id: i64,
    pub separation_model_path: String,
    pub whisper_model_path: String,
    pub use_directml: bool,
    pub cpu_threads: usize,
}

pub fn open(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("could not create app data directory {}", parent.display()))?;
    }
    let connection = Connection::open(path)
        .with_context(|| format!("could not open SQLite database {}", path.display()))?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.pragma_update(None, "temp_store", "MEMORY")?;
    connection.pragma_update(None, "cache_size", -65_536_i64)?;
    connection.pragma_update(None, "mmap_size", 268_435_456_i64)?;
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS tracks (
            id INTEGER PRIMARY KEY,
            file_path TEXT NOT NULL UNIQUE,
            title TEXT NOT NULL,
            artist TEXT NOT NULL DEFAULT '',
            duration_ms INTEGER,
            codec TEXT,
            bitrate_bps INTEGER,
            sample_rate INTEGER,
            channel_count INTEGER,
            tags_json TEXT NOT NULL DEFAULT '{}',
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_tracks_title ON tracks(title COLLATE NOCASE);
        CREATE INDEX IF NOT EXISTS idx_tracks_artist ON tracks(artist COLLATE NOCASE);
        CREATE INDEX IF NOT EXISTS idx_tracks_updated ON tracks(updated_at DESC);

        CREATE TABLE IF NOT EXISTS beat_grids (
            track_id INTEGER PRIMARY KEY REFERENCES tracks(id) ON DELETE CASCADE,
            bpm REAL,
            grid_json TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_beat_grids_bpm ON beat_grids(bpm);

        CREATE TABLE IF NOT EXISTS waveform_peak_cache (
            track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
            peak_count INTEGER NOT NULL CHECK(peak_count BETWEEN 128 AND 2048),
            peaks_blob BLOB NOT NULL CHECK(length(peaks_blob) = peak_count * 2),
            updated_at INTEGER NOT NULL,
            PRIMARY KEY(track_id, peak_count)
        );

        CREATE TABLE IF NOT EXISTS sentiment_values (
            track_id INTEGER PRIMARY KEY REFERENCES tracks(id) ON DELETE CASCADE,
            score REAL NOT NULL CHECK(score >= -1.0 AND score <= 1.0),
            label TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_sentiment_score ON sentiment_values(score);

        CREATE TABLE IF NOT EXISTS stems (
            track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
            kind TEXT NOT NULL,
            file_path TEXT NOT NULL,
            PRIMARY KEY(track_id, kind)
        );
        CREATE INDEX IF NOT EXISTS idx_stems_path ON stems(file_path);

        CREATE TABLE IF NOT EXISTS audio_tags (
            track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
            tag_key TEXT NOT NULL,
            tag_value TEXT NOT NULL,
            PRIMARY KEY(track_id, tag_key, tag_value)
        );
        CREATE INDEX IF NOT EXISTS idx_audio_tags_value
            ON audio_tags(tag_value COLLATE NOCASE, track_id);
        CREATE INDEX IF NOT EXISTS idx_audio_tags_key
            ON audio_tags(tag_key COLLATE NOCASE, track_id);

        CREATE TABLE IF NOT EXISTS track_tags (
            id INTEGER PRIMARY KEY,
            track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
            namespace TEXT NOT NULL CHECK(namespace IN ('riddim', 'producer', 'genre', 'label', 'custom')),
            tag TEXT NOT NULL,
            normalized_tag TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            UNIQUE(track_id, namespace, normalized_tag)
        );
        CREATE INDEX IF NOT EXISTS idx_track_tags_lookup
            ON track_tags(namespace, normalized_tag, track_id);

        CREATE TABLE IF NOT EXISTS instrumental_fingerprints (
            track_id INTEGER PRIMARY KEY REFERENCES tracks(id) ON DELETE CASCADE,
            source_path TEXT NOT NULL,
            algorithm_version INTEGER NOT NULL,
            features BLOB NOT NULL,
            simhash INTEGER NOT NULL,
            fingerprint_hash TEXT NOT NULL,
            created_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_instrumental_fingerprints_hash
            ON instrumental_fingerprints(fingerprint_hash);

        CREATE TABLE IF NOT EXISTS fingerprint_bands (
            track_id INTEGER NOT NULL REFERENCES instrumental_fingerprints(track_id) ON DELETE CASCADE,
            band INTEGER NOT NULL CHECK(band BETWEEN 0 AND 7),
            band_value INTEGER NOT NULL CHECK(band_value BETWEEN 0 AND 255),
            PRIMARY KEY(track_id, band)
        );
        CREATE INDEX IF NOT EXISTS idx_fingerprint_bands_lookup
            ON fingerprint_bands(band, band_value, track_id);

        CREATE TABLE IF NOT EXISTS riddim_families (
            id INTEGER PRIMARY KEY,
            family_key TEXT NOT NULL UNIQUE,
            name TEXT NOT NULL,
            track_count INTEGER NOT NULL CHECK(track_count >= 2),
            created_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_riddim_families_name ON riddim_families(name COLLATE NOCASE);

        CREATE TABLE IF NOT EXISTS riddim_family_tracks (
            track_id INTEGER PRIMARY KEY REFERENCES tracks(id) ON DELETE CASCADE,
            family_id INTEGER NOT NULL REFERENCES riddim_families(id) ON DELETE CASCADE,
            match_score REAL NOT NULL CHECK(match_score >= 0.0 AND match_score <= 1.0),
            match_method TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_riddim_family_tracks_family
            ON riddim_family_tracks(family_id, track_id);

        CREATE TABLE IF NOT EXISTS audio_analysis (
            track_id INTEGER PRIMARY KEY REFERENCES tracks(id) ON DELETE CASCADE,
            musical_key TEXT,
            key_confidence REAL NOT NULL DEFAULT 0.0 CHECK(key_confidence >= 0.0 AND key_confidence <= 1.0),
            bpm_confidence REAL NOT NULL DEFAULT 0.0 CHECK(bpm_confidence >= 0.0 AND bpm_confidence <= 1.0),
            source_sample_rate INTEGER NOT NULL,
            source_channels INTEGER NOT NULL,
            energy_density REAL NOT NULL,
            energy_dbfs REAL NOT NULL,
            dynamic_range_db REAL NOT NULL,
            onset_density REAL NOT NULL,
            spectral_centroid_hz REAL NOT NULL,
            low_band_ratio REAL NOT NULL,
            mid_band_ratio REAL NOT NULL,
            high_band_ratio REAL NOT NULL,
            acoustic_tag TEXT NOT NULL CHECK(acoustic_tag IN ('Aggressiv', 'Deep', 'Conscious', 'Hype')),
            acoustic_scores_json TEXT NOT NULL,
            analyzer_version TEXT NOT NULL,
            analyzed_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_audio_analysis_key ON audio_analysis(musical_key);
        CREATE INDEX IF NOT EXISTS idx_audio_analysis_tag ON audio_analysis(acoustic_tag);

        CREATE TABLE IF NOT EXISTS lyrics_transcripts (
            track_id INTEGER PRIMARY KEY REFERENCES tracks(id) ON DELETE CASCADE,
            model TEXT NOT NULL,
            language TEXT,
            transcript TEXT NOT NULL,
            sentiment_score REAL NOT NULL CHECK(sentiment_score >= -1.0 AND sentiment_score <= 1.0),
            sentiment_label TEXT NOT NULL CHECK(sentiment_label IN ('positive', 'neutral', 'negative')),
            sentiment_confidence REAL NOT NULL CHECK(sentiment_confidence >= 0.0 AND sentiment_confidence <= 1.0),
            created_at INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS lyric_words (
            track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
            word_index INTEGER NOT NULL,
            word TEXT NOT NULL,
            start_ms INTEGER NOT NULL CHECK(start_ms >= 0),
            end_ms INTEGER NOT NULL CHECK(end_ms >= start_ms),
            confidence REAL NOT NULL CHECK(confidence >= 0.0 AND confidence <= 1.0),
            PRIMARY KEY(track_id, word_index)
        );
        CREATE INDEX IF NOT EXISTS idx_lyric_words_time ON lyric_words(track_id, start_ms, end_ms);
        CREATE INDEX IF NOT EXISTS idx_lyric_words_token_lookup
            ON lyric_words(lower(trim(word, ' .,!?;:')));

        CREATE TABLE IF NOT EXISTS import_jobs (
            id INTEGER PRIMARY KEY,
            status TEXT NOT NULL CHECK(status IN ('queued', 'running', 'completed', 'failed', 'cancelled')),
            total_items INTEGER NOT NULL CHECK(total_items >= 0),
            queued_items INTEGER NOT NULL DEFAULT 0 CHECK(queued_items >= 0),
            running_items INTEGER NOT NULL DEFAULT 0 CHECK(running_items >= 0),
            successful_items INTEGER NOT NULL DEFAULT 0 CHECK(successful_items >= 0),
            failed_items INTEGER NOT NULL DEFAULT 0 CHECK(failed_items >= 0),
            cancelled_items INTEGER NOT NULL DEFAULT 0 CHECK(cancelled_items >= 0),
            cancel_requested INTEGER NOT NULL DEFAULT 0 CHECK(cancel_requested IN (0, 1)),
            separation_model_path TEXT NOT NULL DEFAULT '',
            whisper_model_path TEXT NOT NULL DEFAULT '',
            use_directml INTEGER NOT NULL DEFAULT 0 CHECK(use_directml IN (0, 1)),
            cpu_threads INTEGER NOT NULL DEFAULT 4 CHECK(cpu_threads BETWEEN 1 AND 8),
            current_path TEXT,
            current_stage TEXT,
            current_progress REAL NOT NULL DEFAULT 0.0 CHECK(current_progress >= 0.0 AND current_progress <= 1.0),
            error TEXT,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            started_at INTEGER,
            finished_at INTEGER
        );
        CREATE INDEX IF NOT EXISTS idx_import_jobs_status ON import_jobs(status, created_at);

        CREATE TABLE IF NOT EXISTS import_items (
            id INTEGER PRIMARY KEY,
            job_id INTEGER NOT NULL REFERENCES import_jobs(id) ON DELETE CASCADE,
            source_path TEXT NOT NULL,
            track_id INTEGER REFERENCES tracks(id) ON DELETE SET NULL,
            status TEXT NOT NULL CHECK(status IN ('queued', 'running', 'succeeded', 'failed', 'cancelled')),
            error TEXT,
            created_at INTEGER NOT NULL,
            started_at INTEGER,
            finished_at INTEGER,
            UNIQUE(job_id, source_path)
        );
        CREATE INDEX IF NOT EXISTS idx_import_items_pending ON import_items(job_id, status, id);

        CREATE TABLE IF NOT EXISTS recordings (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            file_path TEXT NOT NULL,
            duration_ms INTEGER,
            format TEXT NOT NULL DEFAULT 'unknown',
            sample_rate INTEGER,
            channel_count INTEGER,
            bit_depth INTEGER,
            bitrate_bps INTEGER,
            file_size_bytes INTEGER,
            status TEXT NOT NULL DEFAULT 'complete'
                CHECK(status IN ('recording', 'complete', 'failed', 'aborted')),
            error TEXT,
            created_at INTEGER NOT NULL,
            finished_at INTEGER
        );
        CREATE INDEX IF NOT EXISTS idx_recordings_created ON recordings(created_at DESC);

        CREATE TABLE IF NOT EXISTS licenses (
            id INTEGER PRIMARY KEY CHECK(id = 1),
            signed_json TEXT NOT NULL,
            saved_at INTEGER NOT NULL
        );",
    )?;
    ensure_track_metadata_columns(&connection)?;
    ensure_recording_columns(&connection)?;
    migrate_track_analysis(&connection)?;
    connection.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_tracks_codec ON tracks(codec COLLATE NOCASE);
         CREATE INDEX IF NOT EXISTS idx_tracks_bitrate ON tracks(bitrate_bps);
         CREATE INDEX IF NOT EXISTS idx_tracks_sample_rate ON tracks(sample_rate);",
    )?;
    let counter_columns_added = ensure_import_job_columns(&connection)?;
    connection.execute_batch(
        "CREATE TRIGGER IF NOT EXISTS trg_import_items_count_insert
         AFTER INSERT ON import_items
         BEGIN
             UPDATE import_jobs SET
                 total_items = total_items + 1,
                 queued_items = queued_items + CASE WHEN NEW.status = 'queued' THEN 1 ELSE 0 END,
                 running_items = running_items + CASE WHEN NEW.status = 'running' THEN 1 ELSE 0 END,
                 successful_items = successful_items + CASE WHEN NEW.status = 'succeeded' THEN 1 ELSE 0 END,
                 failed_items = failed_items + CASE WHEN NEW.status = 'failed' THEN 1 ELSE 0 END,
                 cancelled_items = cancelled_items + CASE WHEN NEW.status = 'cancelled' THEN 1 ELSE 0 END
             WHERE id = NEW.job_id;
         END;
         CREATE TRIGGER IF NOT EXISTS trg_import_items_count_status
         AFTER UPDATE OF status ON import_items
         WHEN OLD.status != NEW.status
         BEGIN
             UPDATE import_jobs SET
                 queued_items = queued_items - CASE WHEN OLD.status = 'queued' THEN 1 ELSE 0 END
                     + CASE WHEN NEW.status = 'queued' THEN 1 ELSE 0 END,
                 running_items = running_items - CASE WHEN OLD.status = 'running' THEN 1 ELSE 0 END
                     + CASE WHEN NEW.status = 'running' THEN 1 ELSE 0 END,
                 successful_items = successful_items - CASE WHEN OLD.status = 'succeeded' THEN 1 ELSE 0 END
                     + CASE WHEN NEW.status = 'succeeded' THEN 1 ELSE 0 END,
                 failed_items = failed_items - CASE WHEN OLD.status = 'failed' THEN 1 ELSE 0 END
                     + CASE WHEN NEW.status = 'failed' THEN 1 ELSE 0 END,
                 cancelled_items = cancelled_items - CASE WHEN OLD.status = 'cancelled' THEN 1 ELSE 0 END
                     + CASE WHEN NEW.status = 'cancelled' THEN 1 ELSE 0 END
             WHERE id = NEW.job_id;
         END;
         CREATE TRIGGER IF NOT EXISTS trg_import_items_count_delete
         AFTER DELETE ON import_items
         BEGIN
             UPDATE import_jobs SET
                 total_items = total_items - 1,
                 queued_items = queued_items - CASE WHEN OLD.status = 'queued' THEN 1 ELSE 0 END,
                 running_items = running_items - CASE WHEN OLD.status = 'running' THEN 1 ELSE 0 END,
                 successful_items = successful_items - CASE WHEN OLD.status = 'succeeded' THEN 1 ELSE 0 END,
                 failed_items = failed_items - CASE WHEN OLD.status = 'failed' THEN 1 ELSE 0 END,
                 cancelled_items = cancelled_items - CASE WHEN OLD.status = 'cancelled' THEN 1 ELSE 0 END
             WHERE id = OLD.job_id;
         END;",
    )?;
    if counter_columns_added {
        connection.execute_batch(
            "UPDATE import_jobs SET
                total_items = (SELECT COUNT(*) FROM import_items WHERE job_id = import_jobs.id),
                queued_items = (SELECT COUNT(*) FROM import_items WHERE job_id = import_jobs.id AND status = 'queued'),
                running_items = (SELECT COUNT(*) FROM import_items WHERE job_id = import_jobs.id AND status = 'running'),
                successful_items = (SELECT COUNT(*) FROM import_items WHERE job_id = import_jobs.id AND status = 'succeeded'),
                failed_items = (SELECT COUNT(*) FROM import_items WHERE job_id = import_jobs.id AND status = 'failed'),
                cancelled_items = (SELECT COUNT(*) FROM import_items WHERE job_id = import_jobs.id AND status = 'cancelled')",
        )?;
    }
    Ok(connection)
}

/// Idempotent migration for standards-based loudness and MIR metadata. It runs at startup, not in
/// the import UI path, and the whole relation is keyed by track_id for indexed point reads.
fn migrate_track_analysis(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS track_analysis (
            track_id INTEGER PRIMARY KEY REFERENCES tracks(id) ON DELETE CASCADE,
            lufs_integrated REAL,
            true_peak_dbtp REAL,
            gain_offset_db REAL,
            mood_valence REAL NOT NULL CHECK(mood_valence >= -1.0 AND mood_valence <= 1.0),
            mood_arousal REAL NOT NULL CHECK(mood_arousal >= 0.0 AND mood_arousal <= 1.0),
            mood_label TEXT NOT NULL,
            rhythm_pattern TEXT NOT NULL,
            rhythm_confidence REAL NOT NULL CHECK(rhythm_confidence >= 0.0 AND rhythm_confidence <= 1.0),
            style_genre TEXT NOT NULL,
            vector_blob BLOB NOT NULL CHECK(length(vector_blob) = 64),
            vector_kind TEXT NOT NULL,
            analyzer_version TEXT NOT NULL,
            analyzed_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_track_analysis_mood
            ON track_analysis(mood_label COLLATE NOCASE, track_id);
        CREATE INDEX IF NOT EXISTS idx_track_analysis_rhythm
            ON track_analysis(rhythm_pattern COLLATE NOCASE, track_id);
        CREATE INDEX IF NOT EXISTS idx_track_analysis_style
            ON track_analysis(style_genre COLLATE NOCASE, track_id);
        CREATE INDEX IF NOT EXISTS idx_track_analysis_loudness
            ON track_analysis(lufs_integrated, track_id);",
    )?;
    Ok(())
}

fn ensure_track_metadata_columns(connection: &Connection) -> Result<()> {
    let columns = {
        let mut statement = connection.prepare("PRAGMA table_info(tracks)")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (name, declaration) in [
        ("codec", "TEXT"),
        ("bitrate_bps", "INTEGER"),
        ("sample_rate", "INTEGER"),
        ("channel_count", "INTEGER"),
        ("tags_json", "TEXT NOT NULL DEFAULT '{}'"),
    ] {
        if !columns.iter().any(|column| column == name) {
            connection.execute_batch(&format!(
                "ALTER TABLE tracks ADD COLUMN {name} {declaration}"
            ))?;
        }
    }
    Ok(())
}

fn ensure_recording_columns(connection: &Connection) -> Result<()> {
    let columns = {
        let mut statement = connection.prepare("PRAGMA table_info(recordings)")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (name, declaration) in [
        ("format", "TEXT NOT NULL DEFAULT 'unknown'"),
        ("sample_rate", "INTEGER"),
        ("channel_count", "INTEGER"),
        ("bit_depth", "INTEGER"),
        ("bitrate_bps", "INTEGER"),
        ("file_size_bytes", "INTEGER"),
        (
            "status",
            "TEXT NOT NULL DEFAULT 'complete' CHECK(status IN ('recording', 'complete', 'failed', 'aborted'))",
        ),
        ("error", "TEXT"),
        ("finished_at", "INTEGER"),
    ] {
        if !columns.iter().any(|column| column == name) {
            connection.execute_batch(&format!(
                "ALTER TABLE recordings ADD COLUMN {name} {declaration}"
            ))?;
        }
    }
    Ok(())
}

fn ensure_import_job_columns(connection: &Connection) -> Result<bool> {
    let columns = {
        let mut statement = connection.prepare("PRAGMA table_info(import_jobs)")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let counter_columns = [
        "queued_items",
        "running_items",
        "successful_items",
        "failed_items",
        "cancelled_items",
    ];
    let counter_columns_added = counter_columns
        .iter()
        .any(|name| !columns.iter().any(|column| column == name));
    for (name, declaration) in [
        ("separation_model_path", "TEXT NOT NULL DEFAULT ''"),
        ("whisper_model_path", "TEXT NOT NULL DEFAULT ''"),
        ("use_directml", "INTEGER NOT NULL DEFAULT 0 CHECK(use_directml IN (0, 1))"),
        ("cpu_threads", "INTEGER NOT NULL DEFAULT 4 CHECK(cpu_threads BETWEEN 1 AND 8)"),
        ("current_progress", "REAL NOT NULL DEFAULT 0.0 CHECK(current_progress >= 0.0 AND current_progress <= 1.0)"),
        ("queued_items", "INTEGER NOT NULL DEFAULT 0 CHECK(queued_items >= 0)"),
        ("running_items", "INTEGER NOT NULL DEFAULT 0 CHECK(running_items >= 0)"),
        ("successful_items", "INTEGER NOT NULL DEFAULT 0 CHECK(successful_items >= 0)"),
        ("failed_items", "INTEGER NOT NULL DEFAULT 0 CHECK(failed_items >= 0)"),
        ("cancelled_items", "INTEGER NOT NULL DEFAULT 0 CHECK(cancelled_items >= 0)"),
    ] {
        if !columns.iter().any(|column| column == name) {
            connection.execute_batch(&format!(
                "ALTER TABLE import_jobs ADD COLUMN {name} {declaration}"
            ))?;
        }
    }
    Ok(counter_columns_added)
}

fn replace_audio_tags(transaction: &Transaction<'_>, track_id: i64, tags_json: &str) -> Result<()> {
    let tags = serde_json::from_str::<std::collections::BTreeMap<String, Vec<String>>>(tags_json)
        .context("audio tag metadata must be a JSON object of string lists")?;
    transaction.execute("DELETE FROM audio_tags WHERE track_id = ?1", [track_id])?;
    let mut insert = transaction.prepare_cached(
        "INSERT OR IGNORE INTO audio_tags (track_id, tag_key, tag_value) VALUES (?1, ?2, ?3)",
    )?;
    for (key, values) in tags.into_iter().take(256) {
        let key = key.trim();
        if key.is_empty() || key.len() > 256 {
            continue;
        }
        for value in values.into_iter().take(32) {
            if value.len() <= 4_096 {
                insert.execute(params![track_id, key, value])?;
            }
        }
    }
    Ok(())
}

pub fn upsert_track(connection: &mut Connection, track: &TrackInput) -> Result<i64> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let now = unix_time();
    transaction.execute(
        "INSERT INTO tracks (
            file_path, title, artist, duration_ms, codec, bitrate_bps, sample_rate,
            channel_count, tags_json, created_at, updated_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, COALESCE(?9, '{}'), ?10, ?10)
         ON CONFLICT(file_path) DO UPDATE SET
            title = excluded.title,
            artist = excluded.artist,
            duration_ms = excluded.duration_ms,
            codec = COALESCE(excluded.codec, tracks.codec),
            bitrate_bps = COALESCE(excluded.bitrate_bps, tracks.bitrate_bps),
            sample_rate = COALESCE(excluded.sample_rate, tracks.sample_rate),
            channel_count = COALESCE(excluded.channel_count, tracks.channel_count),
            tags_json = CASE WHEN ?9 IS NULL THEN tracks.tags_json ELSE excluded.tags_json END,
            updated_at = excluded.updated_at",
        params![
            track.file_path,
            track.title,
            track.artist,
            track.duration_ms,
            track.codec,
            track.bitrate_bps,
            track.sample_rate,
            track.channel_count,
            track.tags_json,
            now,
        ],
    )?;
    let track_id: i64 = transaction.query_row(
        "SELECT id FROM tracks WHERE file_path = ?1",
        [&track.file_path],
        |row| row.get(0),
    )?;
    transaction.execute("DELETE FROM waveform_peak_cache WHERE track_id = ?1", [track_id])?;
    if let Some(tags_json) = &track.tags_json {
        replace_audio_tags(&transaction, track_id, tags_json)?;
    }

    if let Some(grid_json) = &track.beat_grid_json {
        transaction.execute(
            "INSERT INTO beat_grids (track_id, bpm, grid_json) VALUES (?1, ?2, ?3)
             ON CONFLICT(track_id) DO UPDATE SET bpm = excluded.bpm, grid_json = excluded.grid_json",
            params![track_id, track.bpm, grid_json],
        )?;
    } else {
        transaction.execute("DELETE FROM beat_grids WHERE track_id = ?1", [track_id])?;
    }

    if let Some(score) = track.sentiment {
        transaction.execute(
            "INSERT INTO sentiment_values (track_id, score) VALUES (?1, ?2)
             ON CONFLICT(track_id) DO UPDATE SET score = excluded.score",
            params![track_id, score],
        )?;
    } else {
        transaction.execute("DELETE FROM sentiment_values WHERE track_id = ?1", [track_id])?;
    }

    transaction.execute("DELETE FROM stems WHERE track_id = ?1", [track_id])?;
    for stem in &track.stems {
        transaction.execute(
            "INSERT INTO stems (track_id, kind, file_path) VALUES (?1, ?2, ?3)",
            params![track_id, stem.kind, stem.file_path],
        )?;
    }
    transaction.commit()?;
    Ok(track_id)
}

pub fn load_stems(connection: &Connection, track_id: i64) -> Result<Vec<StemPath>> {
    let mut statement = connection.prepare(
        "SELECT kind, file_path FROM stems WHERE track_id = ?1 ORDER BY kind",
    )?;
    let rows = statement.query_map([track_id], |row| {
        Ok(StemPath {
            kind: row.get(0)?,
            file_path: row.get(1)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("could not load track stem paths")
}

pub fn load_integrated_lufs(connection: &Connection, track_id: i64) -> Result<Option<f64>> {
    let value = connection
        .query_row(
            "SELECT lufs_integrated FROM track_analysis WHERE track_id = ?1",
            [track_id],
            |row| row.get::<_, Option<f64>>(0),
        )
        .optional()?
        .flatten();
    Ok(value.filter(|lufs| lufs.is_finite() && (-80.0..=10.0).contains(lufs)))
}

pub fn load_beat_grid(connection: &Connection, track_id: i64) -> Result<(Option<f64>, Vec<f64>)> {
    let record = connection
        .query_row(
            "SELECT bpm, grid_json FROM beat_grids WHERE track_id = ?1",
            [track_id],
            |row| Ok((row.get::<_, Option<f64>>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    let Some((bpm, grid_json)) = record else {
        return Ok((None, Vec::new()));
    };
    let beats = serde_json::from_str::<Vec<f64>>(&grid_json)
        .unwrap_or_default()
        .into_iter()
        .filter(|beat| beat.is_finite() && *beat >= 0.0)
        .collect();
    Ok((bpm, beats))
}

pub fn load_waveform_peak_blob(
    connection: &Connection,
    track_id: i64,
    peak_count: u16,
) -> Result<Option<Vec<u8>>> {
    connection
        .query_row(
            "SELECT peaks_blob FROM waveform_peak_cache WHERE track_id = ?1 AND peak_count = ?2",
            params![track_id, i64::from(peak_count)],
            |row| row.get(0),
        )
        .optional()
        .context("could not load cached waveform peaks")
}

pub fn save_waveform_peak_blob(
    connection: &Connection,
    track_id: i64,
    peak_count: u16,
    peaks_blob: &[u8],
) -> Result<()> {
    if !(128..=2048).contains(&peak_count) {
        anyhow::bail!("waveform peak count must be between 128 and 2048");
    }
    let expected_length = usize::from(peak_count) * 2;
    if peaks_blob.len() != expected_length {
        anyhow::bail!("waveform peak BLOB has an invalid length");
    }
    connection.execute(
        "INSERT INTO waveform_peak_cache (track_id, peak_count, peaks_blob, updated_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(track_id, peak_count) DO UPDATE SET
            peaks_blob = excluded.peaks_blob,
            updated_at = excluded.updated_at",
        params![track_id, i64::from(peak_count), peaks_blob, unix_time()],
    )?;
    Ok(())
}

pub fn load_mix_track(connection: &Connection, track_id: i64) -> Result<Option<MixTrackRecord>> {
    let mut track = connection
        .query_row(
            "SELECT t.id, t.file_path, t.title, t.artist, t.duration_ms, b.bpm, b.grid_json,
                    COALESCE(s.score, l.sentiment_score), COALESCE(s.label, l.sentiment_label),
                    a.acoustic_tag, a.energy_density, a.musical_key, r.family_id,
                    ta.lufs_integrated, ta.mood_valence, ta.mood_arousal, ta.mood_label,
                    ta.rhythm_pattern, ta.rhythm_confidence
             FROM tracks AS t
             LEFT JOIN beat_grids AS b ON b.track_id = t.id
             LEFT JOIN sentiment_values AS s ON s.track_id = t.id
             LEFT JOIN lyrics_transcripts AS l ON l.track_id = t.id
             LEFT JOIN audio_analysis AS a ON a.track_id = t.id
             LEFT JOIN riddim_family_tracks AS r ON r.track_id = t.id
             LEFT JOIN track_analysis AS ta ON ta.track_id = t.id
             WHERE t.id = ?1",
            [track_id],
            |row| {
                Ok(MixTrackRecord {
                    id: row.get(0)?,
                    file_path: row.get(1)?,
                    title: row.get(2)?,
                    artist: row.get(3)?,
                    duration_ms: row.get(4)?,
                    bpm: row.get(5)?,
                    beat_grid_json: row.get(6)?,
                    sentiment: row.get(7)?,
                    sentiment_label: row.get(8)?,
                    acoustic_tag: row.get(9)?,
                    energy_density: row.get(10)?,
                    musical_key: row.get(11)?,
                    riddim_family_id: row.get(12)?,
                    integrated_lufs: row.get(13)?,
                    mood_valence: row.get(14)?,
                    mood_arousal: row.get(15)?,
                    mood_label: row.get(16)?,
                    rhythm_pattern: row.get(17)?,
                    rhythm_confidence: row.get(18)?,
                    tags: Vec::new(),
                    stems: Vec::new(),
                    words: Vec::new(),
                })
            },
        )
        .optional()?;
    let Some(track) = track.as_mut() else {
        return Ok(None);
    };

    {
        let mut statement = connection.prepare(
            "SELECT namespace, tag FROM track_tags WHERE track_id = ?1\n             ORDER BY namespace, normalized_tag",
        )?;
        track.tags = statement
            .query_map([track_id], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
    }
    {
        let mut statement = connection.prepare(
            "SELECT kind, file_path FROM stems WHERE track_id = ?1\n             ORDER BY CASE lower(kind)\n                WHEN 'acapella' THEN 0 WHEN 'instrumental' THEN 1\n                WHEN 'bass' THEN 2 WHEN 'drums' THEN 3 ELSE 4 END, kind",
        )?;
        track.stems = statement
            .query_map([track_id], |row| {
                Ok(StemPath {
                    kind: row.get(0)?,
                    file_path: row.get(1)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
    }
    {
        let mut statement = connection.prepare(
            "SELECT word, start_ms, end_ms, confidence FROM lyric_words\n             WHERE track_id = ?1 ORDER BY word_index",
        )?;
        track.words = statement
            .query_map([track_id], |row| {
                Ok(LyricWord {
                    text: row.get(0)?,
                    start_ms: row.get(1)?,
                    end_ms: row.get(2)?,
                    confidence: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
    }
    Ok(Some(track.clone()))
}

/// Return semantic lyric matches first, then a bounded local-stem catalog ordered
/// by automatic/manual riddim family and half-/normal-/double-time BPM proximity.
/// No paths leave SQLite.
pub fn mix_candidate_ids(
    connection: &Connection,
    source_track_id: i64,
    riddim_family_id: Option<i64>,
    reference_bpm: Option<f64>,
    semantic_tokens: &[String],
    limit: usize,
) -> Result<Vec<i64>> {
    use rusqlite::types::Value;

    let limit = limit.clamp(1, 256) as i64;
    let mut ids = Vec::with_capacity(limit as usize);
    let mut seen = std::collections::HashSet::new();
    let tokens = semantic_tokens
        .iter()
        .map(|token| token.trim().to_lowercase())
        .filter(|token| !token.is_empty() && token.len() <= 64)
        .collect::<Vec<_>>();

    if !tokens.is_empty() {
        let placeholders = (0..tokens.len())
            .map(|index| format!("?{}", index + 3))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT w.track_id, COUNT(DISTINCT lower(trim(w.word, ' .,!?;:'))) AS matched_terms\n\
             FROM lyric_words AS w\n\
             WHERE w.track_id != ?1\n\
               AND w.confidence >= 0.20\n\
               AND lower(trim(w.word, ' .,!?;:')) IN ({placeholders})\n\
               AND (SELECT COUNT(DISTINCT lower(s.kind)) FROM stems AS s\n\
                    WHERE s.track_id = w.track_id AND lower(s.kind) IN\n\
                    ('acapella', 'instrumental', 'bass', 'drums')) = 4\n\
             GROUP BY w.track_id\n\
             ORDER BY matched_terms DESC, w.track_id\n\
             LIMIT ?2"
        );
        let mut values = vec![Value::Integer(source_track_id), Value::Integer(limit)];
        values.extend(tokens.into_iter().map(Value::Text));
        let mut statement = connection.prepare(&sql)?;
        let rows = statement.query_map(rusqlite::params_from_iter(values.iter()), |row| {
            row.get::<_, i64>(0)
        })?;
        for id in rows.collect::<rusqlite::Result<Vec<_>>>()? {
            if seen.insert(id) {
                ids.push(id);
            }
        }
    }

    let remaining = limit.saturating_sub(ids.len() as i64);
    if remaining > 0 {
        let mut statement = connection.prepare(
            r#"SELECT t.id FROM tracks AS t
               LEFT JOIN riddim_family_tracks AS r ON r.track_id = t.id
               LEFT JOIN beat_grids AS b ON b.track_id = t.id
               WHERE t.id != ?1
                 AND (SELECT COUNT(DISTINCT lower(s.kind)) FROM stems AS s
                      WHERE s.track_id = t.id AND lower(s.kind) IN
                      ('acapella', 'instrumental', 'bass', 'drums')) = 4
               ORDER BY CASE
                          WHEN ?2 IS NOT NULL AND r.family_id = ?2 THEN 0
                          WHEN EXISTS (
                              SELECT 1 FROM track_tags AS candidate_tag
                              JOIN track_tags AS source_tag
                                ON source_tag.track_id = ?1
                               AND source_tag.namespace = 'riddim'
                               AND candidate_tag.normalized_tag = source_tag.normalized_tag
                              WHERE candidate_tag.track_id = t.id
                                AND candidate_tag.namespace = 'riddim'
                          ) THEN 1
                          ELSE 2
                        END,
                        CASE WHEN ?3 IS NULL OR b.bpm IS NULL THEN 1000.0
                             ELSE MIN(ABS(b.bpm * 0.5 - ?3), ABS(b.bpm - ?3), ABS(b.bpm * 2.0 - ?3)) END,
                        t.updated_at DESC, t.id
               LIMIT ?4"#
        )?;
        let rows = statement.query_map(
            rusqlite::params![source_track_id, riddim_family_id, reference_bpm, remaining],
            |row| row.get::<_, i64>(0),
        )?;
        for id in rows.collect::<rusqlite::Result<Vec<_>>>()? {
            if seen.insert(id) {
                ids.push(id);
            }
        }
    }
    Ok(ids)
}

pub fn search_tracks(connection: &Connection, query: &str, limit: u32) -> Result<Vec<TrackSummary>> {
    search_tracks_page(connection, query, limit, 0).map(|(tracks, _)| tracks)
}

pub fn search_tracks_page(
    connection: &Connection,
    query: &str,
    limit: u32,
    offset: u32,
) -> Result<(Vec<TrackSummary>, u64)> {
    search_tracks_page_with_filters(
        connection,
        query,
        &TrackSearchFilters::default(),
        limit,
        offset,
    )
}

const TRACK_SEARCH_FILTERS_SQL: &str = r#"
    (?1 = '' OR t.title LIKE ?2 COLLATE NOCASE OR t.artist LIKE ?2 COLLATE NOCASE
        OR a.musical_key LIKE ?2 COLLATE NOCASE OR a.acoustic_tag LIKE ?2 COLLATE NOCASE
        OR ta.mood_label LIKE ?2 COLLATE NOCASE OR ta.rhythm_pattern LIKE ?2 COLLATE NOCASE
        OR ta.style_genre LIKE ?2 COLLATE NOCASE
        OR EXISTS (
            SELECT 1 FROM audio_tags AS mt WHERE mt.track_id = t.id
              AND (mt.tag_key LIKE ?2 COLLATE NOCASE OR mt.tag_value LIKE ?2 COLLATE NOCASE)
        ))
    AND (
        ?3 IS NULL
        OR (LOWER(?3) = 'sad' AND (ta.mood_valence <= -0.22 OR LOWER(ta.mood_label) IN ('melancholic', 'tense')))
        OR (LOWER(?3) = 'positive' AND (ta.mood_valence >= 0.22 OR LOWER(ta.mood_label) IN ('uplifting', 'euphoric')))
        OR (LOWER(?3) = 'calm' AND LOWER(ta.mood_label) IN ('calm', 'chill'))
        OR (LOWER(?3) = 'energetic' AND (ta.mood_arousal >= 0.62 OR LOWER(ta.mood_label) IN ('high energy', 'euphoric', 'aggressive')))
        OR (LOWER(?3) = 'aggressive' AND (LOWER(ta.mood_label) = 'aggressive' OR (ta.mood_valence <= -0.28 AND ta.mood_arousal >= 0.62)))
        OR (LOWER(?3) = 'tense' AND (LOWER(ta.mood_label) = 'tense' OR (ta.mood_valence <= -0.32 AND ta.mood_arousal > 0.38)))
        OR (LOWER(?3) = 'uplifting' AND (LOWER(ta.mood_label) IN ('uplifting', 'euphoric') OR ta.mood_valence >= 0.35))
        OR (LOWER(?3) = 'euphoric' AND LOWER(ta.mood_label) = 'euphoric')
        OR (LOWER(?3) = 'balanced' AND LOWER(ta.mood_label) = 'balanced')
        OR (LOWER(?3) NOT IN ('sad', 'positive', 'calm', 'energetic', 'aggressive', 'tense', 'uplifting', 'euphoric', 'balanced')
            AND ta.mood_label LIKE '%' || ?3 || '%' COLLATE NOCASE)
    )
    AND (?4 IS NULL OR ta.rhythm_pattern LIKE '%' || ?4 || '%' COLLATE NOCASE OR ta.style_genre LIKE '%' || ?4 || '%' COLLATE NOCASE)
    AND (?5 IS NULL OR a.musical_key LIKE '%' || ?5 || '%' COLLATE NOCASE)
    AND (?6 IS NULL OR b.bpm >= ?6)
    AND (?7 IS NULL OR b.bpm <= ?7)
    AND (?8 IS NULL OR ta.lufs_integrated >= ?8)
    AND (?9 IS NULL OR ta.lufs_integrated <= ?9)
"#;

/// Search one bounded page with structured mood, rhythm, key, tempo, and loudness filters.
pub fn search_tracks_page_with_filters(
    connection: &Connection,
    query: &str,
    filters: &TrackSearchFilters,
    limit: u32,
    offset: u32,
) -> Result<(Vec<TrackSummary>, u64)> {
    // Prefix matching keeps title/artist search selective; structured fields use their indexed metadata.
    // Paging keeps both the IPC payload and JS heap bounded for 50k-track libraries.
    let query = query.trim();
    let pattern = format!("{query}%");
    let count_sql = format!(
        "SELECT COUNT(*)
         FROM tracks AS t
         LEFT JOIN audio_analysis AS a ON a.track_id = t.id
         LEFT JOIN track_analysis AS ta ON ta.track_id = t.id
         LEFT JOIN beat_grids AS b ON b.track_id = t.id
         WHERE {TRACK_SEARCH_FILTERS_SQL}"
    );
    let total: i64 = connection.query_row(
        &count_sql,
        params![
            query,
            pattern,
            filters.mood.as_deref(),
            filters.rhythm.as_deref(),
            filters.key.as_deref(),
            filters.bpm_min,
            filters.bpm_max,
            filters.lufs_min,
            filters.lufs_max,
        ],
        |row| row.get(0),
    )?;
    let page_sql = format!(
        "SELECT t.id, t.file_path, t.title, t.artist, t.duration_ms, b.bpm, s.score,
                a.musical_key, a.acoustic_tag, t.codec, t.bitrate_bps, t.sample_rate,
                t.channel_count, t.tags_json, ta.lufs_integrated, ta.true_peak_dbtp,
                ta.gain_offset_db, ta.mood_valence, ta.mood_arousal, ta.mood_label,
                ta.rhythm_pattern, ta.style_genre
         FROM tracks AS t
         LEFT JOIN beat_grids AS b ON b.track_id = t.id
         LEFT JOIN sentiment_values AS s ON s.track_id = t.id
         LEFT JOIN audio_analysis AS a ON a.track_id = t.id
         LEFT JOIN track_analysis AS ta ON ta.track_id = t.id
         WHERE {TRACK_SEARCH_FILTERS_SQL}
         ORDER BY t.updated_at DESC, t.title COLLATE NOCASE
         LIMIT ?10 OFFSET ?11"
    );
    let mut statement = connection.prepare(&page_sql)?;
    let rows = statement.query_map(
        params![
            query,
            pattern,
            filters.mood.as_deref(),
            filters.rhythm.as_deref(),
            filters.key.as_deref(),
            filters.bpm_min,
            filters.bpm_max,
            filters.lufs_min,
            filters.lufs_max,
            limit,
            offset,
        ],
        |row| {
            Ok(TrackSummary {
                id: row.get(0)?,
                file_path: row.get(1)?,
                title: row.get(2)?,
                artist: row.get(3)?,
                duration_ms: row.get(4)?,
                bpm: row.get(5)?,
                sentiment: row.get(6)?,
                musical_key: row.get(7)?,
                acoustic_tag: row.get(8)?,
                codec: row.get(9)?,
                bitrate_bps: row.get(10)?,
                sample_rate: row.get(11)?,
                channel_count: row.get(12)?,
                tags_json: row.get(13)?,
                lufs_integrated: row.get(14)?,
                true_peak_dbtp: row.get(15)?,
                gain_offset_db: row.get(16)?,
                mood_valence: row.get(17)?,
                mood_arousal: row.get(18)?,
                mood_label: row.get(19)?,
                rhythm_pattern: row.get(20)?,
                style_genre: row.get(21)?,
            })
        },
    )?;
    let tracks = rows
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("could not read track search results")?;
    Ok((tracks, total.max(0) as u64))
}

pub fn create_import_job(
    connection: &mut Connection,
    paths: &[String],
    separation_model_path: &str,
    whisper_model_path: &str,
    use_directml: bool,
    cpu_threads: usize,
) -> Result<i64> {
    anyhow::ensure!(!paths.is_empty(), "an import job must contain at least one path");
    anyhow::ensure!(paths.len() <= 50_000, "an import batch is limited to 50,000 paths");
    anyhow::ensure!((1..=8).contains(&cpu_threads), "CPU threads must be between 1 and 8");

    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let now = unix_time();
    transaction.execute(
        "INSERT INTO import_jobs (
            status, total_items, separation_model_path, whisper_model_path,
            use_directml, cpu_threads, created_at, updated_at
         ) VALUES ('queued', 0, ?1, ?2, ?3, ?4, ?5, ?5)",
        params![
            separation_model_path,
            whisper_model_path,
            use_directml,
            cpu_threads as i64,
            now,
        ],
    )?;
    let job_id = transaction.last_insert_rowid();
    for path in paths {
        transaction.execute(
            "INSERT OR IGNORE INTO import_items (job_id, source_path, status, created_at)
             VALUES (?1, ?2, 'queued', ?3)",
            params![job_id, path, now],
        )?;
    }
    transaction.commit()?;
    Ok(job_id)
}

pub fn recover_import_jobs(connection: &mut Connection) -> Result<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let now = unix_time();
    transaction.execute(
        "UPDATE import_items SET status = 'cancelled', finished_at = ?1
         WHERE status IN ('queued', 'running') AND job_id IN (
             SELECT id FROM import_jobs WHERE status IN ('queued', 'running') AND cancel_requested = 1
         )",
        [now],
    )?;
    transaction.execute(
        "UPDATE import_jobs SET status = 'cancelled', current_path = NULL, current_stage = NULL,
             finished_at = ?1, updated_at = ?1
         WHERE status IN ('queued', 'running') AND cancel_requested = 1",
        [now],
    )?;
    transaction.execute(
        "UPDATE import_items SET status = 'queued', started_at = NULL
         WHERE status = 'running' AND job_id IN (
             SELECT id FROM import_jobs WHERE status = 'running' AND cancel_requested = 0
         )",
        [],
    )?;
    transaction.execute(
        "UPDATE import_jobs SET status = 'queued', current_path = NULL, current_stage = NULL,
             current_progress = 0.0, updated_at = ?1
         WHERE status = 'running' AND cancel_requested = 0",
        [now],
    )?;
    transaction.commit()?;
    Ok(())
}

pub fn claim_next_import_job(connection: &mut Connection) -> Result<Option<i64>> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let job_id = transaction
        .query_row(
            "SELECT id FROM import_jobs WHERE status = 'queued' ORDER BY id LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(job_id) = job_id {
        transaction.execute(
            "UPDATE import_jobs SET status = 'running', started_at = ?1, updated_at = ?1,
                 current_progress = 0.0, error = NULL
             WHERE id = ?2 AND status = 'queued'",
            params![unix_time(), job_id],
        )?;
    }
    transaction.commit()?;
    Ok(job_id)
}

pub fn import_job_config(connection: &Connection, job_id: i64) -> Result<ImportJobConfig> {
    connection
        .query_row(
            "SELECT id, separation_model_path, whisper_model_path, use_directml, cpu_threads
             FROM import_jobs WHERE id = ?1",
            [job_id],
            |row| {
                Ok(ImportJobConfig {
                    id: row.get(0)?,
                    separation_model_path: row.get(1)?,
                    whisper_model_path: row.get(2)?,
                    use_directml: row.get(3)?,
                    cpu_threads: row.get::<_, i64>(4)?.clamp(1, 8) as usize,
                })
            },
        )
        .context("could not load import model settings")
}

pub fn claim_next_import_item(connection: &mut Connection, job_id: i64) -> Result<Option<ImportItem>> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let item = transaction
        .query_row(
            "SELECT id, job_id, source_path FROM import_items
             WHERE job_id = ?1 AND status = 'queued' ORDER BY id LIMIT 1",
            [job_id],
            |row| {
                Ok(ImportItem {
                    id: row.get(0)?,
                    job_id: row.get(1)?,
                    source_path: row.get(2)?,
                })
            },
        )
        .optional()?;
    if let Some(item) = &item {
        transaction.execute(
            "UPDATE import_items SET status = 'running', started_at = ?1, error = NULL WHERE id = ?2",
            params![unix_time(), item.id],
        )?;
        transaction.execute(
            "UPDATE import_jobs SET current_path = ?1, current_stage = 'starting', updated_at = ?2 WHERE id = ?3",
            params![item.source_path, unix_time(), job_id],
        )?;
    }
    transaction.commit()?;
    Ok(item)
}

pub fn set_import_stage(connection: &Connection, job_id: i64, stage: &str) -> Result<()> {
    connection.execute(
        "UPDATE import_jobs SET current_stage = ?1, current_progress = 0.0, updated_at = ?2
         WHERE id = ?3 AND status = 'running'",
        params![stage, unix_time(), job_id],
    )?;
    Ok(())
}

pub fn set_import_progress(
    connection: &Connection,
    job_id: i64,
    stage: &str,
    progress: f64,
) -> Result<()> {
    anyhow::ensure!(progress.is_finite(), "import progress must be finite");
    connection.execute(
        "UPDATE import_jobs SET current_stage = ?1, current_progress = ?2, updated_at = ?3
         WHERE id = ?4 AND status = 'running'",
        params![stage, progress.clamp(0.0, 1.0), unix_time(), job_id],
    )?;
    Ok(())
}

pub fn import_cancel_requested(connection: &Connection, job_id: i64) -> Result<bool> {
    connection
        .query_row(
            "SELECT cancel_requested != 0 FROM import_jobs WHERE id = ?1",
            [job_id],
            |row| row.get(0),
        )
        .optional()?
        .context("import job no longer exists")
}

pub fn request_import_cancel(connection: &mut Connection, job_id: i64) -> Result<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let status = transaction
        .query_row("SELECT status FROM import_jobs WHERE id = ?1", [job_id], |row| {
            row.get::<_, String>(0)
        })
        .optional()?
        .context("import job does not exist")?;
    anyhow::ensure!(
        status == "queued" || status == "running",
        "import job is not active"
    );
    let now = unix_time();
    if status == "queued" {
        transaction.execute(
            "UPDATE import_items SET status = 'cancelled', finished_at = ?1
             WHERE job_id = ?2 AND status = 'queued'",
            params![now, job_id],
        )?;
        transaction.execute(
            "UPDATE import_jobs SET status = 'cancelled', cancel_requested = 1, current_path = NULL,
                 current_stage = NULL, current_progress = 0.0, finished_at = ?1, updated_at = ?1
             WHERE id = ?2",
            params![now, job_id],
        )?;
    } else {
        transaction.execute(
            "UPDATE import_jobs SET cancel_requested = 1, updated_at = ?1 WHERE id = ?2",
            params![now, job_id],
        )?;
    }
    transaction.commit()?;
    Ok(())
}

pub fn finish_import_item_failed(
    connection: &Connection,
    item_id: i64,
    error: &str,
) -> Result<()> {
    connection.execute(
        "UPDATE import_items SET status = 'failed', error = ?1, finished_at = ?2 WHERE id = ?3",
        params![error, unix_time(), item_id],
    )?;
    Ok(())
}

pub fn finish_import_item_cancelled(connection: &Connection, item_id: i64) -> Result<()> {
    connection.execute(
        "UPDATE import_items SET status = 'cancelled', finished_at = ?1 WHERE id = ?2",
        params![unix_time(), item_id],
    )?;
    Ok(())
}

pub fn cancel_queued_import_items(connection: &Connection, job_id: i64) -> Result<()> {
    connection.execute(
        "UPDATE import_items SET status = 'cancelled', finished_at = ?1
         WHERE job_id = ?2 AND status = 'queued'",
        params![unix_time(), job_id],
    )?;
    Ok(())
}

pub fn finish_import_job(connection: &mut Connection, job_id: i64, cancelled: bool) -> Result<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let cancel_requested: bool = transaction.query_row(
        "SELECT cancel_requested != 0 FROM import_jobs WHERE id = ?1",
        [job_id],
        |row| row.get(0),
    )?;
    let cancelled = cancelled || cancel_requested;
    let now = unix_time();
    if cancelled {
        transaction.execute(
            "UPDATE import_items SET status = 'cancelled', finished_at = ?1
             WHERE job_id = ?2 AND status = 'queued'",
            params![now, job_id],
        )?;
    }
    transaction.execute(
        "UPDATE import_jobs SET status = ?1, current_path = NULL, current_stage = NULL,
             current_progress = ?2, finished_at = ?3, updated_at = ?3 WHERE id = ?4",
        params![
            if cancelled { "cancelled" } else { "completed" },
            if cancelled { 0.0 } else { 1.0 },
            now,
            job_id,
        ],
    )?;
    transaction.commit()?;
    Ok(())
}

pub fn fail_import_job(connection: &Connection, job_id: i64, error: &str) -> Result<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let now = unix_time();
    transaction.execute(
        "UPDATE import_items SET status = 'failed', error = ?1, finished_at = ?2
         WHERE job_id = ?3 AND status IN ('queued', 'running')",
        params![error, now, job_id],
    )?;
    transaction.execute(
        "UPDATE import_jobs SET status = 'failed', error = ?1, current_path = NULL, current_stage = NULL,
             finished_at = ?2, updated_at = ?2 WHERE id = ?3",
        params![error, now, job_id],
    )?;
    transaction.commit()?;
    Ok(())
}

pub fn import_jobs(connection: &Connection, limit: u32) -> Result<Vec<ImportJobSnapshot>> {
    let mut statement = connection.prepare(
        "SELECT id, status, total_items, queued_items, running_items, successful_items,
                failed_items, cancelled_items, current_path, current_stage, current_progress,
                error, created_at, updated_at
         FROM import_jobs
         ORDER BY id DESC LIMIT ?1",
    )?;
    let rows = statement.query_map([limit], |row| {
        let status = row.get::<_, String>(1)?;
        let current_stage = row.get::<_, Option<String>>(9)?;
        Ok(ImportJobSnapshot {
            id: row.get(0)?,
            status_message: localization::import_status(&status),
            status,
            total_items: row.get(2)?,
            queued_items: row.get::<_, Option<i64>>(3)?.unwrap_or(0),
            running_items: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
            successful_items: row.get::<_, Option<i64>>(5)?.unwrap_or(0),
            failed_items: row.get::<_, Option<i64>>(6)?.unwrap_or(0),
            cancelled_items: row.get::<_, Option<i64>>(7)?.unwrap_or(0),
            current_path: row.get(8)?,
            current_stage_message: current_stage.as_deref().map(localization::import_stage),
            current_stage,
            current_progress: row.get(10)?,
            error: row.get(11)?,
            created_at: row.get(12)?,
            updated_at: row.get(13)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("could not read import job status")
}

pub fn commit_import_result(
    connection: &mut Connection,
    job_id: i64,
    item_id: i64,
    result: &ImportResult,
) -> Result<Vec<String>> {
    anyhow::ensure!(result.vector_blob.len() == 64, "MIR descriptor BLOB must contain 64 int8 values");
    anyhow::ensure!(
        result.mood_valence.is_finite()
            && (-1.0..=1.0).contains(&result.mood_valence)
            && result.mood_arousal.is_finite()
            && (0.0..=1.0).contains(&result.mood_arousal)
            && result.rhythm_confidence.is_finite()
            && (0.0..=1.0).contains(&result.rhythm_confidence),
        "MIR mood and rhythm metrics are outside their supported ranges"
    );
    anyhow::ensure!(
        [result.lufs_integrated, result.true_peak_dbtp, result.gain_offset_db]
            .into_iter()
            .flatten()
            .all(|value| value.is_finite()),
        "loudness analysis contains a non-finite value"
    );
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let item_is_active: bool = transaction.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM import_jobs AS jobs
            JOIN import_items AS items ON items.job_id = jobs.id
            WHERE jobs.id = ?1 AND items.id = ?2
              AND jobs.status = 'running' AND jobs.cancel_requested = 0
              AND items.status = 'running'
         )",
        params![job_id, item_id],
        |row| row.get(0),
    )?;
    anyhow::ensure!(item_is_active, "import was cancelled or item is no longer active");
    let now = unix_time();
    transaction.execute(
        "INSERT INTO tracks (
            file_path, title, artist, duration_ms, codec, bitrate_bps, sample_rate,
            channel_count, tags_json, created_at, updated_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)
         ON CONFLICT(file_path) DO UPDATE SET
            title = excluded.title, artist = excluded.artist,
            duration_ms = excluded.duration_ms, codec = excluded.codec,
            bitrate_bps = excluded.bitrate_bps, sample_rate = excluded.sample_rate,
            channel_count = excluded.channel_count, tags_json = excluded.tags_json,
            updated_at = excluded.updated_at",
        params![
            result.source_path,
            result.title,
            result.artist,
            result.duration_ms,
            result.codec,
            result.source_bitrate_bps,
            result.source_sample_rate,
            result.source_channels,
            result.tags_json,
            now,
        ],
    )?;
    let track_id: i64 = transaction.query_row(
        "SELECT id FROM tracks WHERE file_path = ?1",
        [&result.source_path],
        |row| row.get(0),
    )?;

    replace_audio_tags(&transaction, track_id, &result.tags_json)?;

    let old_paths = {
        let mut statement = transaction.prepare("SELECT file_path FROM stems WHERE track_id = ?1")?;
        let rows = statement.query_map([track_id], |row| row.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    transaction.execute(
        "INSERT INTO beat_grids (track_id, bpm, grid_json) VALUES (?1, ?2, ?3)
         ON CONFLICT(track_id) DO UPDATE SET bpm = excluded.bpm, grid_json = excluded.grid_json",
        params![track_id, result.bpm, result.beat_grid_json],
    )?;
    transaction.execute(
        "INSERT INTO audio_analysis (
            track_id, musical_key, key_confidence, bpm_confidence, source_sample_rate, source_channels,
            energy_density, energy_dbfs, dynamic_range_db, onset_density, spectral_centroid_hz,
            low_band_ratio, mid_band_ratio, high_band_ratio, acoustic_tag, acoustic_scores_json,
            analyzer_version, analyzed_at
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, 'championvibe-dsp-1', ?17
         )
         ON CONFLICT(track_id) DO UPDATE SET
            musical_key = excluded.musical_key, key_confidence = excluded.key_confidence,
            bpm_confidence = excluded.bpm_confidence, source_sample_rate = excluded.source_sample_rate,
            source_channels = excluded.source_channels, energy_density = excluded.energy_density,
            energy_dbfs = excluded.energy_dbfs, dynamic_range_db = excluded.dynamic_range_db,
            onset_density = excluded.onset_density, spectral_centroid_hz = excluded.spectral_centroid_hz,
            low_band_ratio = excluded.low_band_ratio, mid_band_ratio = excluded.mid_band_ratio,
            high_band_ratio = excluded.high_band_ratio, acoustic_tag = excluded.acoustic_tag,
            acoustic_scores_json = excluded.acoustic_scores_json,
            analyzer_version = excluded.analyzer_version, analyzed_at = excluded.analyzed_at",
        params![
            track_id,
            result.musical_key,
            result.key_confidence,
            result.bpm_confidence,
            result.source_sample_rate,
            result.source_channels,
            result.energy_density,
            result.energy_dbfs,
            result.dynamic_range_db,
            result.onset_density,
            result.spectral_centroid_hz,
            result.low_band_ratio,
            result.mid_band_ratio,
            result.high_band_ratio,
            result.acoustic_tag,
            result.acoustic_scores_json,
            now,
        ],
    )?;
    transaction.execute(
        "INSERT INTO track_analysis (
            track_id, lufs_integrated, true_peak_dbtp, gain_offset_db,
            mood_valence, mood_arousal, mood_label, rhythm_pattern, rhythm_confidence,
            style_genre, vector_blob, vector_kind, analyzer_version, analyzed_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                   'championvibe-lufs-mir-v1', ?13)
         ON CONFLICT(track_id) DO UPDATE SET
            lufs_integrated = excluded.lufs_integrated,
            true_peak_dbtp = excluded.true_peak_dbtp,
            gain_offset_db = excluded.gain_offset_db,
            mood_valence = excluded.mood_valence,
            mood_arousal = excluded.mood_arousal,
            mood_label = excluded.mood_label,
            rhythm_pattern = excluded.rhythm_pattern,
            rhythm_confidence = excluded.rhythm_confidence,
            style_genre = excluded.style_genre,
            vector_blob = excluded.vector_blob,
            vector_kind = excluded.vector_kind,
            analyzer_version = excluded.analyzer_version,
            analyzed_at = excluded.analyzed_at",
        params![
            track_id,
            result.lufs_integrated,
            result.true_peak_dbtp,
            result.gain_offset_db,
            result.mood_valence,
            result.mood_arousal,
            result.mood_label,
            result.rhythm_pattern,
            result.rhythm_confidence,
            result.style_genre,
            result.vector_blob,
            result.vector_kind,
            now,
        ],
    )?;

    transaction.execute(
        "INSERT INTO lyrics_transcripts (
            track_id, model, language, transcript, sentiment_score, sentiment_label, sentiment_confidence, created_at
         ) VALUES (?1, 'whisper-tiny', ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(track_id) DO UPDATE SET
            model = excluded.model, language = excluded.language, transcript = excluded.transcript,
            sentiment_score = excluded.sentiment_score, sentiment_label = excluded.sentiment_label,
            sentiment_confidence = excluded.sentiment_confidence, created_at = excluded.created_at",
        params![
            track_id,
            result.transcript_language,
            result.transcript,
            result.lyric_sentiment,
            result.lyric_sentiment_label,
            result.lyric_sentiment_confidence,
            now,
        ],
    )?;
    transaction.execute("DELETE FROM lyric_words WHERE track_id = ?1", [track_id])?;
    {
        let mut insert_word = transaction.prepare(
            "INSERT INTO lyric_words (track_id, word_index, word, start_ms, end_ms, confidence)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for (index, word) in result.words.iter().enumerate() {
            insert_word.execute(params![
                track_id,
                index as i64,
                word.text,
                word.start_ms,
                word.end_ms.max(word.start_ms),
                word.confidence.clamp(0.0, 1.0),
            ])?;
        }
    }

    transaction.execute(
        "INSERT INTO sentiment_values (track_id, score, label) VALUES (?1, ?2, ?3)
         ON CONFLICT(track_id) DO UPDATE SET score = excluded.score, label = excluded.label",
        params![track_id, result.lyric_sentiment, result.lyric_sentiment_label],
    )?;
    transaction.execute("DELETE FROM stems WHERE track_id = ?1", [track_id])?;
    for stem in &result.stems {
        transaction.execute(
            "INSERT INTO stems (track_id, kind, file_path) VALUES (?1, ?2, ?3)",
            params![track_id, stem.kind, stem.file_path],
        )?;
    }
    transaction.execute(
        "UPDATE import_items SET status = 'succeeded', track_id = ?1, error = NULL, finished_at = ?2 WHERE id = ?3",
        params![track_id, now, item_id],
    )?;
    transaction.execute(
        "UPDATE import_jobs SET current_stage = 'committing', updated_at = ?1 WHERE id = ?2",
        params![now, job_id],
    )?;
    transaction.commit()?;
    Ok(old_paths)
}

pub fn save_recording_metadata(
    connection: &Connection,
    name: &str,
    file_path: &str,
    duration_ms: Option<i64>,
    metadata: &RecordingMetadata,
) -> Result<i64> {
    let now = unix_time();
    let file_size_bytes = metadata
        .file_size_bytes
        .map(i64::try_from)
        .transpose()
        .context("recording file size does not fit SQLite's integer range")?;
    connection.execute(
        "INSERT INTO recordings (
            name, file_path, duration_ms, format, sample_rate, channel_count,
            bit_depth, bitrate_bps, file_size_bytes, status, created_at, finished_at
         ) VALUES (?1, ?2, ?3, COALESCE(?4, 'unknown'), ?5, ?6, ?7, ?8, ?9, 'complete', ?10, ?10)",
        params![
            name,
            file_path,
            duration_ms,
            metadata.format,
            metadata.sample_rate,
            metadata.channel_count,
            metadata.bit_depth,
            metadata.bitrate_bps,
            file_size_bytes,
            now,
        ],
    )?;
    Ok(connection.last_insert_rowid())
}

pub fn store_license(connection: &Connection, signed_json: &str) -> Result<()> {
    connection.execute(
        "INSERT INTO licenses (id, signed_json, saved_at) VALUES (1, ?1, ?2)
         ON CONFLICT(id) DO UPDATE SET signed_json = excluded.signed_json, saved_at = excluded.saved_at",
        params![signed_json, unix_time()],
    )?;
    Ok(())
}

pub fn load_license(connection: &Connection) -> Result<Option<String>> {
    connection
        .query_row("SELECT signed_json FROM licenses WHERE id = 1", [], |row| row.get(0))
        .optional()
        .context("could not load persisted license")
}

fn unix_time() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::{
        cancel_queued_import_items, claim_next_import_item, claim_next_import_job,
        commit_import_result, create_import_job, ensure_recording_columns,
        finish_import_item_failed, finish_import_job, import_jobs, open, load_mix_track,
        load_waveform_peak_blob, mix_candidate_ids, save_recording_metadata,
        save_waveform_peak_blob, search_tracks, search_tracks_page, search_tracks_page_with_filters,
        upsert_track, ImportResult, RecordingMetadata, TrackInput, TrackSearchFilters,
    };
    use std::path::Path;

    #[test]
    fn sqlite_schema_and_track_upsert_work_for_library_records() {
        let connection = open(Path::new(":memory:")).unwrap();
        let mut connection = connection;
        let track = TrackInput {
            file_path: "C:/Music/test.wav".to_owned(),
            title: "Test track".to_owned(),
            artist: "Champion".to_owned(),
            duration_ms: Some(180_000),
            codec: Some("flac".to_owned()),
            bitrate_bps: Some(900_000),
            sample_rate: Some(44_100),
            channel_count: Some(2),
            tags_json: Some(r#"{"genre":["Dancehall"]}"#.to_owned()),
            bpm: Some(128.0),
            beat_grid_json: Some("[0.0,0.46875]".to_owned()),
            sentiment: Some(0.3),
            stems: vec![],
        };
        let track_id = upsert_track(&mut connection, &track).unwrap();
        let found = search_tracks(&connection, "test", 20).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].title, "Test track");
        assert_eq!(found[0].bpm, Some(128.0));
        assert_eq!(found[0].sentiment, Some(0.3));
        assert_eq!(found[0].codec.as_deref(), Some("flac"));
        assert_eq!(found[0].bitrate_bps, Some(900_000));
        assert_eq!(found[0].sample_rate, Some(44_100));
        assert_eq!(found[0].channel_count, Some(2));
        assert!(found[0].tags_json.contains("Dancehall"));
        assert_eq!(search_tracks(&connection, "Dancehall", 20).unwrap().len(), 1);

        let peak_blob = vec![0x5a; 128 * 2];
        save_waveform_peak_blob(&connection, track_id, 128, &peak_blob).unwrap();
        assert_eq!(load_waveform_peak_blob(&connection, track_id, 128).unwrap(), Some(peak_blob));
        assert!(save_waveform_peak_blob(&connection, track_id, 128, &[1, 2]).is_err());
        upsert_track(&mut connection, &track).unwrap();
        assert_eq!(load_waveform_peak_blob(&connection, track_id, 128).unwrap(), None);
    }

    #[test]
    fn import_commit_persists_loudness_mir_vector_and_searchable_labels() {
        let mut connection = open(Path::new(":memory:")).unwrap();
        let source_path = "C:/Music/mir-track.wav".to_owned();
        let job_id = create_import_job(
            &mut connection,
            std::slice::from_ref(&source_path),
            "separator.onnx",
            "whisper.bin",
            false,
            2,
        )
        .unwrap();
        assert_eq!(claim_next_import_job(&mut connection).unwrap(), Some(job_id));
        let item = claim_next_import_item(&mut connection, job_id)
            .unwrap()
            .unwrap();
        let result = ImportResult {
            source_path,
            title: "Mood Track".to_owned(),
            artist: "Local Artist".to_owned(),
            duration_ms: 120_000,
            source_sample_rate: 44_100,
            source_channels: 2,
            source_bitrate_bps: Some(900_000),
            codec: "flac".to_owned(),
            tags_json: "{}".to_owned(),
            bpm: Some(120.0),
            bpm_confidence: 0.9,
            beat_grid_json: "[0.0,0.5]".to_owned(),
            musical_key: Some("C major".to_owned()),
            key_confidence: 0.8,
            energy_density: 0.1,
            energy_dbfs: -10.0,
            dynamic_range_db: 8.0,
            onset_density: 2.0,
            spectral_centroid_hz: 2_000.0,
            low_band_ratio: 0.4,
            mid_band_ratio: 0.4,
            high_band_ratio: 0.2,
            acoustic_tag: "Hype".to_owned(),
            acoustic_scores_json: "{}".to_owned(),
            lufs_integrated: Some(-14.5),
            true_peak_dbtp: Some(-0.4),
            gain_offset_db: Some(0.5),
            mood_valence: 0.62,
            mood_arousal: 0.71,
            mood_label: "Euphoric".to_owned(),
            rhythm_pattern: "One Drop".to_owned(),
            rhythm_confidence: 0.88,
            style_genre: "Roots / Reggae (heuristic)".to_owned(),
            vector_blob: vec![0x7f; 64],
            vector_kind: "mir-dsp-int8-v1".to_owned(),
            transcript: "local words".to_owned(),
            transcript_language: Some("en".to_owned()),
            lyric_sentiment: 0.2,
            lyric_sentiment_label: "positive".to_owned(),
            lyric_sentiment_confidence: 0.7,
            words: vec![],
            stems: vec![],
        };
        commit_import_result(&mut connection, job_id, item.id, &result).unwrap();

        let found = search_tracks(&connection, "One Drop", 20).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].mood_label.as_deref(), Some("Euphoric"));
        assert_eq!(found[0].rhythm_pattern.as_deref(), Some("One Drop"));
        assert_eq!(found[0].style_genre.as_deref(), Some("Roots / Reggae (heuristic)"));
        assert_eq!(found[0].lufs_integrated, Some(-14.5));
        assert_eq!(found[0].true_peak_dbtp, Some(-0.4));
        assert_eq!(found[0].gain_offset_db, Some(0.5));
        let (semantic_matches, semantic_total) = search_tracks_page_with_filters(
            &connection,
            "",
            &TrackSearchFilters {
                mood: Some("positive".to_owned()),
                rhythm: Some("One Drop".to_owned()),
                key: Some("C major".to_owned()),
                bpm_min: Some(117.0),
                bpm_max: Some(123.0),
                lufs_min: Some(-16.0),
                lufs_max: Some(-13.0),
            },
            20,
            0,
        )
        .unwrap();
        assert_eq!(semantic_total, 1);
        assert_eq!(semantic_matches.len(), 1);
        assert_eq!(semantic_matches[0].title, "Mood Track");
        let (no_tempo_matches, no_tempo_total) = search_tracks_page_with_filters(
            &connection,
            "",
            &TrackSearchFilters {
                bpm_min: Some(130.0),
                bpm_max: Some(140.0),
                ..TrackSearchFilters::default()
            },
            20,
            0,
        )
        .unwrap();
        assert_eq!(no_tempo_matches.len(), 0);
        assert_eq!(no_tempo_total, 0);
        let stored_vector_length: i64 = connection
            .query_row(
                "SELECT length(vector_blob) FROM track_analysis WHERE track_id = ?1",
                [found[0].id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored_vector_length, 64);
        let mood_query_plan: String = connection
            .query_row(
                "EXPLAIN QUERY PLAN SELECT track_id FROM track_analysis WHERE mood_label COLLATE NOCASE = 'Euphoric'",
                [],
                |row| row.get(3),
            )
            .unwrap();
        assert!(mood_query_plan.contains("idx_track_analysis_mood"), "{mood_query_plan}");
    }

    #[test]
    fn recording_metadata_persists_encoder_settings_and_completion_state() {
        let connection = open(Path::new(":memory:")).unwrap();
        let id = save_recording_metadata(
            &connection,
            "Friday session",
            "C:/Users/Champion/Music/ChampionVibe/Recordings/friday.wav",
            Some(3_600_000),
            &RecordingMetadata {
                format: Some("wavPcm24".to_owned()),
                sample_rate: Some(48_000),
                channel_count: Some(2),
                bit_depth: Some(24),
                bitrate_bps: Some(2_304_000),
                file_size_bytes: Some(1_036_800_000),
            },
        )
        .unwrap();
        let recording = connection
            .query_row(
                "SELECT format, sample_rate, channel_count, bit_depth, bitrate_bps,
                        file_size_bytes, status, finished_at
                 FROM recordings WHERE id = ?1",
                [id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<u32>>(1)?,
                        row.get::<_, Option<u32>>(2)?,
                        row.get::<_, Option<u32>>(3)?,
                        row.get::<_, Option<u32>>(4)?,
                        row.get::<_, Option<i64>>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, Option<i64>>(7)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(recording.0, "wavPcm24");
        assert_eq!(recording.1, Some(48_000));
        assert_eq!(recording.2, Some(2));
        assert_eq!(recording.3, Some(24));
        assert_eq!(recording.4, Some(2_304_000));
        assert_eq!(recording.5, Some(1_036_800_000));
        assert_eq!(recording.6, "complete");
        assert!(recording.7.is_some());
    }

    #[test]
    fn recording_metadata_migration_adds_columns_to_legacy_database() {
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE recordings (
                    id INTEGER PRIMARY KEY,
                    name TEXT NOT NULL,
                    file_path TEXT NOT NULL,
                    duration_ms INTEGER,
                    created_at INTEGER NOT NULL
                );
                INSERT INTO recordings (name, file_path, created_at)
                    VALUES ('Old session', 'old.wav', 1);",
            )
            .unwrap();

        ensure_recording_columns(&connection).unwrap();
        let defaults = connection
            .query_row(
                "SELECT format, status, sample_rate FROM recordings WHERE id = 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<u32>>(2)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(defaults.0, "unknown");
        assert_eq!(defaults.1, "complete");
        assert_eq!(defaults.2, None);
    }

    #[test]
    fn paged_track_search_returns_stable_pages_and_matching_total() {
        let mut connection = open(Path::new(":memory:")).unwrap();
        for index in 0..5 {
            upsert_track(
                &mut connection,
                &TrackInput {
                    file_path: format!("/music/track-{index}.wav"),
                    title: format!("Track {index}"),
                    artist: "Local artist".to_owned(),
                    duration_ms: Some(180_000),
                    codec: None,
                    bitrate_bps: None,
                    sample_rate: None,
                    channel_count: None,
                    tags_json: None,
                    bpm: None,
                    beat_grid_json: None,
                    sentiment: None,
                    stems: Vec::new(),
                },
            )
            .unwrap();
        }

        let (first_page, total) = search_tracks_page(&connection, "track", 2, 0).unwrap();
        let (second_page, second_total) = search_tracks_page(&connection, "track", 2, 2).unwrap();
        let (last_page, last_total) = search_tracks_page(&connection, "track", 2, 4).unwrap();
        assert_eq!(total, 5);
        assert_eq!(second_total, total);
        assert_eq!(last_total, total);
        assert_eq!(first_page.len(), 2);
        assert_eq!(second_page.len(), 2);
        assert_eq!(last_page.len(), 1);
        let ids = first_page
            .iter()
            .chain(&second_page)
            .chain(&last_page)
            .map(|track| track.id)
            .collect::<Vec<_>>();
        assert_eq!(ids.iter().copied().collect::<std::collections::HashSet<_>>().len(), 5);
    }

    #[test]
    fn mix_metadata_and_indexed_whisper_candidate_lookup_use_local_four_stem_tracks() {
        let mut connection = open(Path::new(":memory:")).unwrap();
        let make_track = |path: &str, title: &str| TrackInput {
            file_path: path.to_owned(),
            title: title.to_owned(),
            artist: "Local artist".to_owned(),
            duration_ms: Some(180_000),
            codec: None,
            bitrate_bps: None,
            sample_rate: None,
            channel_count: None,
            tags_json: None,
            bpm: Some(120.0),
            beat_grid_json: Some("[0.0,0.5,1.0,1.5]".to_owned()),
            sentiment: Some(-0.2),
            stems: ["acapella", "instrumental", "bass", "drums"]
                .into_iter()
                .map(|kind| super::StemPath {
                    kind: kind.to_owned(),
                    file_path: format!("{path}-{kind}.wav"),
                })
                .collect(),
        };
        let source_id = upsert_track(&mut connection, &make_track("source.wav", "Source")).unwrap();
        let candidate_id = upsert_track(&mut connection, &make_track("next.wav", "Next")).unwrap();
        let manual_riddim_id =
            upsert_track(&mut connection, &make_track("manual.wav", "Manual riddim match")).unwrap();
        connection
            .execute(
                "INSERT INTO lyric_words (track_id, word_index, word, start_ms, end_ms, confidence)
                 VALUES (?1, 0, 'fire,', 12000, 12300, 0.95)",
                [candidate_id],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO track_tags (track_id, namespace, tag, normalized_tag, created_at)
                 VALUES (?1, 'riddim', 'Sound System', 'sound system', 1),
                       (?2, 'riddim', 'sound system', 'sound system', 1)",
                rusqlite::params![source_id, manual_riddim_id],
            )
            .unwrap();

        let record = load_mix_track(&connection, source_id).unwrap().unwrap();
        assert_eq!(record.title, "Source");
        assert_eq!(record.stems.len(), 4);
        assert_eq!(record.sentiment, Some(-0.2));
        let candidates = mix_candidate_ids(
            &connection,
            source_id,
            None,
            Some(120.0),
            &["fire".to_owned()],
            12,
        )
        .unwrap();
        assert_eq!(candidates.first(), Some(&candidate_id));
        assert_eq!(candidates.get(1), Some(&manual_riddim_id));
    }

    #[test]
    fn import_job_counters_track_status_changes_without_aggregating_items() {
        let mut connection = open(Path::new(":memory:")).unwrap();
        let paths = vec!["track-a.wav".to_owned(), "track-b.wav".to_owned()];
        let job_id = create_import_job(
            &mut connection,
            &paths,
            "local-separator.onnx",
            "local-whisper.bin",
            false,
            4,
        )
        .unwrap();

        let queued = import_jobs(&connection, 10).unwrap().remove(0);
        assert_eq!(queued.total_items, 2);
        assert_eq!(queued.queued_items, 2);
        assert_eq!(queued.running_items, 0);

        assert_eq!(claim_next_import_job(&mut connection).unwrap(), Some(job_id));
        let item = claim_next_import_item(&mut connection, job_id)
            .unwrap()
            .expect("first import item should be queued");
        finish_import_item_failed(&connection, item.id, "test failure").unwrap();
        cancel_queued_import_items(&connection, job_id).unwrap();
        finish_import_job(&mut connection, job_id, true).unwrap();

        let finished = import_jobs(&connection, 10).unwrap().remove(0);
        assert_eq!(finished.status, "cancelled");
        assert_eq!(finished.total_items, 2);
        assert_eq!(finished.queued_items, 0);
        assert_eq!(finished.running_items, 0);
        assert_eq!(finished.failed_items, 1);
        assert_eq!(finished.cancelled_items, 1);
    }
}
