use crate::{commands::AppState, media_source};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use rustfft::{num_complex::Complex32, FftPlanner};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::OnceLock,
    time::{SystemTime, UNIX_EPOCH},
};
use symphonia::core::{
    codecs::audio::AudioDecoderOptions,
    errors::Error as SymphoniaError,
    formats::{FormatOptions, TrackType},
    io::MediaSourceStream,
    meta::MetadataOptions,
    probe::Hint,
};

const FINGERPRINT_VERSION: i64 = 1;
const TARGET_SAMPLE_RATE: u32 = 11_025;
const MAX_FINGERPRINT_SECONDS: u32 = 90;
const FFT_SIZE: usize = 1_024;
const HOP_SIZE: usize = 512;
const TIME_BINS: usize = 16;
const SPECTRAL_BANDS: usize = 16;
const FEATURES_PER_BIN: usize = SPECTRAL_BANDS + 1;
const FEATURE_COUNT: usize = TIME_BINS * FEATURES_PER_BIN;
const SIMHASH_BANDS: usize = 8;
const MAX_LSH_BUCKET_SIZE: usize = 4_096;
const MAX_CANDIDATE_HAMMING_DISTANCE: u32 = 24;
const DIRECT_AUDIO_THRESHOLD: f64 = 0.92;
const TAGGED_AUDIO_THRESHOLD: f64 = 0.80;
const TAG_JACCARD_THRESHOLD: f64 = 0.50;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManualTagInput {
    pub namespace: String,
    pub tag: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RiddimFamilySummary {
    pub id: i64,
    pub name: String,
    pub track_count: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackRiddimFamily {
    pub id: i64,
    pub name: String,
    pub track_count: i64,
    pub match_score: f64,
    pub match_method: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RebuildReport {
    pub indexed_instrumentals: usize,
    pub skipped_instrumentals: usize,
    pub families: usize,
    pub grouped_tracks: usize,
}

#[derive(Clone)]
struct Fingerprint {
    features: Vec<i16>,
    simhash: u64,
    hash: String,
}

struct TrackRecord {
    id: i64,
    title: String,
    fingerprint: Option<Fingerprint>,
    tags: Vec<ManualTagInput>,
    normalized_tags: Vec<String>,
}

#[derive(Clone, Copy, Default)]
struct MatchEvidence {
    score: f64,
    method: &'static str,
}

struct ClusteredFamilies {
    members: Vec<Vec<usize>>,
    evidence: Vec<MatchEvidence>,
    grouped_tracks: usize,
}

pub fn rebuild_in_state(state: &AppState) -> Result<RebuildReport> {
    let indexed_sources = {
        let connection = state
            .database
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite connection lock is poisoned"))?;
        connection.execute(
            "DELETE FROM instrumental_fingerprints
             WHERE NOT EXISTS (
                SELECT 1 FROM stems AS s
                WHERE s.track_id = instrumental_fingerprints.track_id
                  AND lower(s.kind) = 'instrumental'
                  AND s.file_path = instrumental_fingerprints.source_path
             )",
            [],
        )?;
        let mut statement = connection.prepare(
            "SELECT track_id, source_path FROM instrumental_fingerprints ORDER BY track_id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let missing_source_ids = indexed_sources
        .into_iter()
        .filter(|(_, path)| {
            let path = Path::new(path);
            crate::import::ensure_local_path(path).is_err() || !path.is_file()
        })
        .map(|(track_id, _)| track_id)
        .collect::<Vec<_>>();
    if !missing_source_ids.is_empty() {
        let mut connection = state
            .database
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite connection lock is poisoned"))?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        {
            let mut delete =
                transaction.prepare("DELETE FROM instrumental_fingerprints WHERE track_id = ?1")?;
            for track_id in missing_source_ids {
                delete.execute([track_id])?;
            }
        }
        transaction.commit()?;
    }

    let pending = {
        let connection = state
            .database
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite connection lock is poisoned"))?;
        missing_instrumentals(&connection)?
    };

    let mut indexed = Vec::new();
    let mut skipped = 0usize;
    for (track_id, source_path) in pending {
        let path = PathBuf::from(&source_path);
        if crate::import::ensure_local_path(&path).is_err() || !path.is_file() {
            skipped += 1;
            continue;
        }
        match fingerprint_instrumental(&path) {
            Ok(fingerprint) => indexed.push((track_id, source_path, fingerprint)),
            Err(error) => {
                skipped += 1;
                eprintln!(
                    "ChampionVibe could not fingerprint instrumental for track {track_id}: {error:#}"
                );
            }
        }
    }

    if !indexed.is_empty() {
        let mut connection = state
            .database
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite connection lock is poisoned"))?;
        store_fingerprints(&mut connection, &indexed)?;
    }

    let tracks = {
        let connection = state
            .database
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite connection lock is poisoned"))?;
        load_track_records(&connection)?
    };
    let clustered = cluster_tracks(&tracks);
    {
        let mut connection = state
            .database
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite connection lock is poisoned"))?;
        save_families(
            &mut connection,
            &tracks,
            &clustered.members,
            &clustered.evidence,
        )?;
    }
    Ok(RebuildReport {
        indexed_instrumentals: indexed.len(),
        skipped_instrumentals: skipped,
        families: clustered.members.len(),
        grouped_tracks: clustered.grouped_tracks,
    })
}

pub fn set_track_tags_in_state(
    state: &AppState,
    track_id: i64,
    tags: Vec<ManualTagInput>,
) -> Result<RebuildReport> {
    anyhow::ensure!(track_id > 0, "track id must be positive");
    anyhow::ensure!(tags.len() <= 50, "a track may have at most 50 manual tags");
    let mut normalized = Vec::with_capacity(tags.len());
    let mut seen = HashSet::with_capacity(tags.len());
    for tag in tags {
        let namespace = normalize_namespace(&tag.namespace).ok_or_else(|| {
            anyhow::anyhow!("unsupported manual tag namespace: {}", tag.namespace)
        })?;
        let display_tag = tag.tag.trim();
        anyhow::ensure!(
            !display_tag.is_empty() && display_tag.chars().count() <= 96,
            "manual tags must contain 1 to 96 characters"
        );
        anyhow::ensure!(
            !display_tag.chars().any(char::is_control),
            "manual tags cannot contain control characters"
        );
        let normalized_tag = normalize_tag(display_tag);
        anyhow::ensure!(!normalized_tag.is_empty(), "manual tag cannot be blank");
        if seen.insert((namespace, normalized_tag.clone())) {
            normalized.push((namespace.to_owned(), display_tag.to_owned(), normalized_tag));
        }
    }

    {
        let mut connection = state
            .database
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite connection lock is poisoned"))?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM tracks WHERE id = ?1)",
            [track_id],
            |row| row.get(0),
        )?;
        anyhow::ensure!(exists, "track does not exist");
        transaction.execute("DELETE FROM track_tags WHERE track_id = ?1", [track_id])?;
        {
            let mut insert = transaction.prepare(
                "INSERT INTO track_tags(track_id, namespace, tag, normalized_tag, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            let now = unix_time();
            for (namespace, tag, normalized_tag) in normalized {
                insert.execute(params![track_id, namespace, tag, normalized_tag, now])?;
            }
        }
        transaction.commit()?;
    }
    rebuild_in_state(state)
}

pub fn load_track_tags(connection: &Connection, track_id: i64) -> Result<Vec<ManualTagInput>> {
    let mut statement = connection.prepare(
        "SELECT namespace, tag FROM track_tags WHERE track_id = ?1
         ORDER BY namespace COLLATE NOCASE, normalized_tag",
    )?;
    let rows = statement.query_map([track_id], |row| {
        Ok(ManualTagInput {
            namespace: row.get(0)?,
            tag: row.get(1)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("could not read track tags")
}

pub fn list_families(connection: &Connection, limit: u32) -> Result<Vec<RiddimFamilySummary>> {
    let mut statement = connection.prepare(
        "SELECT id, name, track_count FROM riddim_families
         ORDER BY track_count DESC, name COLLATE NOCASE, id LIMIT ?1",
    )?;
    let rows = statement.query_map([limit.clamp(1, 250)], |row| {
        Ok(RiddimFamilySummary {
            id: row.get(0)?,
            name: row.get(1)?,
            track_count: row.get(2)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("could not read riddim families")
}

pub fn family_for_track(
    connection: &Connection,
    track_id: i64,
) -> Result<Option<TrackRiddimFamily>> {
    connection
        .query_row(
            "SELECT f.id, f.name, f.track_count, m.match_score, m.match_method
             FROM riddim_family_tracks AS m
             JOIN riddim_families AS f ON f.id = m.family_id
             WHERE m.track_id = ?1",
            [track_id],
            |row| {
                Ok(TrackRiddimFamily {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    track_count: row.get(2)?,
                    match_score: row.get(3)?,
                    match_method: row.get(4)?,
                })
            },
        )
        .optional()
        .context("could not load track riddim family")
}

fn missing_instrumentals(connection: &Connection) -> Result<Vec<(i64, String)>> {
    let mut statement = connection.prepare(
        "SELECT s.track_id, s.file_path
         FROM stems AS s
         LEFT JOIN instrumental_fingerprints AS f ON f.track_id = s.track_id
         WHERE lower(s.kind) = 'instrumental'
           AND (f.track_id IS NULL OR f.source_path != s.file_path OR f.algorithm_version != ?1)
         ORDER BY s.track_id",
    )?;
    let rows = statement.query_map([FINGERPRINT_VERSION], |row| Ok((row.get(0)?, row.get(1)?)))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("could not locate unindexed instrumental stems")
}

fn store_fingerprints(
    connection: &mut Connection,
    fingerprints: &[(i64, String, Fingerprint)],
) -> Result<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    {
        let mut upsert = transaction.prepare(
            "INSERT INTO instrumental_fingerprints(
                track_id, source_path, algorithm_version, features, simhash, fingerprint_hash, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(track_id) DO UPDATE SET
                source_path = excluded.source_path,
                algorithm_version = excluded.algorithm_version,
                features = excluded.features,
                simhash = excluded.simhash,
                fingerprint_hash = excluded.fingerprint_hash,
                created_at = excluded.created_at",
        )?;
        let mut clear_bands =
            transaction.prepare("DELETE FROM fingerprint_bands WHERE track_id = ?1")?;
        let mut insert_band = transaction.prepare(
            "INSERT INTO fingerprint_bands(track_id, band, band_value) VALUES (?1, ?2, ?3)",
        )?;
        let now = unix_time();
        for (track_id, source_path, fingerprint) in fingerprints {
            let blob = encode_features(&fingerprint.features);
            upsert.execute(params![
                track_id,
                source_path,
                FINGERPRINT_VERSION,
                blob,
                fingerprint.simhash as i64,
                fingerprint.hash,
                now,
            ])?;
            clear_bands.execute([track_id])?;
            for band in 0..SIMHASH_BANDS {
                let value = ((fingerprint.simhash >> (band * 8)) & 0xff) as i64;
                insert_band.execute(params![track_id, band as i64, value])?;
            }
        }
    }
    transaction.commit()?;
    Ok(())
}

fn load_track_records(connection: &Connection) -> Result<Vec<TrackRecord>> {
    let mut tracks = {
        let mut statement = connection.prepare("SELECT id, title FROM tracks ORDER BY id")?;
        let rows = statement.query_map([], |row| {
            Ok(TrackRecord {
                id: row.get(0)?,
                title: row.get(1)?,
                fingerprint: None,
                tags: Vec::new(),
                normalized_tags: Vec::new(),
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let positions = tracks
        .iter()
        .enumerate()
        .map(|(index, track)| (track.id, index))
        .collect::<HashMap<_, _>>();

    {
        let mut statement = connection.prepare(
            "SELECT track_id, features, simhash, fingerprint_hash, algorithm_version
             FROM instrumental_fingerprints ORDER BY track_id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?;
        for row in rows {
            let (track_id, blob, simhash, hash, version) = row?;
            if version != FINGERPRINT_VERSION {
                continue;
            }
            if let (Some(position), Some(features)) =
                (positions.get(&track_id).copied(), decode_features(&blob))
            {
                tracks[position].fingerprint = Some(Fingerprint {
                    features,
                    simhash: simhash as u64,
                    hash,
                });
            }
        }
    }
    {
        let mut statement = connection.prepare(
            "SELECT track_id, namespace, tag, normalized_tag FROM track_tags
             ORDER BY track_id, namespace, normalized_tag",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        for row in rows {
            let (track_id, namespace, tag, normalized_tag) = row?;
            if let Some(position) = positions.get(&track_id).copied() {
                tracks[position].normalized_tags.push(format!(
                    "{}:{}",
                    namespace,
                    normalize_tag(&normalized_tag)
                ));
                tracks[position]
                    .tags
                    .push(ManualTagInput { namespace, tag });
            }
        }
    }
    for track in &mut tracks {
        track.normalized_tags.sort_unstable();
        track.normalized_tags.dedup();
    }
    Ok(tracks)
}

fn cluster_tracks(tracks: &[TrackRecord]) -> ClusteredFamilies {
    let mut union_find = UnionFind::new(tracks.len());
    let mut evidence = vec![MatchEvidence::default(); tracks.len()];

    let mut named_riddims = HashMap::<String, usize>::new();
    for (index, track) in tracks.iter().enumerate() {
        for tag in track.tags.iter().filter(|tag| tag.namespace == "riddim") {
            let normalized = normalize_tag(&tag.tag);
            if let Some(first) = named_riddims.get(&normalized).copied() {
                union_find.union(first, index);
                record_evidence(&mut evidence, first, index, 1.0, "manual_riddim_tag");
            } else {
                named_riddims.insert(normalized, index);
            }
        }
    }

    let mut exact_fingerprints = HashMap::<&str, usize>::new();
    for (index, track) in tracks.iter().enumerate() {
        let Some(fingerprint) = &track.fingerprint else {
            continue;
        };
        if let Some(first) = exact_fingerprints.get(fingerprint.hash.as_str()).copied() {
            union_find.union(first, index);
            record_evidence(&mut evidence, first, index, 1.0, "instrumental_similarity");
        } else {
            exact_fingerprints.insert(fingerprint.hash.as_str(), index);
        }
    }

    let mut buckets = (0..SIMHASH_BANDS)
        .map(|_| HashMap::<u8, Vec<usize>>::new())
        .collect::<Vec<_>>();
    for (index, track) in tracks.iter().enumerate() {
        let Some(fingerprint) = &track.fingerprint else {
            continue;
        };
        for band in 0..SIMHASH_BANDS {
            let value = ((fingerprint.simhash >> (band * 8)) & 0xff) as u8;
            buckets[band].entry(value).or_default().push(index);
        }
    }

    let mut searchable_bands = vec![0u8; tracks.len()];
    for (band, band_buckets) in buckets.iter().enumerate() {
        for members in band_buckets.values() {
            if members.len() <= MAX_LSH_BUCKET_SIZE {
                for &index in members {
                    searchable_bands[index] |= 1 << band;
                }
            }
        }
    }

    for (band, band_buckets) in buckets.iter().enumerate() {
        for members in band_buckets.values() {
            // Common bands are often caused by near-silent or unusually uniform audio. Other
            // bands still provide candidates; skipping huge buckets bounds worst-case work.
            if members.len() > MAX_LSH_BUCKET_SIZE {
                continue;
            }
            for left_position in 0..members.len() {
                let left_index = members[left_position];
                let Some(left_fp) = &tracks[left_index].fingerprint else {
                    continue;
                };
                for right_index in members.iter().copied().skip(left_position + 1) {
                    let Some(right_fp) = &tracks[right_index].fingerprint else {
                        continue;
                    };
                    if shares_earlier_band(
                        left_fp.simhash,
                        right_fp.simhash,
                        band,
                        searchable_bands[left_index],
                        searchable_bands[right_index],
                    ) || left_fp.hash == right_fp.hash
                    {
                        continue;
                    }
                    if (left_fp.simhash ^ right_fp.simhash).count_ones()
                        > MAX_CANDIDATE_HAMMING_DISTANCE
                    {
                        continue;
                    }
                    let audio_similarity = cosine_similarity(&left_fp.features, &right_fp.features);
                    if audio_similarity < TAGGED_AUDIO_THRESHOLD {
                        continue;
                    }
                    let tag_similarity = tag_jaccard(
                        &tracks[left_index].normalized_tags,
                        &tracks[right_index].normalized_tags,
                    );
                    let method = if audio_similarity >= DIRECT_AUDIO_THRESHOLD {
                        Some("instrumental_similarity")
                    } else if tag_similarity >= TAG_JACCARD_THRESHOLD {
                        Some("instrumental_plus_manual_tags")
                    } else {
                        None
                    };
                    if let Some(method) = method {
                        union_find.union(left_index, right_index);
                        record_evidence(
                            &mut evidence,
                            left_index,
                            right_index,
                            audio_similarity,
                            method,
                        );
                    }
                }
            }
        }
    }

    let mut components = HashMap::<usize, Vec<usize>>::new();
    for index in 0..tracks.len() {
        let root = union_find.find(index);
        components.entry(root).or_default().push(index);
    }
    let mut families = components
        .into_values()
        .filter(|members| members.len() > 1)
        .collect::<Vec<_>>();
    for members in &mut families {
        members.sort_by_key(|index| tracks[*index].id);
    }
    families.sort_by_key(|members| tracks[members[0]].id);
    let grouped_tracks = families.iter().map(Vec::len).sum();
    ClusteredFamilies {
        members: families,
        evidence,
        grouped_tracks,
    }
}

fn record_evidence(
    evidence: &mut [MatchEvidence],
    left: usize,
    right: usize,
    score: f64,
    method: &'static str,
) {
    for index in [left, right] {
        if score > evidence[index].score {
            evidence[index] = MatchEvidence { score, method };
        }
    }
}

fn save_families(
    connection: &mut Connection,
    tracks: &[TrackRecord],
    families: &[Vec<usize>],
    evidence: &[MatchEvidence],
) -> Result<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute("DELETE FROM riddim_family_tracks", [])?;
    transaction.execute("DELETE FROM riddim_families", [])?;
    let now = unix_time();
    for members in families {
        let first = members
            .first()
            .ok_or_else(|| anyhow::anyhow!("empty riddim family in clustering result"))?;
        let canonical_track_id = tracks[*first].id;
        let family_key = format!("automatic:{canonical_track_id}");
        let name = family_name(members, tracks, canonical_track_id);
        transaction.execute(
            "INSERT INTO riddim_families(family_key, name, track_count, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![family_key, name, members.len() as i64, now],
        )?;
        let family_id = transaction.last_insert_rowid();
        let mut insert = transaction.prepare(
            "INSERT INTO riddim_family_tracks(track_id, family_id, match_score, match_method)
             VALUES (?1, ?2, ?3, ?4)",
        )?;
        for index in members {
            let track = &tracks[*index];
            let explicit_riddim = track.tags.iter().any(|tag| tag.namespace == "riddim");
            let match_evidence = evidence.get(*index).copied().unwrap_or_default();
            let method = if explicit_riddim {
                "manual_riddim_tag"
            } else if !match_evidence.method.is_empty() {
                match_evidence.method
            } else if track.fingerprint.is_some() {
                "instrumental_similarity_component"
            } else {
                "manual_tag_component"
            };
            let score = if explicit_riddim {
                1.0
            } else {
                match_evidence.score.max(0.5).clamp(0.0, 1.0)
            };
            insert.execute(params![track.id, family_id, score, method])?;
        }
    }
    transaction.commit()?;
    Ok(())
}

fn family_name(members: &[usize], tracks: &[TrackRecord], canonical_track_id: i64) -> String {
    let mut riddim_tags = members
        .iter()
        .flat_map(|index| tracks[*index].tags.iter())
        .filter(|tag| tag.namespace == "riddim")
        .map(|tag| tag.tag.trim().to_owned())
        .filter(|tag| !tag.is_empty())
        .collect::<Vec<_>>();
    riddim_tags.sort_by_key(|tag| tag.to_lowercase());
    if let Some(name) = riddim_tags.into_iter().next() {
        return name;
    }
    let title = members
        .first()
        .map(|index| tracks[*index].title.trim())
        .filter(|title| !title.is_empty())
        .unwrap_or("Untitled instrumental");
    format!("Riddim · {title} #{canonical_track_id}")
}

fn normalize_namespace(namespace: &str) -> Option<&'static str> {
    match namespace.trim().to_ascii_lowercase().as_str() {
        "riddim" => Some("riddim"),
        "producer" => Some("producer"),
        "genre" => Some("genre"),
        "label" => Some("label"),
        "custom" | "tag" => Some("custom"),
        _ => None,
    }
}

fn normalize_tag(tag: &str) -> String {
    tag.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn tag_jaccard(left: &[String], right: &[String]) -> f64 {
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    let mut intersection = 0usize;
    let (mut left_index, mut right_index) = (0usize, 0usize);
    while left_index < left.len() && right_index < right.len() {
        match left[left_index].cmp(&right[right_index]) {
            std::cmp::Ordering::Less => left_index += 1,
            std::cmp::Ordering::Greater => right_index += 1,
            std::cmp::Ordering::Equal => {
                intersection += 1;
                left_index += 1;
                right_index += 1;
            }
        }
    }
    let union = left.len() + right.len() - intersection;
    if union == 0 {
        0.0
    } else {
        intersection as f64 / union as f64
    }
}

fn shares_earlier_band(
    left: u64,
    right: u64,
    current_band: usize,
    left_searchable: u8,
    right_searchable: u8,
) -> bool {
    (0..current_band).any(|band| {
        let mask = 1 << band;
        left_searchable & mask != 0
            && right_searchable & mask != 0
            && ((left >> (band * 8)) & 0xff) == ((right >> (band * 8)) & 0xff)
    })
}

fn cosine_similarity(left: &[i16], right: &[i16]) -> f64 {
    if left.len() != right.len() || left.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f64;
    let mut left_norm = 0.0f64;
    let mut right_norm = 0.0f64;
    for (left, right) in left.iter().zip(right) {
        let left = f64::from(*left);
        let right = f64::from(*right);
        dot += left * right;
        left_norm += left * left;
        right_norm += right * right;
    }
    if left_norm <= 0.0 || right_norm <= 0.0 {
        0.0
    } else {
        (dot / (left_norm * right_norm).sqrt()).clamp(0.0, 1.0)
    }
}

fn fingerprint_instrumental(path: &Path) -> Result<Fingerprint> {
    let source = media_source::open_media_source(path)
        .with_context(|| format!("cannot open instrumental stem {}", path.display()))?;
    let source = MediaSourceStream::new(source, Default::default());
    let hint = Hint::new();
    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            source,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .with_context(|| format!("unsupported instrumental audio: {}", path.display()))?;
    let (track_id, codec_params) = {
        let track = format
            .first_track_known_codec(TrackType::Audio)
            .ok_or_else(|| anyhow::anyhow!("no decodable audio track in {}", path.display()))?;
        let params = track
            .codec_params
            .as_ref()
            .and_then(|params| params.audio())
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("instrumental codec parameters are missing"))?;
        (track.id, params)
    };
    let mut decoder = crate::symphonia_support::codec_registry()
        .make_audio_decoder(&codec_params, &AudioDecoderOptions::default())
        .with_context(|| format!("unsupported instrumental codec: {}", path.display()))?;

    let mut mono = Vec::<f32>::new();
    let mut input_rate = None;
    let mut input_channels = None;
    'packets: loop {
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(SymphoniaError::ResetRequired) => {
                anyhow::bail!("instrumental changed its codec configuration mid-file")
            }
            Err(error) => return Err(error).context("instrumental demux failed"),
        };
        if packet.track_id != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(SymphoniaError::ResetRequired) => {
                decoder.reset();
                continue;
            }
            Err(error) => return Err(error).context("instrumental decode failed"),
        };
        let sample_rate = decoded.spec().rate();
        let channels = decoded.spec().channels().count();
        if sample_rate == 0 || channels == 0 {
            anyhow::bail!("instrumental decoder reported invalid audio format")
        }
        if input_rate.is_some_and(|rate| rate != sample_rate)
            || input_channels.is_some_and(|count| count != channels)
        {
            anyhow::bail!("instrumental format changed during decoding")
        }
        input_rate = Some(sample_rate);
        input_channels = Some(channels);

        let interleaved_count = decoded.samples_interleaved();
        let mut interleaved = vec![0.0f32; interleaved_count];
        decoded.copy_to_slice_interleaved(&mut interleaved);
        let max_input_frames =
            (u64::from(sample_rate).saturating_mul(u64::from(MAX_FINGERPRINT_SECONDS))) as usize;
        for channel_frame in interleaved.chunks_exact(channels) {
            if mono.len() >= max_input_frames {
                break 'packets;
            }
            let sample = channel_frame
                .iter()
                .copied()
                .filter(|value| value.is_finite())
                .sum::<f32>()
                / channels as f32;
            mono.push(sample.clamp(-1.0, 1.0));
        }
    }
    let input_rate = input_rate.unwrap_or(TARGET_SAMPLE_RATE);
    anyhow::ensure!(!mono.is_empty(), "instrumental contains no decodable audio");

    // Keep a bounded, uniformly spaced preview in RAM: 90 seconds at 11.025 kHz is
    // enough for a small descriptor, while the decoder still streams the FLAC/WAV.
    let input_limit = (u64::from(input_rate) * u64::from(MAX_FINGERPRINT_SECONDS)) as usize;
    if mono.len() > input_limit {
        mono.truncate(input_limit);
    }
    let output_length =
        ((mono.len() as u64 * u64::from(TARGET_SAMPLE_RATE)) / u64::from(input_rate)) as usize;
    let mut samples = Vec::with_capacity(output_length.max(FFT_SIZE));
    for output_index in 0..output_length {
        let source_position =
            output_index as f64 * f64::from(input_rate) / f64::from(TARGET_SAMPLE_RATE);
        let first_index = (source_position.floor() as usize).min(mono.len() - 1);
        let second_index = first_index.saturating_add(1).min(mono.len() - 1);
        let fraction = (source_position - first_index as f64) as f32;
        samples.push(mono[first_index] + (mono[second_index] - mono[first_index]) * fraction);
    }
    anyhow::ensure!(
        samples.len() >= FFT_SIZE,
        "instrumental stem is shorter than the fingerprint window"
    );

    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(FFT_SIZE);
    let mut fft_buffer = vec![Complex32::new(0.0, 0.0); FFT_SIZE];
    let mut sums = [[0.0f64; SPECTRAL_BANDS]; TIME_BINS];
    let mut energy_sums = [0.0f64; TIME_BINS];
    let mut frame_counts = [0u32; TIME_BINS];
    let sample_duration = samples.len();
    let min_log_frequency = 40.0f32.ln();
    let max_log_frequency = 5_000.0f32.ln();

    for start in (0..=samples.len() - FFT_SIZE).step_by(HOP_SIZE) {
        let time_bin = ((start * TIME_BINS) / sample_duration).min(TIME_BINS - 1);
        for index in 0..FFT_SIZE {
            let window = 0.5f32
                - 0.5f32 * (std::f32::consts::TAU * index as f32 / (FFT_SIZE - 1) as f32).cos();
            fft_buffer[index] = Complex32::new(samples[start + index] * window, 0.0);
        }
        fft.process(&mut fft_buffer);
        let mut band_energy = [0.0f64; SPECTRAL_BANDS];
        for (bin, value) in fft_buffer.iter().take(FFT_SIZE / 2).enumerate().skip(1) {
            let frequency = bin as f32 * TARGET_SAMPLE_RATE as f32 / FFT_SIZE as f32;
            if !(40.0..=5_000.0).contains(&frequency) {
                continue;
            }
            let log_frequency = frequency.ln();
            let normalized = ((log_frequency - min_log_frequency)
                / (max_log_frequency - min_log_frequency))
                .clamp(0.0, 0.999_999);
            let band = (normalized * SPECTRAL_BANDS as f32) as usize;
            band_energy[band] += f64::from(value.norm_sqr());
        }
        let total_energy = band_energy.iter().sum::<f64>();
        energy_sums[time_bin] += total_energy.max(1.0e-16).ln();
        if total_energy > 1.0e-16 {
            for (band, energy) in band_energy.iter().enumerate() {
                sums[time_bin][band] += *energy / total_energy;
            }
        }
        frame_counts[time_bin] = frame_counts[time_bin].saturating_add(1);
    }

    let mut features = vec![0.0f64; FEATURE_COUNT];
    let mut bucket_log_energy = [0.0f64; TIME_BINS];
    for time_bin in 0..TIME_BINS {
        let count = f64::from(frame_counts[time_bin].max(1));
        let mut row_logs = [0.0f64; SPECTRAL_BANDS];
        for band in 0..SPECTRAL_BANDS {
            let mean = sums[time_bin][band] / count;
            row_logs[band] = mean.max(1.0e-12).ln();
        }
        let row_mean = row_logs.iter().sum::<f64>() / SPECTRAL_BANDS as f64;
        for band in 0..SPECTRAL_BANDS {
            features[time_bin * FEATURES_PER_BIN + band] = row_logs[band] - row_mean;
        }
        bucket_log_energy[time_bin] = if frame_counts[time_bin] > 0 {
            energy_sums[time_bin] / count
        } else {
            -30.0
        };
    }
    let energy_mean = bucket_log_energy.iter().sum::<f64>() / TIME_BINS as f64;
    for time_bin in 0..TIME_BINS {
        features[time_bin * FEATURES_PER_BIN + SPECTRAL_BANDS] =
            bucket_log_energy[time_bin] - energy_mean;
    }
    let norm = features
        .iter()
        .map(|value| value * value)
        .sum::<f64>()
        .sqrt();
    anyhow::ensure!(norm > 1.0e-9, "instrumental has no usable spectral energy");
    let quantized = features
        .into_iter()
        .map(|value| {
            ((value / norm) * i16::MAX as f64)
                .round()
                .clamp(-32767.0, 32767.0) as i16
        })
        .collect::<Vec<_>>();
    let simhash = calculate_simhash(&quantized);
    let hash = hex::encode(Sha256::digest(encode_features(&quantized)));
    Ok(Fingerprint {
        features: quantized,
        simhash,
        hash,
    })
}

fn calculate_simhash(features: &[i16]) -> u64 {
    static PROJECTIONS: OnceLock<[[i8; FEATURE_COUNT]; 64]> = OnceLock::new();
    let projections = PROJECTIONS.get_or_init(|| {
        std::array::from_fn(|bit| {
            std::array::from_fn(|index| {
                let seed = ((bit as u64) << 32) ^ index as u64 ^ 0x6a09_e667_f3bc_c909;
                if mix64(seed) & 1 == 0 {
                    1
                } else {
                    -1
                }
            })
        })
    });
    let mut result = 0u64;
    for (bit, signs) in projections.iter().enumerate() {
        let mut projection = 0i32;
        for (feature, sign) in features.iter().zip(signs) {
            projection += i32::from(*feature) * i32::from(*sign);
        }
        if projection >= 0 {
            result |= 1u64 << bit;
        }
    }
    result
}

fn mix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

fn encode_features(features: &[i16]) -> Vec<u8> {
    let mut blob = Vec::with_capacity(features.len() * 2);
    for feature in features {
        blob.extend_from_slice(&feature.to_le_bytes());
    }
    blob
}

fn decode_features(blob: &[u8]) -> Option<Vec<i16>> {
    if blob.len() != FEATURE_COUNT * 2 {
        return None;
    }
    Some(
        blob.chunks_exact(2)
            .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
            .collect(),
    )
}

fn unix_time() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or_default()
}

struct UnionFind {
    parent: Vec<usize>,
    rank: Vec<u8>,
}

impl UnionFind {
    fn new(length: usize) -> Self {
        Self {
            parent: (0..length).collect(),
            rank: vec![0; length],
        }
    }

    fn find(&mut self, index: usize) -> usize {
        if self.parent[index] != index {
            self.parent[index] = self.find(self.parent[index]);
        }
        self.parent[index]
    }

    fn union(&mut self, left: usize, right: usize) {
        let mut left_root = self.find(left);
        let mut right_root = self.find(right);
        if left_root == right_root {
            return;
        }
        if self.rank[left_root] < self.rank[right_root] {
            std::mem::swap(&mut left_root, &mut right_root);
        }
        self.parent[right_root] = left_root;
        if self.rank[left_root] == self.rank[right_root] {
            self.rank[left_root] = self.rank[left_root].saturating_add(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: i64, hash: u64, features: &[i16], tags: Vec<ManualTagInput>) -> TrackRecord {
        TrackRecord {
            id,
            title: format!("track-{id}"),
            fingerprint: Some(Fingerprint {
                features: features.to_vec(),
                simhash: hash,
                hash: format!("{hash:x}"),
            }),
            normalized_tags: {
                let mut values = tags
                    .iter()
                    .map(|tag| format!("{}:{}", tag.namespace, normalize_tag(&tag.tag)))
                    .collect::<Vec<_>>();
                values.sort_unstable();
                values
            },
            tags,
        }
    }

    #[test]
    fn manual_riddim_tags_join_tracks_without_audio_fingerprints() {
        let tags = vec![ManualTagInput {
            namespace: "riddim".to_owned(),
            tag: "Sleng Teng".to_owned(),
        }];
        let first = TrackRecord {
            id: 1,
            title: "one".to_owned(),
            fingerprint: None,
            tags: tags.clone(),
            normalized_tags: vec!["riddim:sleng teng".to_owned()],
        };
        let second = TrackRecord {
            id: 2,
            title: "two".to_owned(),
            fingerprint: None,
            tags,
            normalized_tags: vec!["riddim:sleng teng".to_owned()],
        };
        let clustered = cluster_tracks(&[first, second]);
        assert_eq!(clustered.members.len(), 1);
        assert_eq!(clustered.grouped_tracks, 2);
    }

    #[test]
    fn similar_instrumental_vectors_form_a_family() {
        let left = [10i16, 20, 30, 40, 50, 60];
        let right = [10i16, 20, 30, 40, 50, 60];
        let first = track(10, 0x0808_0808_0808_0808, &left, Vec::new());
        let second = track(11, 0x0808_0808_0808_0808, &right, Vec::new());
        let clustered = cluster_tracks(&[first, second]);
        assert_eq!(clustered.members.len(), 1);
        assert_eq!(clustered.grouped_tracks, 2);
        assert_eq!(cosine_similarity(&left, &right), 1.0);
    }

    #[test]
    fn unrelated_vectors_do_not_match_without_shared_manual_tags() {
        let left = [100i16, 0, 0, 0];
        let right = [0i16, 0, 0, 100];
        assert_eq!(cosine_similarity(&left, &right), 0.0);
        assert_eq!(tag_jaccard(&[], &[]), 0.0);
    }

    #[test]
    fn normalized_tags_ignore_case_and_whitespace() {
        assert_eq!(normalize_tag("  King   Jammy "), "king jammy");
    }
}
