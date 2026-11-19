//! On-demand local model registry.
//!
//! Every network artifact has a fixed, immutable Hugging Face revision and a hard-coded SHA-256
//! pin. Downloads are written to a temporary file, length-limited, hashed, structurally checked,
//! and only then atomically installed. User-selected files are never treated as trusted downloads;
//! their format/tensor contract is checked separately before use.

use anyhow::{Context, Result};
use ort::{
    session::Session,
    value::{TensorElementType, ValueType},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
use whisper_rs::{WhisperContext, WhisperContextParameters};

const WHISPER_REVISION: &str = "80da2d8bfee42b0e836fc3a9890373e5defc00a6";
const KOKORO_REVISION: &str = "468588286ebb2dd77c25b9771e5d165896538cce";
const MANIFEST_FORMAT: &str = "championvibe-mdxnet-v1";
const MDX_DIM_F: usize = 2_048;
const MDX_FRAMES: usize = 512;
const MDX_HOP_LENGTH: usize = 1_024;
const DEFAULT_COMPENSATION: f32 = 1.035;
const MAX_ONNX_BYTES: u64 = 512 * 1024 * 1024;
const MAX_WHISPER_BYTES: u64 = 256 * 1024 * 1024;
const MAX_VOICE_BYTES: u64 = 16 * 1024 * 1024;
static TEMP_FILE_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedModelInfo {
    pub id: String,
    pub title: String,
    pub purpose: String,
    pub approximate_size: String,
    pub license: String,
    pub source_url: String,
    pub sha256: String,
    pub installed: bool,
    pub installed_path: Option<String>,
    pub rights_note: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelValidation {
    pub valid: bool,
    pub kind: String,
    pub contract: String,
    pub details: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SeparatorModelManifest {
    pub format: String,
    /// `two-stem` yields vocals + an input-minus-vocals instrumental on Android.
    /// `four-stem` yields vocals, other/instrumental, bass, and drums on Windows.
    pub mode: String,
    pub models: Vec<SeparatorModelEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SeparatorModelEntry {
    pub stem: String,
    /// A safe path relative to the manifest directory.
    pub file: String,
    /// Hash of an installed managed/user-imported artifact. None is only accepted for legacy
    /// manifests created by older development builds; new manifests always include the pin.
    pub sha256: Option<String>,
    pub n_fft: usize,
    pub dim_f: usize,
    pub frames: usize,
    pub hop_length: usize,
    pub compensation: f32,
}

#[derive(Debug, Clone, Copy)]
enum ArtifactKind {
    WhisperTiny,
    KokoroOnnx,
    KokoroVoice,
}

#[derive(Debug, Clone, Copy)]
struct Artifact {
    filename: &'static str,
    repository: &'static str,
    revision: &'static str,
    repository_path: &'static str,
    sha256: &'static str,
    max_bytes: u64,
    validator: ArtifactKind,
}

const WHISPER_TINY: Artifact = Artifact {
    filename: "ggml-tiny.bin",
    repository: "ggerganov/whisper.cpp",
    revision: WHISPER_REVISION,
    repository_path: "ggml-tiny.bin",
    sha256: "be07e048e1e599ad46341c8d2a135645097a538221678b7acdd1b1919c6e1b21",
    max_bytes: MAX_WHISPER_BYTES,
    validator: ArtifactKind::WhisperTiny,
};
const KOKORO_MODEL: Artifact = Artifact {
    filename: "kokoro-model-quantized.onnx",
    repository: "onnx-community/Kokoro-82M-v1.0-ONNX",
    revision: KOKORO_REVISION,
    repository_path: "onnx/model_quantized.onnx",
    sha256: "fbae9257e1e05ffc727e951ef9b9c98418e6d79f1c9b6b13bd59f5c9028a1478",
    max_bytes: MAX_ONNX_BYTES,
    validator: ArtifactKind::KokoroOnnx,
};
const KOKORO_VOICE: Artifact = Artifact {
    filename: "kokoro-af-heart.bin",
    repository: "onnx-community/Kokoro-82M-v1.0-ONNX",
    revision: KOKORO_REVISION,
    repository_path: "voices/af_heart.bin",
    sha256: "d583ccff3cdca2f7fae535cb998ac07e9fcb90f09737b9a41fa2734ec44a8f0b",
    max_bytes: MAX_VOICE_BYTES,
    validator: ArtifactKind::KokoroVoice,
};

/// Cleared, non-separator artifacts only. Separator weights are never listed here: the default
/// separator comes from the pinned Spleeter registry in [`super::model_loader`], and every other
/// separator is user-supplied through BYOM.
const ALL_ARTIFACTS: &[Artifact] = &[WHISPER_TINY, KOKORO_MODEL, KOKORO_VOICE];

pub struct ModelManager {
    root: PathBuf,
    client: reqwest::Client,
}

impl ModelManager {
    pub fn new(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(&root).with_context(|| {
            format!(
                "could not create managed model directory {}",
                root.display()
            )
        })?;
        let client = reqwest::Client::builder()
            .user_agent("ChampionVibe/0.1 local-model-manager")
            .timeout(std::time::Duration::from_secs(30 * 60))
            .build()
            .context("could not configure the local model downloader")?;
        Ok(Self { root, client })
    }

    pub fn catalog(&self) -> Vec<ManagedModelInfo> {
        vec![
            self.managed_info(
                "whisper-tiny",
                "Whisper Tiny",
                "Local transcription · English/multilingual tiny",
                "77.7 MB",
                "MIT",
                &WHISPER_TINY,
                "OpenAI Whisper and whisper.cpp source repositories declare MIT; retain their notices in redistributed builds.",
            ),
            self.managed_info(
                "kokoro-model",
                "US English AI voice with Patois pronunciation approximation (Kokoro v1.0, INT8)",
                "Local CPU text-to-speech · 24 kHz · not an authentic Patois voice",
                "92.4 MB",
                "Apache-2.0 declared by the pinned model repository",
                &KOKORO_MODEL,
                "The pinned model repository declares Apache-2.0. The app downloads on demand and does not bundle the weights. The voice speaks US English; the Patois spelling rules only approximate pronunciation.",
            ),
            self.managed_info(
                "kokoro-voice-af-heart",
                "Kokoro voice af_heart (US English)",
                "Voice pack · female American English · not Jamaican Patois",
                "522 kB",
                "Apache-2.0 declared by the pinned model repository",
                &KOKORO_VOICE,
                "The repository declares Apache-2.0; speaker-data provenance should still be reviewed before commercial redistribution.",
            ),
        ]
    }

    #[allow(clippy::too_many_arguments)]
    fn managed_info(
        &self,
        id: &str,
        title: &str,
        purpose: &str,
        approximate_size: &str,
        license: &str,
        artifact: &Artifact,
        rights_note: &str,
    ) -> ManagedModelInfo {
        let installed_path = self.root.join(artifact.filename);
        let installed = installed_path.is_file();
        let installed_text = installed.then(|| installed_path.to_string_lossy().into_owned());
        ManagedModelInfo {
            id: id.to_owned(),
            title: title.to_owned(),
            purpose: purpose.to_owned(),
            approximate_size: approximate_size.to_owned(),
            license: license.to_owned(),
            source_url: artifact_url(artifact),
            sha256: artifact.sha256.to_owned(),
            installed,
            installed_path: installed_text,
            rights_note: rights_note.to_owned(),
        }
    }

    /// Returns a locally installed, pinned Kokoro artifact as raw bytes for the WebView's
    /// on-device ONNX Runtime Web session. This response is sent through Tauri's binary IPC path,
    /// never JSON number arrays.
    pub fn read_kokoro_asset(&self, asset: &str) -> Result<Vec<u8>> {
        let artifact = match asset {
            "model" => &KOKORO_MODEL,
            "voice" => &KOKORO_VOICE,
            _ => anyhow::bail!("unknown Kokoro asset; expected model or voice"),
        };
        let path = self.root.join(artifact.filename);
        anyhow::ensure!(
            path.is_file(),
            "download and validate the Kokoro {asset} before loading WASM TTS"
        );
        let size = fs::metadata(&path)?.len();
        anyhow::ensure!(
            size > 0 && size <= artifact.max_bytes,
            "Kokoro {asset} exceeds its pinned size limit"
        );
        let actual = sha256_file(&path)?;
        anyhow::ensure!(
            actual.eq_ignore_ascii_case(artifact.sha256),
            "Kokoro {asset} failed its SHA-256 integrity check"
        );
        validate_artifact(artifact, &path)?;
        fs::read(&path).with_context(|| format!("could not read local Kokoro {asset}"))
    }

    pub async fn download(&self, id: &str) -> Result<PathBuf> {
        match id {
            "whisper-tiny" => self.download_artifact(&WHISPER_TINY).await,
            "kokoro-model" => self.download_artifact(&KOKORO_MODEL).await,
            "kokoro-voice-af-heart" => self.download_artifact(&KOKORO_VOICE).await,
            other => anyhow::bail!(
                "unknown managed model id {other}; cleaned separator weights must be installed through the Spleeter loader and everything else through BYOM"
            ),
        }
    }

    async fn download_artifact(&self, artifact: &Artifact) -> Result<PathBuf> {
        fs::create_dir_all(&self.root)?;
        let final_path = self.root.join(artifact.filename);
        if final_path.is_file() && sha256_file(&final_path).ok().as_deref() == Some(artifact.sha256)
        {
            validate_artifact(artifact, &final_path)?;
            return Ok(final_path);
        }

        let sequence = TEMP_FILE_ID.fetch_add(1, Ordering::Relaxed);
        let temporary = self.root.join(format!(
            ".{}.{}.{}.part",
            artifact.filename,
            std::process::id(),
            sequence
        ));
        let result = self
            .download_to_file(artifact, &temporary)
            .await
            .and_then(|()| {
                let actual_hash = sha256_file(&temporary)?;
                anyhow::ensure!(
                    actual_hash.eq_ignore_ascii_case(artifact.sha256),
                    "SHA-256 mismatch for {}: expected {}, got {}",
                    artifact.filename,
                    artifact.sha256,
                    actual_hash
                );
                validate_artifact(artifact, &temporary)?;
                if final_path.exists() {
                    fs::remove_file(&final_path).with_context(|| {
                        format!(
                            "could not replace invalid managed model {}",
                            final_path.display()
                        )
                    })?;
                }
                fs::rename(&temporary, &final_path).with_context(|| {
                    format!(
                        "could not install verified model at {}",
                        final_path.display()
                    )
                })?;
                Ok(())
            });
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result?;
        Ok(final_path)
    }

    async fn download_to_file(&self, artifact: &Artifact, path: &Path) -> Result<()> {
        let url = artifact_url(artifact);
        let mut response = self
            .client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("could not download {}", artifact.filename))?
            .error_for_status()
            .with_context(|| format!("model source returned an error for {}", artifact.filename))?;
        let final_url = response.url();
        anyhow::ensure!(
            final_url.scheme() == "https"
                && trusted_huggingface_host(final_url.host_str().unwrap_or("")),
            "model download redirected to an untrusted host"
        );
        if let Some(content_length) = response.content_length() {
            anyhow::ensure!(
                content_length > 0 && content_length <= artifact.max_bytes,
                "model download exceeds the size limit for {}",
                artifact.filename
            );
        }
        let mut file = File::create(path)
            .with_context(|| format!("could not create temporary model file {}", path.display()))?;
        let mut downloaded = 0_u64;
        while let Some(chunk) = response
            .chunk()
            .await
            .with_context(|| format!("download stream failed for {}", artifact.filename))?
        {
            downloaded = downloaded.saturating_add(chunk.len() as u64);
            anyhow::ensure!(
                downloaded <= artifact.max_bytes,
                "model download exceeded the size limit for {}",
                artifact.filename
            );
            file.write_all(&chunk).with_context(|| {
                format!("could not write downloaded model {}", artifact.filename)
            })?;
        }
        anyhow::ensure!(downloaded > 0, "model source returned an empty file");
        file.sync_all().with_context(|| {
            format!("could not finalize downloaded model {}", artifact.filename)
        })?;
        Ok(())
    }

    /// Checks any manifest SHA pins before a separator or TTS model is handed to an inference path.
    pub fn verify_path_if_managed(&self, path: &Path) -> Result<()> {
        let canonical = fs::canonicalize(path)
            .with_context(|| format!("could not resolve model path {}", path.display()))?;
        let is_manifest = canonical
            .file_name()
            .and_then(|value| value.to_str())
            .is_some_and(|name| name.to_ascii_lowercase().ends_with(".cvmodel.json"));
        if is_manifest {
            // Spleeter descriptors carry per-file hashes and are verified by
            // [`super::model_loader`]; only MDX-Net target sets have the MDX manifest contract.
            if manifest_format_of(&canonical).as_deref() != Some(MANIFEST_FORMAT) {
                return Ok(());
            }
            let manifest = read_manifest(&canonical)?;
            validate_manifest_models(&manifest, canonical.parent().unwrap_or(&self.root), true)?;
            return Ok(());
        }
        for artifact in ALL_ARTIFACTS {
            let managed = self.root.join(artifact.filename);
            if fs::canonicalize(&managed).ok().as_deref() == Some(canonical.as_path()) {
                let actual = sha256_file(&canonical)?;
                anyhow::ensure!(
                    actual.eq_ignore_ascii_case(artifact.sha256),
                    "managed model {} failed its SHA-256 integrity check",
                    artifact.filename
                );
                validate_artifact(artifact, &canonical)?;
                return Ok(());
            }
        }
        // User-imported models receive structural validation but have no upstream pin.
        Ok(())
    }

    pub fn validate_user_file(&self, path_text: &str, kind: &str) -> Result<ModelValidation> {
        anyhow::ensure!(
            !path_text.trim().is_empty(),
            "choose a local model file first"
        );
        anyhow::ensure!(path_text.len() <= 4_096, "model path is too long");
        let supplied = PathBuf::from(path_text.trim());
        let path = fs::canonicalize(&supplied)
            .with_context(|| format!("could not resolve model file {}", supplied.display()))?;
        anyhow::ensure!(path.is_file(), "model path is not a file");
        crate::import::ensure_local_path(&path)?;
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default();

        let (contract, details) = match kind {
            "separator" => {
                if extension.eq_ignore_ascii_case("json") {
                    let manifest = read_manifest(&path)?;
                    validate_manifest_models(
                        &manifest,
                        path.parent().unwrap_or(&self.root),
                        false,
                    )?;
                    (
                        format!("MDX-Net {}", manifest.mode),
                        "ONNX tensor shapes, dtypes, preprocessing parameters, and local files validated".to_owned(),
                    )
                } else {
                    anyhow::ensure!(
                        extension.eq_ignore_ascii_case("onnx"),
                        "separator models must be .onnx or a ChampionVibe .cvmodel.json manifest"
                    );
                    let signature = inspect_mdx_signature(&path)?;
                    (
                        "MDX-Net single-target ONNX".to_owned(),
                        format!(
                            "float32 [1, 4, {}, {}] → matching output; target-specific n_fft and stem identity must be assigned in a model manifest before import",
                            signature.dim_f,
                            signature.frames
                        ),
                    )
                }
            }
            "whisper" => {
                anyhow::ensure!(
                    extension.eq_ignore_ascii_case("bin"),
                    "Whisper Tiny models must be .bin files"
                );
                let model_type = validate_whisper_model(&path)?;
                (
                    "whisper.cpp GGML model".to_owned(),
                    format!("model header and tensor graph load successfully ({model_type})"),
                )
            }
            "kokoro-model" => {
                anyhow::ensure!(
                    extension.eq_ignore_ascii_case("onnx"),
                    "Kokoro model must be an .onnx file"
                );
                validate_kokoro_onnx(&path)?;
                (
                    "Kokoro v1.0 ONNX".to_owned(),
                    "input_ids int64, style float32 [1, 256], speed float32, and audio float32 output validated".to_owned(),
                )
            }
            "kokoro-voice" => {
                anyhow::ensure!(
                    extension.eq_ignore_ascii_case("bin"),
                    "Kokoro voices must be raw .bin style-vector packs"
                );
                let rows = validate_kokoro_voice(&path)?;
                (
                    "Kokoro voice-vector pack".to_owned(),
                    format!("{rows} finite 256-float style vectors validated"),
                )
            }
            _ => anyhow::bail!("unknown model contract"),
        };
        self.verify_path_if_managed(&path)?;
        Ok(ModelValidation {
            valid: true,
            kind: kind.to_owned(),
            contract,
            details,
            path: path.to_string_lossy().into_owned(),
        })
    }

    /// Copies user-selected MDX-Net files into the managed model folder only after checking every
    /// ONNX signature. Android accepts one vocal target; Windows requires named vocals/other/bass/
    /// drums models so the four output lanes remain aligned and semantically clear.
    pub fn import_user_separator_models(&self, paths: &[String], mode: &str) -> Result<PathBuf> {
        let expected_count = match mode {
            "two-stem" => 1,
            "four-stem" => 4,
            _ => anyhow::bail!("separator import mode must be two-stem or four-stem"),
        };
        anyhow::ensure!(
            paths.len() == expected_count,
            "{mode} model import requires exactly {expected_count} ONNX file(s)"
        );
        let mut sources = Vec::with_capacity(paths.len());
        let mut seen = std::collections::HashSet::new();
        for path_text in paths {
            let source = fs::canonicalize(path_text.trim()).with_context(|| {
                format!("could not resolve custom separator model {}", path_text)
            })?;
            crate::import::ensure_local_path(&source)?;
            anyhow::ensure!(source.is_file(), "custom separator model is not a file");
            anyhow::ensure!(
                source
                    .extension()
                    .and_then(|value| value.to_str())
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("onnx")),
                "custom separator files must use .onnx"
            );
            let stem = if mode == "two-stem" {
                "vocals".to_owned()
            } else {
                infer_stem_name(&source)?
            };
            anyhow::ensure!(
                seen.insert(stem.clone()),
                "custom model set contains duplicate {stem} models"
            );
            let signature = inspect_mdx_signature(&source)?;
            sources.push((stem, source, signature));
        }
        if mode == "four-stem" {
            for stem in ["vocals", "other", "bass", "drums"] {
                anyhow::ensure!(
                    seen.contains(stem),
                    "custom four-stem set is missing a file whose name identifies {stem}"
                );
            }
        }
        let frames = sources
            .first()
            .map(|(_, _, signature)| signature.frames)
            .unwrap_or(MDX_FRAMES);
        let shared_dimensions = sources
            .iter()
            .all(|(_, _, signature)| signature.frames == frames && signature.dim_f == MDX_DIM_F);
        anyhow::ensure!(
            shared_dimensions,
            "all custom MDX-Net models must share float32 [1, 4, 2048, T] tensor dimensions"
        );
        let mut hashes = sources
            .iter()
            .map(|(stem, source, _)| Ok((stem.clone(), sha256_file(source)?)))
            .collect::<Result<Vec<_>>>()?;
        hashes.sort_by(|left, right| left.0.cmp(&right.0));
        let bundle_hash = sha256_bytes(
            hashes
                .iter()
                .flat_map(|(stem, hash)| format!("{stem}:{hash}\n").into_bytes())
                .collect::<Vec<_>>(),
        );
        let bundle_dir = self.root.join("user-separators").join(&bundle_hash[..20]);
        fs::create_dir_all(&bundle_dir).with_context(|| {
            format!(
                "could not create custom model folder {}",
                bundle_dir.display()
            )
        })?;

        let mut entries = Vec::with_capacity(sources.len());
        for (stem, source, _) in sources {
            let filename = format!("{stem}.onnx");
            let destination = bundle_dir.join(&filename);
            let source_hash = sha256_file(&source)?;
            if !destination.is_file()
                || sha256_file(&destination).ok().as_deref() != Some(source_hash.as_str())
            {
                fs::copy(&source, &destination).with_context(|| {
                    format!("could not copy custom {stem} model into the local model library")
                })?;
            }
            let n_fft = match stem.as_str() {
                "vocals" => 6_144,
                "other" => 8_192,
                "bass" => 16_384,
                "drums" => 4_096,
                _ => unreachable!("validated separator stem"),
            };
            entries.push(SeparatorModelEntry {
                stem,
                file: filename,
                sha256: Some(source_hash),
                n_fft,
                dim_f: MDX_DIM_F,
                frames,
                hop_length: MDX_HOP_LENGTH,
                compensation: DEFAULT_COMPENSATION,
            });
        }
        entries.sort_by_key(|entry| match entry.stem.as_str() {
            "vocals" => 0,
            "other" => 1,
            "bass" => 2,
            "drums" => 3,
            _ => 4,
        });
        let manifest = SeparatorModelManifest {
            format: MANIFEST_FORMAT.to_owned(),
            mode: mode.to_owned(),
            models: entries,
        };
        validate_manifest_models(&manifest, &bundle_dir, true)?;
        let manifest_path = bundle_dir.join("model.cvmodel.json");
        write_manifest(&manifest_path, &manifest)?;
        Ok(manifest_path)
    }
}

/// Reads just the `format` field so both manifest families can share the `.cvmodel.json` suffix.
pub(crate) fn manifest_format_of(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    if bytes.len() > 64 * 1024 {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value
        .get("format")
        .and_then(|format| format.as_str())
        .map(|format| format.to_owned())
}

pub(crate) fn read_manifest(path: &Path) -> Result<SeparatorModelManifest> {
    let bytes = fs::read(path)
        .with_context(|| format!("could not read model manifest {}", path.display()))?;
    anyhow::ensure!(
        bytes.len() <= 64 * 1024,
        "separator model manifest is too large"
    );
    let manifest = serde_json::from_slice::<SeparatorModelManifest>(&bytes)
        .with_context(|| format!("invalid ChampionVibe model manifest {}", path.display()))?;
    anyhow::ensure!(
        manifest.format == MANIFEST_FORMAT,
        "unsupported separator model manifest format"
    );
    anyhow::ensure!(
        matches!(manifest.mode.as_str(), "two-stem" | "four-stem"),
        "unsupported separator model profile"
    );
    Ok(manifest)
}

pub(crate) fn resolve_manifest_models(
    manifest: &SeparatorModelManifest,
    manifest_directory: &Path,
) -> Result<Vec<(SeparatorModelEntry, PathBuf)>> {
    let expected_stems: &[&str] = if manifest.mode == "two-stem" {
        &["vocals"]
    } else {
        &["vocals", "other", "bass", "drums"]
    };
    anyhow::ensure!(
        manifest.models.len() == expected_stems.len(),
        "{} model manifest has the wrong number of ONNX targets",
        manifest.mode
    );
    let mut seen = std::collections::HashSet::new();
    let mut resolved = Vec::with_capacity(manifest.models.len());
    for entry in &manifest.models {
        anyhow::ensure!(
            expected_stems.contains(&entry.stem.as_str()),
            "unknown separator stem {}",
            entry.stem
        );
        anyhow::ensure!(
            seen.insert(entry.stem.as_str()),
            "duplicate separator target {}",
            entry.stem
        );
        anyhow::ensure!(
            entry.file.len() <= 255 && !entry.file.is_empty(),
            "invalid relative ONNX filename in model manifest"
        );
        let relative = Path::new(&entry.file);
        anyhow::ensure!(
            relative.components().count() == 1 && relative.file_name().is_some(),
            "model manifest files must be simple relative filenames"
        );
        anyhow::ensure!(
            relative
                .extension()
                .and_then(|value| value.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("onnx")),
            "model manifest target must be an ONNX file"
        );
        anyhow::ensure!(
            matches!(entry.n_fft, 4_096 | 6_144 | 8_192 | 16_384),
            "model manifest FFT size is outside the supported MDX-Net set"
        );
        anyhow::ensure!(
            entry.dim_f == MDX_DIM_F,
            "MDX-Net target must use dim_f=2048"
        );
        anyhow::ensure!(
            entry.frames == 512,
            "MDX-Net inference currently supports only the verified 512-frame model contract"
        );
        anyhow::ensure!(
            entry.hop_length == MDX_HOP_LENGTH,
            "MDX-Net target must use a 1024-sample hop"
        );
        anyhow::ensure!(
            entry.compensation.is_finite() && (0.5..=2.0).contains(&entry.compensation),
            "model compensation must be finite and within 0.5–2.0"
        );
        let path = fs::canonicalize(manifest_directory.join(relative))
            .with_context(|| format!("could not resolve ONNX model {}", entry.file))?;
        crate::import::ensure_local_path(&path)?;
        anyhow::ensure!(
            path.is_file(),
            "separator model is missing: {}",
            path.display()
        );
        resolved.push((entry.clone(), path));
    }
    for stem in expected_stems {
        anyhow::ensure!(
            seen.contains(stem),
            "model manifest is missing the {stem} target"
        );
    }
    Ok(resolved)
}

pub(crate) fn validate_manifest_models(
    manifest: &SeparatorModelManifest,
    directory: &Path,
    enforce_sha: bool,
) -> Result<Vec<(SeparatorModelEntry, PathBuf)>> {
    let resolved = resolve_manifest_models(manifest, directory)?;
    for (entry, path) in &resolved {
        if enforce_sha {
            let expected = entry
                .sha256
                .as_deref()
                .context("separator model manifest is missing a SHA-256 pin")?;
            anyhow::ensure!(
                expected.len() == 64 && expected.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "separator model manifest contains an invalid SHA-256 pin"
            );
            let actual = sha256_file(path)?;
            anyhow::ensure!(
                actual.eq_ignore_ascii_case(expected),
                "SHA-256 mismatch for separator target {}",
                entry.stem
            );
        }
        validate_mdx_model(path, entry.dim_f, entry.frames)?;
    }
    Ok(resolved)
}

fn write_manifest(path: &Path, manifest: &SeparatorModelManifest) -> Result<()> {
    let serialized = serde_json::to_vec_pretty(manifest)?;
    let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
    fs::write(&temporary, serialized)
        .with_context(|| format!("could not write model manifest {}", temporary.display()))?;
    if path.exists() {
        fs::remove_file(path)
            .with_context(|| format!("could not replace model manifest {}", path.display()))?;
    }
    fs::rename(&temporary, path)
        .with_context(|| format!("could not install model manifest {}", path.display()))?;
    Ok(())
}

fn artifact_url(artifact: &Artifact) -> String {
    format!(
        "https://huggingface.co/{}/resolve/{}/{}?download=true",
        artifact.repository, artifact.revision, artifact.repository_path
    )
}

fn trusted_huggingface_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("huggingface.co")
        || host.eq_ignore_ascii_case("hf.co")
        || host.to_ascii_lowercase().ends_with(".huggingface.co")
        || host.to_ascii_lowercase().ends_with(".hf.co")
        || host.to_ascii_lowercase().ends_with(".xethub.hf.co")
}

fn validate_artifact(artifact: &Artifact, path: &Path) -> Result<()> {
    match artifact.validator {
        ArtifactKind::WhisperTiny => {
            let size = fs::metadata(path)?.len();
            anyhow::ensure!(
                size > 16 * 1024 * 1024 && size <= artifact.max_bytes,
                "Whisper Tiny file size is outside the expected range"
            );
            Ok(())
        }
        ArtifactKind::KokoroOnnx => validate_kokoro_onnx(path),
        ArtifactKind::KokoroVoice => {
            validate_kokoro_voice(path)?;
            Ok(())
        }
    }
}

fn validate_whisper_model(path: &Path) -> Result<String> {
    let context = WhisperContext::new_with_params(path, WhisperContextParameters::default())
        .with_context(|| format!("could not load local Whisper model {}", path.display()))?;
    let model_type = context
        .model_type_readable_str()
        .context("could not identify the selected Whisper model type")?
        .to_owned();
    anyhow::ensure!(
        model_type == "tiny" || model_type == "tiny.en",
        "select Whisper tiny or tiny.en; this file is {model_type}"
    );
    Ok(model_type)
}

#[derive(Debug, Clone, Copy)]
struct MdxSignature {
    dim_f: usize,
    frames: usize,
}

fn local_onnx_session(path: &Path, model_label: &str) -> Result<Session> {
    let mut builder = super::onnx_runtime::session_builder()?
        .with_intra_threads(1)
        .map_err(|error| anyhow::anyhow!("could not configure ONNX Runtime: {error}"))?
        .with_inter_threads(1)
        .map_err(|error| anyhow::anyhow!("could not configure ONNX Runtime: {error}"))?
        .with_parallel_execution(false)
        .map_err(|error| anyhow::anyhow!("could not configure ONNX Runtime: {error}"))?;
    builder.commit_from_file(path).map_err(|error| {
        anyhow::anyhow!("could not load {model_label} {}: {error}", path.display())
    })
}

fn inspect_mdx_signature(path: &Path) -> Result<MdxSignature> {
    let session = local_onnx_session(path, "separator ONNX model")?;
    anyhow::ensure!(
        session.inputs().len() == 1 && session.outputs().len() == 1,
        "MDX-Net ONNX must have exactly one input and one output tensor"
    );
    let input = session
        .inputs()
        .first()
        .context("separator ONNX has no inputs")?;
    let output = session
        .outputs()
        .first()
        .context("separator ONNX has no outputs")?;
    anyhow::ensure!(
        input.name() == "input",
        "MDX-Net ONNX input tensor must be named 'input'"
    );
    let input_shape = tensor_shape(input.dtype(), TensorElementType::Float32)
        .context("separator ONNX input must be a float32 tensor")?;
    let output_shape = tensor_shape(output.dtype(), TensorElementType::Float32)
        .context("separator ONNX output must be a float32 tensor")?;
    anyhow::ensure!(
        input_shape.len() == 4 && output_shape.len() == 4,
        "MDX-Net tensors must be rank-4 [1,4,dim_f,frames]"
    );
    anyhow::ensure!(
        fixed_dim_matches(input_shape[0], 1) && fixed_dim_matches(input_shape[1], 4),
        "MDX-Net input batch/channel shape must be [1,4,...]"
    );
    anyhow::ensure!(
        fixed_dim_matches(input_shape[2], MDX_DIM_F as i64),
        "MDX-Net input dim_f must be 2048"
    );
    anyhow::ensure!(
        fixed_dim_matches(output_shape[0], 1) && fixed_dim_matches(output_shape[1], 4),
        "MDX-Net output batch/channel shape must be [1,4,...]"
    );
    anyhow::ensure!(
        fixed_dim_matches(output_shape[2], MDX_DIM_F as i64),
        "MDX-Net output dim_f must be 2048"
    );
    let frames = if input_shape[3] > 0 {
        input_shape[3] as usize
    } else if output_shape[3] > 0 {
        output_shape[3] as usize
    } else {
        MDX_FRAMES
    };
    anyhow::ensure!(
        frames == MDX_FRAMES,
        "MDX-Net tensor frame count must be 512 for the supported preprocessing contract"
    );
    anyhow::ensure!(
        fixed_dim_matches(output_shape[3], frames as i64),
        "MDX-Net input/output frame dimensions do not match"
    );
    Ok(MdxSignature {
        dim_f: MDX_DIM_F,
        frames,
    })
}

fn validate_mdx_model(path: &Path, dim_f: usize, frames: usize) -> Result<()> {
    let signature = inspect_mdx_signature(path)?;
    anyhow::ensure!(
        signature.dim_f == dim_f && signature.frames == frames,
        "MDX-Net ONNX tensors do not match the declared [1, 4, {dim_f}, {frames}] contract"
    );
    Ok(())
}

fn validate_kokoro_onnx(path: &Path) -> Result<()> {
    let session = local_onnx_session(path, "Kokoro ONNX model")?;
    let mut input_ids = false;
    let mut style = false;
    let mut speed = false;
    for input in session.inputs() {
        match input.name() {
            "input_ids" => {
                let shape = tensor_shape(input.dtype(), TensorElementType::Int64)?;
                input_ids = shape.len() == 2 && fixed_dim_matches(shape[0], 1);
            }
            "style" => {
                let shape = tensor_shape(input.dtype(), TensorElementType::Float32)?;
                style = shape.len() == 2
                    && fixed_dim_matches(shape[0], 1)
                    && fixed_dim_matches(shape[1], 256);
            }
            "speed" => {
                let shape = tensor_shape(input.dtype(), TensorElementType::Float32)?;
                speed = shape.len() == 1 && fixed_dim_matches(shape[0], 1);
            }
            _ => {}
        }
    }
    anyhow::ensure!(
        input_ids && style && speed,
        "Kokoro v1.0 needs input_ids int64 [1,T], style float32 [1,256], and speed float32 [1]"
    );
    anyhow::ensure!(
        !session.outputs().is_empty(),
        "Kokoro ONNX model has no outputs"
    );
    let output = session
        .outputs()
        .first()
        .context("Kokoro ONNX model has no output")?;
    let _ = tensor_shape(output.dtype(), TensorElementType::Float32)
        .context("Kokoro audio output must be float32")?;
    Ok(())
}

fn validate_kokoro_voice(path: &Path) -> Result<usize> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("could not read voice pack {}", path.display()))?;
    let bytes = usize::try_from(metadata.len()).context("Kokoro voice pack is too large")?;
    const ROW_BYTES: usize = 256 * std::mem::size_of::<f32>();
    anyhow::ensure!(
        bytes >= ROW_BYTES && bytes % ROW_BYTES == 0,
        "Kokoro voice pack must contain 256-float little-endian vectors"
    );
    anyhow::ensure!(
        bytes <= MAX_VOICE_BYTES as usize,
        "Kokoro voice pack exceeds the size limit"
    );
    let mut file = File::open(path)?;
    let mut buffer = [0_u8; 64 * 1024];
    let mut pending = Vec::new();
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        pending.extend_from_slice(&buffer[..read]);
        let complete = pending.len() / 4 * 4;
        for chunk in pending[..complete].chunks_exact(4) {
            let value = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            anyhow::ensure!(
                value.is_finite(),
                "Kokoro voice pack contains a non-finite vector value"
            );
        }
        pending.drain(..complete);
    }
    anyhow::ensure!(
        pending.is_empty(),
        "Kokoro voice pack has a truncated float32 value"
    );
    Ok(bytes / ROW_BYTES)
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

