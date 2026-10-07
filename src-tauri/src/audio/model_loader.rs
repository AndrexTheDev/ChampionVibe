//! Zero-risk separator model strategy.
//!
//! ChampionVibe ships **no** separator weights and downloads **only** artifacts whose code *and*
//! weights are cleared for commercial use. There are exactly two tiers:
//!
//! 1. **Default tier** (`cleared`): a SHA-256-pinned Spleeter 2-stem ONNX export
//!    (MIT weights by Deezer, Apache-2.0 conversion by sherpa-onnx). The registry entry in
//!    `assets/model-registry.json` is the single source of truth for URL, byte size, SHA-256,
//!    archive members and licence notices, and `.github/workflows/model-pins.yml` re-verifies the
//!    pin against the bytes the release actually serves on every change.
//! 2. **BYOM tier** (Bring Your Own Model): the user drops a local `.onnx`/`.cvmodel.json` file.
//!    ChampionVibe never downloads, copies or redistributes those weights. It validates structure
//!    and tensor shapes, stores the *local path* plus a hash in `byom-models.json`, and requires an
//!    explicit acknowledgement for models whose rights are unclear (HT-Demucs, MDX-Net mirrors).
//!
//! Unclear-rights sources (HT-Demucs, KUIELab/UVR mirrors, Open-Unmix) are listed in
//! `policy.forbiddenDownloadSources` and both the Rust unit tests and the CI pin job fail if any of
//! them ever becomes downloadable again.
//!
//! Every download is length-limited, hashed while on disk, and only then installed; a pin that has
//! not been verified against upstream bytes (`PENDING_VERIFICATION`) is rejected instead of being
//! trusted.

use anyhow::{Context, Result};
use ort::{
    session::Session,
    value::{TensorElementType, ValueType},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs::{self, File},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::OnceLock,
};

pub(crate) const REGISTRY_FORMAT: &str = "championvibe-model-registry-v2";
pub(crate) const SPLEETER_DESCRIPTOR_FORMAT: &str = "championvibe-spleeter-onnx-v1";
pub(crate) const BYOM_STORE_FORMAT: &str = "championvibe-byom-v1";
const REGISTRY_JSON: &str = include_str!("../../assets/model-registry.json");
const BYOM_STORE_FILE: &str = "byom-models.json";
#[cfg(test)]
const PIN_PENDING: &str = "PENDING_VERIFICATION";
const MAX_DESCRIPTOR_BYTES: u64 = 64 * 1024;
const RECOMMENDED_MEMBER_BYTES: u64 = 512 * 1024 * 1024;

/// Spleeter 2-stem U-Net contract: float32 `[2, splits, 512, 1024]` in, same shape out.
pub(crate) const SPLEETER_SAMPLE_RATE: u32 = 44_100;
pub(crate) const SPLEETER_N_FFT: usize = 4_096;
pub(crate) const SPLEETER_HOP_LENGTH: usize = 1_024;
pub(crate) const SPLEETER_BINS: usize = 1_024;
pub(crate) const SPLEETER_FRAMES: usize = 512;
/// HT-Demucs contract that ChampionVibe can still load from a user-supplied file.
pub(crate) const HTDEMUCS_WINDOW_SAMPLES: usize = 343_980;
/// MDX-Net contract, imported through a `.cvmodel.json` manifest.
pub(crate) const MDX_DIM_F: usize = 2_048;
pub(crate) const MDX_FRAMES: usize = 512;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRegistry {
    pub format: String,
    pub policy: RegistryPolicy,
    pub artifacts: Vec<RegistryArtifact>,
    pub byom: ByomPolicy,
}

/// The machine-readable form of the clearing rules: the download loader enforces
/// `cleared_tier_only`, while `statement` and `pin_rule` are quoted by
/// `scripts/verify-model-pins.sh`, `docs/model-registry.md` and the pin workflow.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct RegistryPolicy {
    pub default_separator_id: String,
    pub cleared_tier_only: bool,
    pub statement: String,
    pub forbidden_download_sources: Vec<String>,
    pub pin_rule: String,
}

/// Mirrors one entry of `assets/model-registry.json`. The provenance fields (`weightsLicense`,
/// `conversionCopyright`, `upstreamStatement`, `rightsNote`, …) exist so that the JSON stays the
/// single audited source; the frontend and the pin workflow read them, which is why this struct
/// does not re-read every field at runtime.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct RegistryArtifact {
    pub id: String,
    pub tier: String,
    pub kind: String,
    pub title: String,
    pub purpose: String,
    pub repository: String,
    pub release_tag: String,
    pub asset: String,
    pub url: String,
    pub checksum_source: Option<String>,
    pub sha256: String,
    pub size_bytes: u64,
    pub max_bytes: u64,
    pub archive: String,
    pub archive_root: String,
    pub members: Vec<String>,
    pub approximate_size: String,
    pub sample_rate: u32,
    pub weights_license: String,
    pub weights_copyright: String,
    pub conversion_license: String,
    pub conversion_copyright: String,
    pub license_summary: String,
    pub upstream_statement: String,
    pub rights_note: String,
    pub notices: Vec<String>,
}