fn fixed_dim_matches(actual: i64, expected: i64) -> bool {
    actual == expected || actual == -1
}

fn infer_stem_name(path: &Path) -> Result<String> {
    let name = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let matches = ["vocals", "other", "bass", "drums"]
        .into_iter()
        .filter(|stem| name.contains(stem))
        .collect::<Vec<_>>();
    anyhow::ensure!(
        matches.len() == 1,
        "four-stem user model filenames must identify exactly one of vocals, other, bass, or drums"
    );
    Ok(matches[0].to_owned())
}

fn sha256_file(path: &Path) -> Result<String> {
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

fn sha256_bytes(bytes: Vec<u8>) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::{fixed_dim_matches, validate_kokoro_voice, DEFAULT_COMPENSATION, MDX_FRAMES};

    #[test]
    fn tensor_dimensions_accept_static_and_dynamic_axes_only() {
        assert!(fixed_dim_matches(1, 1));
        assert!(fixed_dim_matches(-1, 1));
        assert!(!fixed_dim_matches(2, 1));
    }

    #[test]
    fn mdx_manifest_defaults_are_stable() {
        assert_eq!(MDX_FRAMES, 512);
        assert!((DEFAULT_COMPENSATION - 1.035).abs() < f32::EPSILON);
    }

    #[test]
    fn voice_pack_requires_an_exact_float_vector_matrix() {
        assert!(validate_kokoro_voice(std::path::Path::new("nonexistent.bin")).is_err());
    }
}