/// Kinds are entered through `accepted_kinds` and `requires_acknowledgement`; `statement` is the
/// user-facing liability text surfaced by the BYOM importer UI.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct ByomPolicy {
    pub statement: String,
    pub accepted_kinds: Vec<ByomAcceptedKind>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ByomAcceptedKind {
    pub kind: String,
    pub contract: String,
    pub requires_acknowledgement: bool,
    #[serde(default)]
    pub rights_warning: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedArtifactInfo {
    pub id: String,
    pub title: String,
    pub purpose: String,
    pub approximate_size: String,
    pub license_summary: String,
    pub source_url: String,
    pub sha256: String,
    pub sha256_verified: bool,
    pub installed: bool,
    pub installed_path: Option<String>,
    pub rights_note: String,
    pub notices: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ByomModelRecord {
    pub id: String,
    pub kind: String,
    pub display_name: String,
    pub path: String,
    pub file_name: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub contract: String,
    pub added_at_unix: u64,
    pub acknowledgement: bool,
    #[serde(default)]
    pub rights_warning: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ByomContractReport {
    pub valid: bool,
    pub kind: String,
    pub contract: String,
    pub details: String,
    pub path: String,
    pub input_names: Vec<String>,
    pub input_shapes: Vec<Vec<i64>>,
    pub output_shapes: Vec<Vec<i64>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ByomStore {
    format: String,
    models: Vec<ByomModelRecord>,
}

/// Descriptor written next to the installed Spleeter graphs so the import pipeline can select the
/// two-stem profile without re-deriving it from file names.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SpleeterDescriptor {
    pub format: String,
    pub id: String,
    pub kind: String,
    pub sample_rate: u32,
    pub n_fft: usize,
    pub hop_length: usize,
    pub window: String,
    pub center: bool,
    pub bins: usize,
    pub frames: usize,
    pub stems: Vec<String>,
    pub models: HashMap<String, String>,
    pub sha256: HashMap<String, String>,
    pub installed_at_unix: u64,
    pub license_summary: String,
    pub notices: Vec<String>,
    pub source_url: String,
    pub source_sha256: String,
}

pub(crate) fn registry() -> &'static ModelRegistry {
    static REGISTRY: OnceLock<ModelRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let registry: ModelRegistry = serde_json::from_str(REGISTRY_JSON)
            .expect("assets/model-registry.json must deserialize");
        assert_eq!(
            registry.format, REGISTRY_FORMAT,
            "assets/model-registry.json must declare the {REGISTRY_FORMAT} format"
        );
        registry
    })
}

pub(crate) fn default_separator_id() -> &'static str {
    registry().policy.default_separator_id.as_str()
}

pub(crate) fn artifact(id: &str) -> Option<&'static RegistryArtifact> {
    registry()
        .artifacts
        .iter()
        .find(|artifact| artifact.id == id)
}

fn is_pinned(artifact: &RegistryArtifact) -> bool {
    artifact.sha256.len() == 64 && artifact.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(crate) struct ModelLoader {
    root: PathBuf,
    client: reqwest::Client,
}

impl ModelLoader {
    pub fn new(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(&root).with_context(|| {
            format!(
                "could not create the managed model directory {}",
                root.display()
            )
        })?;
        let client = reqwest::Client::builder()
            .user_agent("ChampionVibe/0.1 cleared-model-loader")
            .timeout(std::time::Duration::from_secs(30 * 60))
            .build()
            .context("could not configure the model downloader")?;
        Ok(Self { root, client })
    }

    pub fn catalog(&self) -> Vec<ManagedArtifactInfo> {
        registry()
            .artifacts
            .iter()
            .map(|artifact| {
                let installed_path = self.installed_descriptor(artifact);
                ManagedArtifactInfo {
                    id: artifact.id.clone(),
                    title: artifact.title.clone(),
                    purpose: artifact.purpose.clone(),
                    approximate_size: artifact.approximate_size.clone(),
                    license_summary: artifact.license_summary.clone(),
                    source_url: artifact.url.clone(),
                    sha256: artifact.sha256.clone(),
                    sha256_verified: is_pinned(artifact),
                    installed: installed_path.is_some(),
                    installed_path: installed_path.map(|path| path.to_string_lossy().into_owned()),
                    rights_note: artifact.rights_note.clone(),
                    notices: artifact.notices.clone(),
                }
            })
            .collect()
    }

    /// Path of the descriptor that marks an installed artifact as ready for inference.
    pub(crate) fn descriptor_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.cvmodel.json"))
    }

    fn descriptor_directory(&self, id: &str) -> PathBuf {
        self.root.join("installed").join(id)
    }

    fn installed_descriptor(&self, artifact: &RegistryArtifact) -> Option<PathBuf> {
        let descriptor = self.descriptor_path(&artifact.id);
        if descriptor.is_file() {
            return Some(descriptor);
        }
        None
    }

    /// First installed separator descriptor, preferring the registry default.
    pub(crate) fn installed_separator_descriptor(&self) -> Option<PathBuf> {
        let default = self.descriptor_path(default_separator_id());
        if default.is_file() {
            return Some(default);
        }
        registry()
            .artifacts
            .iter()
            .map(|artifact| self.descriptor_path(&artifact.id))
            .find(|path| path.is_file())
    }

    pub async fn download_separator(&self, id: &str) -> Result<PathBuf> {
        let artifact = artifact(id).with_context(|| {
            format!("unknown managed model id {id}; only cleared artifacts are downloadable")
        })?;
        anyhow::ensure!(
            artifact.tier == "cleared",
            "only cleared-tier artifacts can be downloaded"
        );
        anyhow::ensure!(
            registry().policy.cleared_tier_only,
            "the model registry does not declare cleared-tier-only downloads; refusing to download"
        );
        anyhow::ensure!(
            is_pinned(artifact),
            "{} has no verified SHA-256 pin yet ({}) and is therefore not installable; see reports/model-pin-verification.json",
            artifact.id,
            artifact.sha256
        );
        anyhow::ensure!(
            forbidden_source_hits(&artifact.url).is_empty(),
            "{} resolves to an uncleared separator source",
            artifact.id
        );

        let directory = self.descriptor_directory(&artifact.id);
        fs::create_dir_all(&directory)
            .with_context(|| format!("could not create {}", directory.display()))?;
        let archive_path = directory.join(&artifact.asset);
        let archive_hash = self.download_archive(artifact, &archive_path).await?;
        let descriptor =
            self.install_from_archive(artifact, &archive_path, &archive_hash, &directory)?;
        Ok(descriptor)
    }

    async fn download_archive(&self, artifact: &RegistryArtifact, path: &Path) -> Result<String> {
        if path.is_file() {
            let existing = fs::metadata(path)?.len();
            if existing == artifact.size_bytes {
                let hash = sha256_file(path)?;
                if hash.eq_ignore_ascii_case(&artifact.sha256) {
                    return Ok(hash);
                }
            }
        }
        let temporary = path.with_extension(format!("{}.part", std::process::id()));
        let result = async {
            let mut response = self
                .client
                .get(&artifact.url)
                .send()
                .await
                .with_context(|| format!("could not download {}", artifact.asset))?
                .error_for_status()
                .with_context(|| {
                    format!("model source returned an error for {}", artifact.asset)
                })?;
            let final_url = response.url().clone();
            anyhow::ensure!(
                final_url.scheme() == "https",
                "model download redirected away from HTTPS"
            );
            anyhow::ensure!(
                final_url.host_str().is_some_and(trusted_download_host),
                "model download redirected to an untrusted host"
            );
            let mut file = File::create(&temporary)
                .with_context(|| format!("could not create {}", temporary.display()))?;
            let mut downloaded = 0_u64;
            while let Some(chunk) = response
                .chunk()
                .await
                .with_context(|| format!("download stream failed for {}", artifact.asset))?
            {
                downloaded = downloaded.saturating_add(chunk.len() as u64);
                anyhow::ensure!(
                    downloaded <= artifact.max_bytes,
                    "{} exceeded its {} byte limit",
                    artifact.asset,
                    artifact.max_bytes
                );
                file.write_all(&chunk)?;
            }
            anyhow::ensure!(downloaded > 0, "model source returned an empty file");
            file.sync_all()?;
            let hash = sha256_file(&temporary)?;
            anyhow::ensure!(
                hash.eq_ignore_ascii_case(&artifact.sha256),
                "SHA-256 mismatch for {}: expected {}, got {}",
                artifact.asset,
                artifact.sha256,
                hash
            );
            anyhow::ensure!(
                downloaded == artifact.size_bytes,
                "{} size drifted: expected {} bytes, got {}",
                artifact.asset,
                artifact.size_bytes,
                downloaded
            );
            fs::rename(&temporary, path).with_context(|| {
                format!(
                    "could not install the verified archive at {}",
                    path.display()
                )
            })?;
            Ok(hash)
        }
        .await;
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    fn install_from_archive(
        &self,
        artifact: &RegistryArtifact,
        archive_path: &Path,
        archive_hash: &str,
        directory: &Path,
    ) -> Result<PathBuf> {
        let members = &artifact.members;
        let reader = File::open(archive_path)
            .with_context(|| format!("could not reopen {}", archive_path.display()))?;
        let extracted = extract_members(
            reader,
            directory,
            &artifact.archive_root,
            members,
            RECOMMENDED_MEMBER_BYTES.min(artifact.max_bytes),
        )?;
        anyhow::ensure!(
            extracted.len() == members.len(),
            "{} archive is missing members: expected {:?}, found {:?}",
            artifact.asset,
            members,
            extracted
                .iter()
                .map(|path| path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned())
                .collect::<Vec<_>>()
        );

        let mut models = HashMap::new();
        let mut hashes = HashMap::new();
        for (stem, path) in members.iter().zip(extracted.iter()) {
            models.insert(stem.clone(), path.to_string_lossy().into_owned());
            hashes.insert(stem.clone(), sha256_file(path)?);
        }
        let descriptor = SpleeterDescriptor {
            format: SPLEETER_DESCRIPTOR_FORMAT.to_owned(),
            id: artifact.id.clone(),
            kind: artifact.kind.clone(),
            sample_rate: artifact.sample_rate,
            n_fft: SPLEETER_N_FFT,
            hop_length: SPLEETER_HOP_LENGTH,
            window: "hann-periodic".to_owned(),
            center: false,
            bins: SPLEETER_BINS,
            frames: SPLEETER_FRAMES,
            stems: vec!["acapella".to_owned(), "instrumental".to_owned()],
            models,
            sha256: hashes,
            installed_at_unix: unix_now(),
            license_summary: artifact.license_summary.clone(),
            notices: artifact.notices.clone(),
            source_url: artifact.url.clone(),
            source_sha256: archive_hash.to_owned(),
        };
        let descriptor_path = self.descriptor_path(&artifact.id);
        write_json_atomic(&descriptor_path, &serde_json::to_vec_pretty(&descriptor)?)?;
        Ok(descriptor_path)
    }

    /// Re-hashes an installed artifact to prove the local files still match what was downloaded.
    ///
    /// Kept as a support and troubleshooting entry point: the UI never re-hashes the ~75 MB graph
    /// set on a status refresh, so this is intentionally not part of the normal catalogue path.
    #[allow(dead_code)]
    pub(crate) fn verify_installed_artifact(&self, id: &str) -> Result<bool> {
        let descriptor_path = self.descriptor_path(id);
        let descriptor = read_spleeter_descriptor(&descriptor_path)?;
        for (stem, expected) in &descriptor.sha256 {
            let Some(path) = descriptor.models.get(stem) else {
                return Ok(false);
            };
            let actual = sha256_file(Path::new(path))?;
            if !actual.eq_ignore_ascii_case(expected) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub fn byom_records(&self) -> Vec<ByomModelRecord> {
        load_byom_store(&self.root)
            .map(|store| store.models)
            .unwrap_or_default()
    }

    pub fn import_byom(
        &self,
        path_text: &str,
        kind: &str,
        acknowledged: bool,
        display_name: Option<&str>,
    ) -> Result<(ByomModelRecord, ByomContractReport)> {
        let accepted = accepted_kind(kind).with_context(|| {
            format!("unknown BYOM kind {kind}; expected one of the documented contracts")
        })?;
        anyhow::ensure!(
            !accepted.requires_acknowledgement || acknowledged,
            "{}",
            accepted.rights_warning.clone().unwrap_or_else(|| {
                "this model kind needs an explicit rights acknowledgement".to_owned()
            })
        );
        anyhow::ensure!(
            !path_text.trim().is_empty(),
            "choose a local model file first"
        );
        anyhow::ensure!(path_text.len() <= 4_096, "model path is too long");
        let supplied = PathBuf::from(path_text.trim());
        let path = fs::canonicalize(&supplied)
            .with_context(|| format!("could not resolve {}", supplied.display()))?;
        anyhow::ensure!(path.is_file(), "model path is not a file");
        crate::import::ensure_local_path(&path)?;

        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        anyhow::ensure!(
            extension == "onnx" || extension == "json",
            "BYOM accepts .onnx graphs or .cvmodel.json manifests"
        );

        let report = if extension == "json" {
            inspect_descriptor_contract(&path, kind)?
        } else {
            inspect_onnx_contract(&path, kind)?
        };

        let sha256 = sha256_file(&path)?;
        let size_bytes = fs::metadata(&path)?.len();
        let file_name = path
            .file_name()
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_else(|| "model.onnx".to_owned());
        let display = display_name
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .unwrap_or(file_name.as_str())
            .to_owned();
        let record = ByomModelRecord {
            id: format!("byom-{}", &sha256[..16]),
            kind: kind.to_owned(),
            display_name: display,
            path: path.to_string_lossy().into_owned(),
            file_name,
            size_bytes,
            sha256,
            contract: report.contract.clone(),
            added_at_unix: unix_now(),
            acknowledgement: acknowledged,
            rights_warning: accepted.rights_warning.clone(),
        };

        let mut store = load_byom_store(&self.root)?;
        store.models.retain(|existing| existing.id != record.id);
        store.models.push(record.clone());
        save_byom_store(&self.root, &store)?;
        Ok((record, report))
    }

    pub fn remove_byom(&self, id: &str) -> Result<bool> {
        let mut store = load_byom_store(&self.root)?;
        let before = store.models.len();
        store.models.retain(|record| record.id != id);
        let removed = store.models.len() != before;
        if removed {
            save_byom_store(&self.root, &store)?;
        }
        Ok(removed)
    }

    /// Validates a local file *without* storing it, so the UI can preview a dropped model.
    pub fn validate_byom_file(&self, path_text: &str, kind: &str) -> Result<ByomContractReport> {
        accepted_kind(kind).with_context(|| format!("unknown BYOM kind {kind}"))?;
        let supplied = PathBuf::from(path_text.trim());
        let path = fs::canonicalize(&supplied)
            .with_context(|| format!("could not resolve {}", supplied.display()))?;
        anyhow::ensure!(path.is_file(), "model path is not a file");
        crate::import::ensure_local_path(&path)?;
        if path
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.eq_ignore_ascii_case("json"))
        {
            inspect_descriptor_contract(&path, kind)
        } else {
            inspect_onnx_contract(&path, kind)
        }
    }
}

fn accepted_kind(kind: &str) -> Option<&'static ByomAcceptedKind> {
    registry()
        .byom
        .accepted_kinds
        .iter()
        .find(|accepted| accepted.kind == kind)
}

/// Collects every forbidden source that the URL would resolve to.
pub(crate) fn forbidden_source_hits(url: &str) -> Vec<String> {
    let lowered = url.to_ascii_lowercase();
    registry()
        .policy
        .forbidden_download_sources
        .iter()
        .filter(|source| !source.is_empty() && lowered.contains(&source.to_ascii_lowercase()))
        .cloned()
        .collect()
}

fn trusted_download_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("github.com")
        || host.eq_ignore_ascii_case("objects.githubusercontent.com")
        || host.eq_ignore_ascii_case("release-assets.githubusercontent.com")
        || host.eq_ignore_ascii_case("codeload.github.com")
        || host.eq_ignore_ascii_case("huggingface.co")
        || host.eq_ignore_ascii_case("hf.co")
        || host.to_ascii_lowercase().ends_with(".huggingface.co")
}

/// Shape helpers kept free of ONNX Runtime so they can be unit tested on every platform.
pub(crate) fn axis_matches(actual: i64, expected: i64) -> bool {
    actual == expected || actual == -1
}

fn axis_dynamic(actual: i64) -> bool {
    actual <= 0
}

pub(crate) fn spleeter_input_shape_matches(shape: &[i64]) -> bool {
    shape.len() == 4
        && axis_matches(shape[0], 2)
        && (shape[1] == -1 || shape[1] >= 1)
        && axis_matches(shape[2], SPLEETER_FRAMES as i64)
        && axis_matches(shape[3], SPLEETER_BINS as i64)
}

pub(crate) fn spleeter_output_shape_matches(output: &[i64]) -> bool {
    output.len() == 4
        && axis_matches(output[0], 2)
        && axis_dynamic(output[1])
        && axis_matches(output[2], SPLEETER_FRAMES as i64)
        && axis_matches(output[3], SPLEETER_BINS as i64)
}

pub(crate) fn htdemucs_shape_matches(input: &[i64], output: &[i64]) -> bool {
    input.len() == 3
        && axis_matches(input[0], 1)
        && axis_matches(input[1], 2)
        && axis_matches(input[2], HTDEMUCS_WINDOW_SAMPLES as i64)
        && output.len() == 4
        && axis_matches(output[0], 1)
        && axis_matches(output[1], 4)
        && axis_matches(output[2], 2)
        && axis_matches(output[3], HTDEMUCS_WINDOW_SAMPLES as i64)
}

pub(crate) fn mdx_shape_matches(input: &[i64], output: &[i64]) -> bool {
    input.len() == 4
        && axis_matches(input[0], 1)
        && axis_matches(input[1], 4)
        && axis_matches(input[2], MDX_DIM_F as i64)
        && axis_matches(input[3], MDX_FRAMES as i64)
        && output.len() == 4
        && axis_matches(output[0], 1)
        && axis_matches(output[1], 4)
        && axis_matches(output[2], MDX_DIM_F as i64)
        && axis_matches(output[3], MDX_FRAMES as i64)
}

fn classify_shape_contract(input: &[i64], output: &[i64], kind: &str) -> &'static str {
    if kind == "spleeter-2stems" && spleeter_input_shape_matches(input) {
        return "spleeter-2stems";
    }
    if kind == "htdemucs" && htdemucs_shape_matches(input, output) {
        return "htdemucs";
    }
    if kind == "mdx-net" && mdx_shape_matches(input, output) {
        return "mdx-net";
    }
    "unknown"
}

fn tensor_shape(value_type: &ValueType, expected: TensorElementType) -> Result<Vec<i64>> {
    match value_type {
        ValueType::Tensor { ty, shape, .. } => {
            anyhow::ensure!(*ty == expected, "ONNX tensor has the wrong element type");
            Ok(shape.iter().copied().collect())
        }
        _ => anyhow::bail!("ONNX value must be a tensor"),
    }
}

fn local_session(path: &Path) -> Result<Session> {
    let mut builder = Session::builder()
        .map_err(|error| anyhow::anyhow!("could not initialize ONNX Runtime: {error}"))?
        .with_intra_threads(1)
        .map_err(|error| anyhow::anyhow!("could not configure ONNX Runtime: {error}"))?
        .with_inter_threads(1)
        .map_err(|error| anyhow::anyhow!("could not configure ONNX Runtime: {error}"))?
        .with_parallel_execution(false)
        .map_err(|error| anyhow::anyhow!("could not configure ONNX Runtime: {error}"))?;
    builder
        .commit_from_file(path)
        .map_err(|error| anyhow::anyhow!("could not load {}: {error}", path.display()))
}

fn inspect_onnx_contract(path: &Path, kind: &str) -> Result<ByomContractReport> {
    let session = local_session(path)?;
    anyhow::ensure!(
        session.inputs().len() == 1 && session.outputs().len() == 1,
        "BYOM separator graphs must have exactly one float32 input and one float32 output"
    );
    let input = session
        .inputs()
        .first()
        .context("separator ONNX has no input")?;
    let output = session
        .outputs()
        .first()
        .context("separator ONNX has no output")?;
    let input_shape = tensor_shape(input.dtype(), TensorElementType::Float32)
        .context("separator ONNX input must be float32")?;
    let output_shape = tensor_shape(output.dtype(), TensorElementType::Float32)
        .context("separator ONNX output must be float32")?;
    let contract = classify_shape_contract(&input_shape, &output_shape, kind);
    anyhow::ensure!(
        contract != "unknown",
        "the dropped graph does not match the {kind} tensor contract: input {input_shape:?}, output {output_shape:?}"
    );
    let accepted = accepted_kind(kind).context("unknown BYOM kind")?;
    Ok(ByomContractReport {
        valid: true,
        kind: kind.to_owned(),
        contract: accepted.contract.clone(),
        details: format!(
            "{} · input '{}' {:?} → output '{}' {:?}",
            contract,
            input.name(),
            input_shape,
            output.name(),
            output_shape
        ),
        path: path.to_string_lossy().into_owned(),
        input_names: vec![input.name().to_owned()],
        input_shapes: vec![input_shape],
        output_shapes: vec![output_shape],
    })
}

fn inspect_descriptor_contract(path: &Path, kind: &str) -> Result<ByomContractReport> {
    if kind == "mdx-net" {
        let manifest = crate::audio::model_manager::read_manifest(path)?;
        let directory = path.parent().unwrap_or_else(|| Path::new("."));
        let resolved =
            crate::audio::model_manager::validate_manifest_models(&manifest, directory, true)?;
        return Ok(ByomContractReport {
            valid: true,
            kind: kind.to_owned(),
            contract: format!("MDX-Net {} manifest", manifest.mode),
            details: format!(
                "{} verified ONNX target(s) with per-file SHA-256 pins",
                resolved.len()
            ),
            path: path.to_string_lossy().into_owned(),
            input_names: vec!["input".to_owned()],
            input_shapes: vec![vec![1, 4, MDX_DIM_F as i64, MDX_FRAMES as i64]],
            output_shapes: vec![vec![1, 4, MDX_DIM_F as i64, MDX_FRAMES as i64]],
        });
    }
    let bytes = fs::read(path)
        .with_context(|| format!("could not read model manifest {}", path.display()))?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_DESCRIPTOR_BYTES,
        "model manifest is too large"
    );
    let descriptor: SpleeterDescriptor = serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid ChampionVibe model manifest {}", path.display()))?;
    anyhow::ensure!(
        descriptor.format == SPLEETER_DESCRIPTOR_FORMAT,
        "only ChampionVibe Spleeter manifests are accepted here; MDX-Net sets are imported through the MDX importer"
    );
    anyhow::ensure!(
        kind == "spleeter-2stems",
        "Spleeter manifests belong to the spleeter-2stems kind"
    );
    for (stem, model_path) in &descriptor.models {
        let resolved = fs::canonicalize(model_path)
            .with_context(|| format!("manifest member {stem} could not be resolved"))?;
        crate::import::ensure_local_path(&resolved)?;
        anyhow::ensure!(
            resolved.is_file(),
            "manifest member {stem} is missing: {}",
            resolved.display()
        );
        if let Some(expected) = descriptor.sha256.get(stem) {
            let actual = sha256_file(&resolved)?;
            anyhow::ensure!(
                actual.eq_ignore_ascii_case(expected),
                "manifest member {stem} does not match its recorded SHA-256"
            );
        }
    }
    Ok(ByomContractReport {
        valid: true,
        kind: kind.to_owned(),
        contract:
            "two Spleeter U-Net graphs (vocals + accompaniment), float32 [2, splits, 512, 1024]"
                .to_owned(),
        details: format!(
            "descriptor {} with {} graph(s) and per-file SHA-256 verification",
            descriptor.id,
            descriptor.models.len()
        ),
        path: path.to_string_lossy().into_owned(),
        input_names: vec!["x".to_owned()],
        input_shapes: vec![vec![2, -1, SPLEETER_FRAMES as i64, SPLEETER_BINS as i64]],
        output_shapes: vec![vec![2, -1, SPLEETER_FRAMES as i64, SPLEETER_BINS as i64]],
    })
}

pub(crate) fn read_spleeter_descriptor(path: &Path) -> Result<SpleeterDescriptor> {
    let bytes = fs::read(path)
        .with_context(|| format!("could not read model manifest {}", path.display()))?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_DESCRIPTOR_BYTES,
        "model manifest is too large"
    );
    let descriptor: SpleeterDescriptor = serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid ChampionVibe model manifest {}", path.display()))?;
    anyhow::ensure!(
        descriptor.format == SPLEETER_DESCRIPTOR_FORMAT,
        "unsupported ChampionVibe separator manifest format"
    );
    Ok(descriptor)
}

fn load_byom_store(root: &Path) -> Result<ByomStore> {
    let path = root.join(BYOM_STORE_FILE);
    if !path.is_file() {
        return Ok(ByomStore {
            format: BYOM_STORE_FORMAT.to_owned(),
            models: Vec::new(),
        });
    }
    let bytes = fs::read(&path).with_context(|| format!("could not read {}", path.display()))?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_DESCRIPTOR_BYTES,
        "BYOM store is too large"
    );
    let store: ByomStore = serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid BYOM store {}", path.display()))?;
    anyhow::ensure!(
        store.format == BYOM_STORE_FORMAT,
        "unsupported BYOM store format"
    );
    Ok(store)
}

fn save_byom_store(root: &Path, store: &ByomStore) -> Result<()> {
    let path = root.join(BYOM_STORE_FILE);
    write_json_atomic(&path, &serde_json::to_vec_pretty(store)?)
}

fn write_json_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
    fs::write(&temporary, bytes)
        .with_context(|| format!("could not write {}", temporary.display()))?;
    if path.exists() {
        fs::remove_file(path).with_context(|| format!("could not replace {}", path.display()))?;
    }
    fs::rename(&temporary, path)
        .with_context(|| format!("could not install {}", path.display()))?;
    Ok(())
}

/// Extracts the listed members of a `.tar.bz2` archive, rejecting traversal and unexpected files.
pub(crate) fn extract_members<R: Read>(
    reader: R,
    destination: &Path,
    archive_root: &str,
    members: &[String],
    max_bytes: u64,
) -> Result<Vec<PathBuf>> {
    fs::create_dir_all(destination)
        .with_context(|| format!("could not create {}", destination.display()))?;
    let decoder = bzip2::read::BzDecoder::new(reader);
    let mut archive = tar::Archive::new(decoder);
    let mut extracted: Vec<PathBuf> = Vec::new();
    let mut total = 0_u64;
    for entry in archive
        .entries()
        .context("model archive is not a readable tar.bz2")?
    {
        let entry = entry.context("model archive contains an unreadable entry")?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry
            .path()
            .context("model archive contains an invalid path")?
            .into_owned();
        if path
            .components()
            .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
        {
            continue;
        }
        let Some(file_name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if !members.iter().any(|member| member == file_name) {
            continue;
        }
        if let Some(parent) = path
            .parent()
            .and_then(|value| value.file_name())
            .and_then(|value| value.to_str())
        {
            if !archive_root.is_empty() && !parent.eq_ignore_ascii_case(archive_root) {
                continue;
            }
        }
        let target = destination.join(file_name);
        let mut file = File::create(&target)
            .with_context(|| format!("could not create {}", target.display()))?;
        let remaining = max_bytes.saturating_sub(total).max(1);
        let copied = std::io::copy(&mut entry.take(remaining), &mut file)
            .with_context(|| format!("could not extract {}", file_name))?;
        total = total.saturating_add(copied);
        anyhow::ensure!(
            total <= max_bytes,
            "model archive expanded beyond its {} byte limit",
            max_bytes
        );
        if !extracted.iter().any(|existing| existing == &target) {
            extracted.push(target);
        }
    }
    // Return paths in the caller's member order so callers can zip names and files.
    let mut ordered = Vec::with_capacity(members.len());
    for member in members {
        if let Some(found) = extracted
            .iter()
            .find(|path| path.file_name().and_then(|value| value.to_str()) == Some(member.as_str()))
        {
            ordered.push(found.clone());
        }
    }
    Ok(ordered)
}

pub(crate) fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)
        .with_context(|| format!("could not read {} for SHA-256 verification", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn temp_dir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "championvibe-model-loader-{label}-{}-{}",
            std::process::id(),
            unix_now()
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    #[test]
    fn registry_is_cleared_pinned_and_free_of_uncleared_sources() {
        let registry = registry();
        assert_eq!(registry.format, REGISTRY_FORMAT);
        assert!(registry.policy.cleared_tier_only);
        assert!(
            registry
                .artifacts
                .iter()
                .any(|artifact| artifact.id == registry.policy.default_separator_id),
            "the default separator must exist in the cleared registry"
        );
        assert!(!registry.artifacts.is_empty());
        for artifact in &registry.artifacts {
            assert_eq!(artifact.tier, "cleared", "{} is not cleared", artifact.id);
            assert!(
                is_pinned(artifact),
                "{} must carry a verified 64-hex SHA-256 pin, found {}",
                artifact.id,
                artifact.sha256
            );
            assert!(artifact.size_bytes > 0, "{} has no byte size", artifact.id);
            assert!(
                artifact.max_bytes >= artifact.size_bytes,
                "{} byte limit is below its own size",
                artifact.id
            );
            assert!(
                artifact.url.starts_with("https://"),
                "{} must download over HTTPS",
                artifact.id
            );
            assert!(
                forbidden_source_hits(&artifact.url).is_empty(),
                "{} resolves to an uncleared source",
                artifact.id
            );
            assert!(
                artifact
                    .weights_license
                    .to_ascii_lowercase()
                    .contains("mit"),
                "{} must declare MIT weights to stay in the cleared tier",
                artifact.id
            );
            assert!(
                !artifact.notices.is_empty(),
                "{} has no notices",
                artifact.id
            );
            assert_eq!(artifact.archive, "tar.bz2");
            assert_eq!(artifact.members.len(), 2);
        }
    }

    #[test]
    fn pending_pins_are_rejected_instead_of_trusted() {
        let mut artifact = registry().artifacts[0].clone();
        artifact.sha256 = PIN_PENDING.to_owned();
        assert!(!is_pinned(&artifact));
    }

    #[test]
    fn forbidden_sources_are_detected_case_insensitively() {
        assert!(forbidden_source_hits(
            "https://github.com/k2-fsa/sherpa-onnx/releases/download/source-separation-models/sherpa-onnx-spleeter-2stems.tar.bz2"
        )
        .is_empty());
        assert!(!forbidden_source_hits(
            "https://huggingface.co/facebookresearch/demucs/resolve/main/htdemucs.onnx"
        )
        .is_empty());
        assert!(!forbidden_source_hits(
            "https://github.com/Politrees/UVR_resources/releases/download/models/kuielab_a_vocals.onnx"
        )
        .is_empty());
    }

    #[test]
    fn tensor_contracts_accept_dynamic_axes_only_where_documented() {
        assert!(spleeter_input_shape_matches(&[2, -1, 512, 1024]));
        assert!(spleeter_input_shape_matches(&[2, 1, 512, 1024]));
        assert!(!spleeter_input_shape_matches(&[1, 1, 512, 1024]));
        assert!(!spleeter_input_shape_matches(&[2, 1, 1024, 512]));
        assert!(spleeter_output_shape_matches(&[2, -1, 512, 1024]));

        assert!(htdemucs_shape_matches(
            &[1, 2, 343_980],
            &[1, 4, 2, 343_980]
        ));
        assert!(!htdemucs_shape_matches(&[1, 2, 343_980], &[1, 4, 2, 1]));

        assert!(mdx_shape_matches(&[1, 4, 2048, 512], &[1, 4, 2048, 512]));
        assert!(!mdx_shape_matches(&[1, 4, 2048, 512], &[1, 2, 2048, 512]));

        assert_eq!(
            classify_shape_contract(&[2, -1, 512, 1024], &[2, -1, 512, 1024], "spleeter-2stems"),
            "spleeter-2stems"
        );
        assert_eq!(
            classify_shape_contract(&[1, 2, 343_980], &[1, 4, 2, 343_980], "htdemucs"),
            "htdemucs"
        );
        assert_eq!(
            classify_shape_contract(&[1, 2, 343_980], &[1, 4, 2, 343_980], "spleeter-2stems"),
            "unknown"
        );
    }

    #[test]
    fn byom_store_round_trips_and_never_stores_weights_in_the_repo() {
        let root = temp_dir("byom");
        let record = ByomModelRecord {
            id: "byom-0123456789abcdef".to_owned(),
            kind: "htdemucs".to_owned(),
            display_name: "my-htdemucs".to_owned(),
            path: "/home/user/Downloads/htdemucs.onnx".to_owned(),
            file_name: "htdemucs.onnx".to_owned(),
            size_bytes: 42,
            sha256: "0".repeat(64),
            contract: "float32 [1,2,343980] → [1,4,2,343980]".to_owned(),
            added_at_unix: 1,
            acknowledgement: true,
            rights_warning: None,
        };
        let store = ByomStore {
            format: BYOM_STORE_FORMAT.to_owned(),
            models: vec![record.clone()],
        };
        save_byom_store(&root, &store).expect("save store");
        let loaded = load_byom_store(&root).expect("load store");
        assert_eq!(loaded.models.len(), 1);
        assert_eq!(loaded.models[0].path, record.path);
        assert_eq!(loaded.models[0].kind, record.kind);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn tarbz2_extraction_keeps_only_declared_members() {
        let root = temp_dir("extract");
        let mut archive_bytes = Vec::new();
        {
            let encoder =
                bzip2::write::BzEncoder::new(&mut archive_bytes, bzip2::Compression::best());
            let mut builder = tar::Builder::new(encoder);
            let mut header = tar::Header::new_gnu();
            for (name, payload) in [
                (
                    "sherpa-onnx-spleeter-2stems/vocals.onnx",
                    b"vocals".to_vec(),
                ),
                (
                    "sherpa-onnx-spleeter-2stems/accompaniment.onnx",
                    b"accompaniment".to_vec(),
                ),
                (
                    "sherpa-onnx-spleeter-2stems/readme.txt",
                    b"ignored".to_vec(),
                ),
            ] {
                header.set_size(payload.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder
                    .append_data(&mut header, name, Cursor::new(payload))
                    .expect("append member");
            }
            let encoder = builder.into_inner().expect("finish tar");
            encoder.finish().expect("finish bzip2");
        }

        let members = vec!["vocals.onnx".to_owned(), "accompaniment.onnx".to_owned()];
        let extracted = extract_members(
            Cursor::new(archive_bytes),
            &root,
            "sherpa-onnx-spleeter-2stems",
            &members,
            1024 * 1024,
        )
        .expect("extract members");
        assert_eq!(extracted.len(), 2);
        assert_eq!(
            extracted[0].file_name().and_then(|value| value.to_str()),
            Some("vocals.onnx")
        );
        assert!(!root.join("readme.txt").exists());
        assert_eq!(
            fs::read(&extracted[1]).expect("read member"),
            b"accompaniment"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn descriptor_round_trip_records_local_paths_and_hashes() {
        let root = temp_dir("descriptor");
        let descriptor = SpleeterDescriptor {
            format: SPLEETER_DESCRIPTOR_FORMAT.to_owned(),
            id: "spleeter-2stems-onnx".to_owned(),
            kind: "spleeter-2stems".to_owned(),
            sample_rate: SPLEETER_SAMPLE_RATE,
            n_fft: SPLEETER_N_FFT,
            hop_length: SPLEETER_HOP_LENGTH,
            window: "hann-periodic".to_owned(),
            center: false,
            bins: SPLEETER_BINS,
            frames: SPLEETER_FRAMES,
            stems: vec!["acapella".to_owned(), "instrumental".to_owned()],
            models: HashMap::from([
                ("vocals".to_owned(), "/tmp/vocals.onnx".to_owned()),
                (
                    "accompaniment".to_owned(),
                    "/tmp/accompaniment.onnx".to_owned(),
                ),
            ]),
            sha256: HashMap::new(),
            installed_at_unix: 7,
            license_summary: "MIT + Apache-2.0".to_owned(),
            notices: vec!["THIRD-PARTY-NOTICES.md#deezer-spleeter-mit".to_owned()],
            source_url: "https://example.invalid/model.tar.bz2".to_owned(),
            source_sha256: "a".repeat(64),
        };
        let path = root.join("spleeter-2stems.cvmodel.json");
        write_json_atomic(
            &path,
            &serde_json::to_vec_pretty(&descriptor).expect("serialize"),
        )
        .expect("write descriptor");
        let loaded = read_spleeter_descriptor(&path).expect("read descriptor");
        assert_eq!(loaded.frames, SPLEETER_FRAMES);
        assert_eq!(loaded.bins, SPLEETER_BINS);
        assert_eq!(loaded.models.len(), 2);
        assert_eq!(loaded.sample_rate, 44_100);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn byom_acknowledgement_is_required_for_unclear_rights_kinds() {
        let demucs = accepted_kind("htdemucs").expect("htdemucs kind is documented");
        assert!(demucs.requires_acknowledgement);
        assert!(demucs.rights_warning.is_some());
        let spleeter = accepted_kind("spleeter-2stems").expect("spleeter kind is documented");
        assert!(!spleeter.requires_acknowledgement);
        assert!(accepted_kind("not-a-model").is_none());
    }
}
