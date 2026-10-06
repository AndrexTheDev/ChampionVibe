mod analysis;
mod mdx;
mod media;
pub(crate) mod separator;
mod whisper;

use crate::{commands::AppState, database, media_source};
use anyhow::{Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
};
#[cfg(target_os = "android")]
use std::{
    io::{self, Read, Write},
    sync::atomic::AtomicU64,
};
use tauri::{AppHandle, Emitter, Manager};
#[cfg(target_os = "android")]
use tauri_plugin_android_fs::{AndroidFsExt, Entry, FsUri};

const MAX_IMPORT_ITEMS: usize = 50_000;
const MAX_ANDROID_SAMPLE_FILES: usize = 61; // 64 total sample slots minus three built-ins.
const MAX_IMPORT_JOBS_TO_EMIT: u32 = 100;
const IMPORT_JOB_UPDATED_EVENT: &str = "import-job-updated";
#[cfg(target_os = "android")]
const AUDIO_EXTENSIONS: &[&str] = &[
    "aac", "aif", "aiff", "aifc", "caf", "flac", "m4a", "mp3", "mp4", "oga", "ogg", "opus", "wav",
    "wave", "webm",
];

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartImportRequest {
    pub paths: Vec<String>,
    pub separation_model_path: String,
    pub whisper_model_path: String,
    #[serde(default)]
    pub use_directml: bool,
    #[serde(default)]
    pub cpu_threads: Option<usize>,
}

pub struct ImportManager {
    data_dir: PathBuf,
    worker_running: AtomicBool,
    active_cancellations: Mutex<std::collections::HashMap<i64, Arc<AtomicBool>>>,
}

impl ImportManager {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            worker_running: AtomicBool::new(false),
            active_cancellations: Mutex::new(std::collections::HashMap::new()),
        }
    }

    pub fn ensure_worker(self: &Arc<Self>, app: AppHandle) -> Result<()> {
        if self
            .worker_running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Ok(());
        }

        let manager = Arc::clone(self);
        let spawn_result = thread::Builder::new()
            .name("championvibe-import-worker".to_owned())
            .spawn(move || {
                if let Err(error) = run_worker_loop(&app, &manager) {
                    eprintln!("ChampionVibe import worker stopped: {error:#}");
                }
                manager.worker_running.store(false, Ordering::Release);
                if has_queued_jobs(&app) {
                    let _ = manager.ensure_worker(app);
                }
            });
        if let Err(error) = spawn_result {
            self.worker_running.store(false, Ordering::Release);
            return Err(error).context("could not start the local import worker");
        }
        Ok(())
    }

    fn register_cancellation(&self, job_id: i64) -> Result<Arc<AtomicBool>> {
        let cancellation = Arc::new(AtomicBool::new(false));
        let mut active = self
            .active_cancellations
            .lock()
            .map_err(|_| anyhow::anyhow!("import cancellation registry lock is poisoned"))?;
        active.insert(job_id, Arc::clone(&cancellation));
        Ok(cancellation)
    }

    fn unregister_cancellation(&self, job_id: i64) {
        if let Ok(mut active) = self.active_cancellations.lock() {
            active.remove(&job_id);
        }
    }

    pub fn signal_cancel(&self, job_id: i64) {
        if let Ok(active) = self.active_cancellations.lock() {
            if let Some(cancellation) = active.get(&job_id) {
                cancellation.store(true, Ordering::Release);
            }
        }
    }

    fn data_dir(&self) -> &Path {
        &self.data_dir
    }
}

pub fn start_job(
    app: &AppHandle,
    state: &AppState,
    request: StartImportRequest,
) -> Result<database::ImportJobSnapshot, String> {
    let separation_model = canonical_separator_path(&request.separation_model_path)
        .map_err(|error| format!("{error:#}"))?;
    if is_mdx_manifest_path(Path::new(&separation_model)) {
        state
            .model_manager
            .verify_path_if_managed(Path::new(&separation_model))
            .map_err(|error| format!("MDX-Net manifest, tensor contract, or SHA-256 pin failed validation: {error:#}"))?;
    } else {
        separator::validate_htdemucs_model(Path::new(&separation_model))
            .map_err(|error| format!("HT-Demucs model failed validation: {error:#}"))?;
        state
            .model_manager
            .verify_path_if_managed(Path::new(&separation_model))
            .map_err(|error| format!("separator model integrity check failed: {error:#}"))?;
    }
    let whisper_model = canonical_model_path(&request.whisper_model_path, "bin")
        .map_err(|error| format!("{error:#}"))?;
    state
        .model_manager
        .validate_user_file(&whisper_model, "whisper")
        .map_err(|error| format!("Whisper model failed validation: {error:#}"))?;
    let paths = expand_import_paths(app, state.import_manager.data_dir(), &request.paths)
        .map_err(|error| format!("{error:#}"))?;
    let cpu_threads = request
        .cpu_threads
        .unwrap_or_else(default_cpu_threads)
        .clamp(1, 8);
    #[cfg(windows)]
    let use_directml = request.use_directml;
    #[cfg(not(windows))]
    let use_directml = {
        let _ = request.use_directml;
        false
    };

    let job_id = {
        let mut connection = state
            .database
            .lock()
            .map_err(|_| "SQLite connection lock is poisoned".to_owned())?;
        database::create_import_job(
            &mut connection,
            &paths,
            &separation_model,
            &whisper_model,
            use_directml,
            cpu_threads,
        )
        .map_err(|error| format!("could not create import job: {error:#}"))?
    };
    state
        .import_manager
        .ensure_worker(app.clone())
        .map_err(|error| {
            format!("import job {job_id} was queued, but the worker did not start: {error:#}")
        })?;
    emit_job(app, job_id);
    load_job(state, job_id)
}

pub fn recent_jobs(
    state: &AppState,
    limit: u32,
) -> Result<Vec<database::ImportJobSnapshot>, String> {
    let connection = state
        .database
        .lock()
        .map_err(|_| "SQLite connection lock is poisoned".to_owned())?;
    database::import_jobs(&connection, limit.clamp(1, 100))
        .map_err(|error| format!("could not read import jobs: {error:#}"))
}

pub fn cancel_job(app: &AppHandle, state: &AppState, job_id: i64) -> Result<(), String> {
    {
        let mut connection = state
            .database
            .lock()
            .map_err(|_| "SQLite connection lock is poisoned".to_owned())?;
        database::request_import_cancel(&mut connection, job_id)
            .map_err(|error| format!("could not cancel import: {error:#}"))?;
    }
    state.import_manager.signal_cancel(job_id);
    emit_job(app, job_id);
    Ok(())
}

fn load_job(state: &AppState, job_id: i64) -> Result<database::ImportJobSnapshot, String> {
    recent_jobs(state, MAX_IMPORT_JOBS_TO_EMIT)?
        .into_iter()
        .find(|job| job.id == job_id)
        .ok_or_else(|| "new import job disappeared from SQLite".to_owned())
}

fn is_mdx_manifest_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|name| name.to_ascii_lowercase().ends_with(".cvmodel.json"))
}

fn canonical_separator_path(path_text: &str) -> Result<String> {
    anyhow::ensure!(
        !path_text.trim().is_empty(),
        "choose a local HT-Demucs .onnx model or MDX-Net .cvmodel.json manifest"
    );
    anyhow::ensure!(path_text.len() <= 4096, "separator model path is too long");
    let supplied = PathBuf::from(path_text.trim());
    ensure_local_path(&supplied)?;
    let path = fs::canonicalize(&supplied)
        .with_context(|| format!("could not resolve separator model {}", supplied.display()))?;
    anyhow::ensure!(
        path.is_file(),
        "separator model path is not a file: {}",
        path.display()
    );
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    anyhow::ensure!(
        extension.eq_ignore_ascii_case("onnx") || is_mdx_manifest_path(&path),
        "separator model must be a local .onnx file or .cvmodel.json manifest"
    );
    ensure_local_path(&path)?;
    Ok(path.to_string_lossy().into_owned())
}

fn canonical_model_path(path_text: &str, extension: &str) -> Result<String> {
    anyhow::ensure!(
        !path_text.trim().is_empty(),
        "a local .{extension} model file is required"
    );
    anyhow::ensure!(path_text.len() <= 4096, "model path is too long");
    let supplied = PathBuf::from(path_text.trim());
    ensure_local_path(&supplied)?;
    let path = fs::canonicalize(&supplied)
        .with_context(|| format!("could not resolve model file {}", supplied.display()))?;
    anyhow::ensure!(
        path.is_file(),
        "model path is not a file: {}",
        path.display()
    );
    let actual_extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    anyhow::ensure!(
        actual_extension.eq_ignore_ascii_case(extension),
        "expected a local .{extension} model file, got {}",
        path.display()
    );
    ensure_local_path(&path)?;
    Ok(path.to_string_lossy().into_owned())
}

fn expand_audio_paths(requested: &[String]) -> Result<Vec<String>> {
    anyhow::ensure!(
        !requested.is_empty(),
        "choose at least one audio file or folder"
    );
    anyhow::ensure!(
        requested.len() <= MAX_IMPORT_ITEMS,
        "an import request accepts at most 50,000 selections"
    );

    let mut directories = Vec::<PathBuf>::new();
    let mut found_paths = Vec::<String>::new();
    let mut seen = HashSet::<String>::new();
    for item in requested {
        let supplied = PathBuf::from(item.trim());
        anyhow::ensure!(!item.trim().is_empty(), "an import path is empty");
        ensure_local_path(&supplied)?;
        let path = fs::canonicalize(&supplied)
            .with_context(|| format!("could not resolve import path {}", supplied.display()))?;
        ensure_local_path(&path)?;
        if path.is_file() {
            anyhow::ensure!(
                is_supported_audio_path(&path),
                "the file contents are not a recognized local audio container: {}",
                path.display()
            );
            add_audio_path(path, &mut seen, &mut found_paths)?;
        } else if path.is_dir() {
            directories.push(path);
        } else {
            anyhow::bail!("import path is not a file or folder: {}", path.display());
        }
    }

    while let Some(directory) = directories.pop() {
        let mut entries = fs::read_dir(&directory)
            .with_context(|| format!("cannot scan audio folder {}", directory.display()))?
            .collect::<std::io::Result<Vec<_>>>()
            .with_context(|| format!("cannot enumerate audio folder {}", directory.display()))?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries.into_iter().rev() {
            let file_type = entry
                .file_type()
                .with_context(|| format!("cannot inspect {}", entry.path().display()))?;
            if file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            if file_type.is_dir() {
                directories.push(path);
            } else if file_type.is_file() && is_supported_audio_path(&path) {
                add_audio_path(path, &mut seen, &mut found_paths)?;
            }
            anyhow::ensure!(
                found_paths.len() <= MAX_IMPORT_ITEMS,
                "an import batch is limited to 50,000 audio tracks"
            );
        }
    }

    anyhow::ensure!(
        !found_paths.is_empty(),
        "no supported audio files were found"
    );
    found_paths.sort_by(|left, right| path_sort_key(left).cmp(&path_sort_key(right)));
    Ok(found_paths)
}

fn expand_import_paths(
    app: &AppHandle,
    data_dir: &Path,
    requested: &[String],
) -> Result<Vec<String>> {
    #[cfg(target_os = "android")]
    {
        anyhow::ensure!(
            !requested.is_empty(),
            "choose at least one audio file or folder"
        );
        anyhow::ensure!(
            requested.len() <= MAX_IMPORT_ITEMS,
            "an import request accepts at most 50,000 selections"
        );
        let mut local_paths = Vec::new();
        let mut android_uris = Vec::new();
        for selection in requested {
            let selection = selection.trim();
            anyhow::ensure!(!selection.is_empty(), "an import path is empty");
            if is_android_fs_uri(selection) {
                android_uris.push(selection.to_owned());
            } else {
                local_paths.push(selection.to_owned());
            }
        }

        let mut paths = if local_paths.is_empty() {
            Vec::new()
        } else {
            expand_audio_paths(&local_paths)?
        };
        if !android_uris.is_empty() {
            paths.extend(expand_android_audio_uris(app, data_dir, &android_uris)?);
        }
        let mut seen = HashSet::new();
        paths.retain(|path| seen.insert(path_sort_key(path)));
        anyhow::ensure!(!paths.is_empty(), "no supported audio files were found");
        anyhow::ensure!(
            paths.len() <= MAX_IMPORT_ITEMS,
            "an import batch is limited to 50,000 audio tracks"
        );
        paths.sort_by(|left, right| path_sort_key(left).cmp(&path_sort_key(right)));
        Ok(paths)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (app, data_dir);
        expand_audio_paths(requested)
    }
}

#[cfg(target_os = "android")]
fn is_android_fs_uri(value: &str) -> bool {
    let value = value.trim_start();
    value.starts_with('{') || value.starts_with("content://") || value.starts_with("file://")
}

#[cfg(target_os = "android")]
fn parse_android_fs_uri(value: &str) -> Result<FsUri> {
    let value = value.trim();
    let uri = if value.starts_with('{') {
        FsUri::from_json_str(value)
            .map_err(|error| anyhow::anyhow!("invalid Android file selection: {error}"))?
    } else {
        FsUri::from_uri(value)
    };
    anyhow::ensure!(
        uri.is_content_scheme() || uri.is_file_scheme(),
        "Android import only accepts local file and content URIs"
    );
    Ok(uri)
}

#[cfg(target_os = "android")]
fn expand_android_audio_uris(
    app: &AppHandle,
    data_dir: &Path,
    requested: &[String],
) -> Result<Vec<String>> {
    let source_root = data_dir.join("library").join("sources");
    expand_android_files(
        app,
        &source_root,
        requested,
        MAX_IMPORT_ITEMS,
        is_supported_audio_name,
        true,
        "no supported audio files were found",
        "an import batch is limited to 50,000 audio tracks",
    )
}

#[cfg(target_os = "android")]
fn expand_android_files(
    app: &AppHandle,
    destination_root: &Path,
    requested: &[String],
    max_files: usize,
    supports_file: fn(&str) -> bool,
    detect_audio_from_content: bool,
    empty_error: &'static str,
    limit_error: &'static str,
) -> Result<Vec<String>> {
    let api = app.android_fs();
    let mut directories = Vec::<FsUri>::new();
    let mut audio_files = Vec::<(FsUri, String)>::new();
    let mut seen_directories = HashSet::<String>::new();
    let mut seen_files = HashSet::<String>::new();

    for selection in requested {
        let uri = parse_android_fs_uri(selection)?;
        let entry = api
            .get_info(&uri)
            .map_err(|error| anyhow::anyhow!("cannot inspect Android selection: {error}"))?;
        collect_android_entry(
            &api,
            entry,
            &mut directories,
            &mut audio_files,
            &mut seen_files,
            max_files,
            supports_file,
            detect_audio_from_content,
            limit_error,
        )?;
    }

    while let Some(directory) = directories.pop() {
        let directory_key = directory
            .to_json_string()
            .map_err(|error| anyhow::anyhow!("cannot identify Android directory: {error}"))?;
        if !seen_directories.insert(directory_key) {
            continue;
        }
        let entries = api
            .read_dir(&directory)
            .map_err(|error| anyhow::anyhow!("cannot list selected Android directory: {error}"))?;
        for entry in entries {
            collect_android_entry(
                &api,
                entry,
                &mut directories,
                &mut audio_files,
                &mut seen_files,
                max_files,
                supports_file,
                detect_audio_from_content,
                limit_error,
            )?;
        }
    }

    anyhow::ensure!(!audio_files.is_empty(), "{empty_error}");
    fs::create_dir_all(destination_root).with_context(|| {
        format!(
            "cannot create local Android staging directory {}",
            destination_root.display()
        )
    })?;
    let mut paths = Vec::with_capacity(audio_files.len());
    let mut seen_paths = HashSet::new();
    for (uri, name) in audio_files {
        let staged = stage_android_file(&api, &uri, &name, destination_root)?;
        let canonical = fs::canonicalize(&staged)
            .with_context(|| format!("cannot resolve staged Android file {}", staged.display()))?;
        let value = canonical.to_string_lossy().into_owned();
        if seen_paths.insert(path_sort_key(&value)) {
            paths.push(value);
        }
    }
    anyhow::ensure!(!paths.is_empty(), "no supported Android files were staged");
    anyhow::ensure!(paths.len() <= max_files, "{limit_error}");
    paths.sort_by(|left, right| path_sort_key(left).cmp(&path_sort_key(right)));
    Ok(paths)
}

#[cfg(target_os = "android")]
fn collect_android_entry<R: tauri::Runtime>(
    api: &tauri_plugin_android_fs::api::api_sync::AndroidFs<R>,
    entry: Entry,
    directories: &mut Vec<FsUri>,
    audio_files: &mut Vec<(FsUri, String)>,
    seen_files: &mut HashSet<String>,
    max_files: usize,
    supports_file: fn(&str) -> bool,
    detect_audio_from_content: bool,
    limit_error: &'static str,
) -> Result<()> {
    match entry {
        Entry::File { uri, name, .. } => {
            let accepted = if detect_audio_from_content {
                android_uri_has_audio_signature(api, &uri)?
            } else {
                supports_file(&name)
            };
            if accepted {
                let key = uri
                    .to_json_string()
                    .map_err(|error| anyhow::anyhow!("cannot identify Android file: {error}"))?;
                if seen_files.insert(key) {
                    audio_files.push((uri, name));
                    anyhow::ensure!(audio_files.len() <= max_files, "{limit_error}");
                }
            }
        }
        Entry::Dir { uri, .. } => directories.push(uri),
        _ => {}
    }
    Ok(())
}

#[cfg(target_os = "android")]
fn android_uri_has_audio_signature<R: tauri::Runtime>(
    api: &tauri_plugin_android_fs::api::api_sync::AndroidFs<R>,
    uri: &FsUri,
) -> Result<bool> {
    let mut file = api.open_file_readable(uri).map_err(|error| {
        anyhow::anyhow!("cannot inspect selected Android audio content: {error}")
    })?;
    let mut header = [0_u8; 16];
    let read = file
        .read(&mut header)
        .context("cannot read selected Android file signature")?;
    Ok(media_source::classify_audio_header(&header[..read]).is_some())
}

#[cfg(target_os = "android")]
fn is_supported_audio_name(name: &str) -> bool {
    let basename = name
        .rsplit(|character| character == '/' || character == '\\')
        .next()
        .unwrap_or(name);
    Path::new(basename)
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|extension| {
            AUDIO_EXTENSIONS
                .iter()
                .any(|allowed| extension.eq_ignore_ascii_case(allowed))
        })
}

#[cfg(target_os = "android")]
fn is_wav_name(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|extension| matches!(extension.to_ascii_lowercase().as_str(), "wav" | "wave"))
}

#[cfg(target_os = "android")]
static ANDROID_STAGE_FILE_ID: AtomicU64 = AtomicU64::new(0);

#[cfg(target_os = "android")]
struct HashingWriter<'a> {
    output: &'a mut fs::File,
    hasher: Sha256,
}

#[cfg(target_os = "android")]
impl Write for HashingWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let written = self.output.write(buffer)?;
        self.hasher.update(&buffer[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}

#[cfg(target_os = "android")]
fn stage_android_file<R: tauri::Runtime>(
    api: &tauri_plugin_android_fs::api::api_sync::AndroidFs<R>,
    uri: &FsUri,
    name: &str,
    destination_root: &Path,
) -> Result<PathBuf> {
    fs::create_dir_all(destination_root).with_context(|| {
        format!(
            "cannot create local staging directory {}",
            destination_root.display()
        )
    })?;
    let safe_name = sanitize_android_filename(name);
    let mut input = api
        .open_file_readable(uri)
        .map_err(|error| anyhow::anyhow!("cannot read selected local Android file: {error}"))?;
    let (temporary_path, mut output) = create_android_stage_file(destination_root)?;
    let staged_result: Result<PathBuf> = (|| {
        let digest = {
            let mut writer = HashingWriter {
                output: &mut output,
                hasher: Sha256::new(),
            };
            io::copy(&mut input, &mut writer)
                .context("could not copy selected Android file locally")?;
            writer
                .flush()
                .context("could not flush staged Android file")?;
            hex::encode(writer.hasher.clone().finalize())
        };
        output
            .sync_all()
            .context("could not finalize staged Android file")?;
        let destination = destination_root.join(format!("{digest}-{safe_name}"));
        if destination.exists() {
            fs::remove_file(&temporary_path)
                .context("could not remove duplicate Android staging file")?;
        } else if let Err(error) = fs::rename(&temporary_path, &destination) {
            if destination.exists() {
                fs::remove_file(&temporary_path)
                    .context("could not remove duplicate Android staging file")?;
            } else {
                return Err(error).with_context(|| {
                    format!(
                        "could not store staged Android file at {}",
                        destination.display()
                    )
                });
            }
        }
        Ok(destination)
    })();
    if staged_result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    staged_result
}

#[cfg(target_os = "android")]
fn create_android_stage_file(directory: &Path) -> Result<(PathBuf, fs::File)> {
    for _ in 0..100 {
        let id = ANDROID_STAGE_FILE_ID.fetch_add(1, Ordering::Relaxed);
        let path = directory.join(format!(".stage-{}-{id}.tmp", std::process::id()));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("cannot create {}", path.display()))
            }
        }
    }
    anyhow::bail!("could not allocate a unique Android staging file")
}

#[cfg(target_os = "android")]
fn sanitize_android_filename(name: &str) -> String {
    let basename = name
        .rsplit(|character| character == '/' || character == '\\')
        .next()
        .unwrap_or(name);
    let sanitized = basename
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_' | ' ') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    let sanitized =
        sanitized.trim_matches(|character: char| character == '.' || character.is_whitespace());
    if sanitized.is_empty() {
        "audio".to_owned()
    } else {
        sanitized.to_owned()
    }
}

#[cfg(target_os = "android")]
pub fn pick_android_sample_folder(app: &AppHandle) -> Result<Option<String>, String> {
    let api = app.android_fs();
    let mut selected = api
        .picker()
        .pick_dir(None, true)
        .map(|selected| selected.into_iter().collect::<Vec<_>>())
        .map_err(|error| format!("Android WAV folder picker failed: {error}"))?;
    let Some(uri) = selected.pop() else {
        return Ok(None);
    };
    let selection = uri
        .to_json_string()
        .map_err(|error| format!("could not serialize Android sample folder: {error}"))?;
    let data_dir = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("could not resolve application data directory: {error}"))?;
    let destination = data_dir.join("samples").join("android-selected");
    if destination.exists() {
        fs::remove_dir_all(&destination).map_err(|error| {
            format!("could not clear previous Android sample selection: {error}")
        })?;
    }
    expand_android_files(
        app,
        &destination,
        &[selection],
        MAX_ANDROID_SAMPLE_FILES,
        is_wav_name,
        false,
        "no WAV files were found in the selected folder",
        "Android sample folders are limited to 61 WAV files",
    )
    .map_err(|error| format!("could not stage Android WAV samples locally: {error:#}"))?;
    Ok(Some(destination.to_string_lossy().into_owned()))
}

#[cfg(not(target_os = "android"))]
pub fn pick_android_sample_folder(_app: &AppHandle) -> Result<Option<String>, String> {
    Err("the Storage Access Framework sample picker is only available on Android".to_owned())
}

#[cfg(target_os = "android")]
pub fn pick_android_import_paths(app: &AppHandle, directory: bool) -> Result<Vec<String>, String> {
    let api = app.android_fs();
    let picker = api.picker();
    let selected = if directory {
        picker
            .pick_dir(None, true)
            .map(|selected| selected.into_iter().collect::<Vec<_>>())
            .map_err(|error| format!("Android folder picker failed: {error}"))?
    } else {
        picker
            .pick_files(None, &["audio/*"], true)
            .map_err(|error| format!("Android audio picker failed: {error}"))?
    };
    selected
        .into_iter()
        .map(|uri| {
            uri.to_json_string()
                .map_err(|error| format!("could not serialize Android file selection: {error}"))
        })
        .collect()
}

#[cfg(not(target_os = "android"))]
pub fn pick_android_import_paths(
    _app: &AppHandle,
    _directory: bool,
) -> Result<Vec<String>, String> {
    Err("the Storage Access Framework picker is only available on Android".to_owned())
}

#[cfg(target_os = "android")]
pub fn pick_android_model(app: &AppHandle, extension: &str) -> Result<Option<String>, String> {
    if !matches!(extension, "onnx" | "bin" | "json") {
        return Err("unsupported local model extension".to_owned());
    }
    let api = app.android_fs();
    let selected = api
        .picker()
        .pick_file(None, &["*/*"], true)
        .map_err(|error| format!("Android model picker failed: {error}"))?;
    let Some(uri) = selected else {
        return Ok(None);
    };
    let name = api
        .get_name(&uri)
        .map_err(|error| format!("could not read selected Android model name: {error}"))?;
    let extension_matches = Path::new(&name)
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|actual| actual.eq_ignore_ascii_case(extension));
    if !extension_matches {
        return Err(format!("select a local .{extension} model file"));
    }
    let data_dir = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("could not resolve application data directory: {error}"))?;
    let staged = stage_android_file(&api, &uri, &name, &data_dir.join("models"))
        .map_err(|error| format!("could not stage local model: {error:#}"))?;
    let canonical = fs::canonicalize(&staged)
        .map_err(|error| format!("could not resolve staged model: {error}"))?;
    Ok(Some(canonical.to_string_lossy().into_owned()))
}

#[cfg(not(target_os = "android"))]
pub fn pick_android_model(_app: &AppHandle, _extension: &str) -> Result<Option<String>, String> {
    Err("the Storage Access Framework model picker is only available on Android".to_owned())
}

fn add_audio_path(
    path: PathBuf,
    seen: &mut HashSet<String>,
    found: &mut Vec<String>,
) -> Result<()> {
    let canonical = fs::canonicalize(&path)
        .with_context(|| format!("could not resolve audio file {}", path.display()))?;
    ensure_local_path(&canonical)?;
    let value = canonical.to_string_lossy().into_owned();
    let key = path_sort_key(&value);
    if seen.insert(key) {
        found.push(value);
    }
    anyhow::ensure!(
        found.len() <= MAX_IMPORT_ITEMS,
        "an import batch is limited to 50,000 audio tracks"
    );
    Ok(())
}

fn is_supported_audio_path(path: &Path) -> bool {
    media_source::looks_like_audio_file(path)
}

fn path_sort_key(path: &str) -> String {
    #[cfg(windows)]
    {
        path.to_lowercase()
    }
    #[cfg(not(windows))]
    {
        path.to_owned()
    }
}

pub(crate) fn ensure_local_path(path: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        let lowercase = text.to_ascii_lowercase();
        let extended_prefix = r"\\?\";
        anyhow::ensure!(
            !lowercase.starts_with(r"\\?\unc\")
                && (!lowercase.starts_with(r"\\") || lowercase.starts_with(extended_prefix)),
            "network UNC paths are not accepted; select audio and model files stored locally"
        );
        let local_path = if lowercase.starts_with(extended_prefix) {
            &text[extended_prefix.len()..]
        } else {
            text.as_ref()
        };
        let bytes = local_path.as_bytes();
        let drive_letter = bytes
            .first()
            .copied()
            .filter(|byte| byte.is_ascii_alphabetic())
            .context("only local drive-letter paths are supported on Windows")?;
        anyhow::ensure!(
            bytes.get(1) == Some(&b':'),
            "only local drive-letter paths are supported on Windows"
        );
        let root = [
            u16::from(drive_letter.to_ascii_uppercase()),
            u16::from(b':'),
            u16::from(b'\\'),
            0,
        ];
        let drive_type = unsafe { GetDriveTypeW(root.as_ptr()) };
        anyhow::ensure!(
            drive_type != 4,
            "mapped network drives are not accepted; select audio and model files stored on a local drive"
        );
        anyhow::ensure!(
            drive_type != 0 && drive_type != 1,
            "could not verify that the selected Windows drive is local"
        );
    }
    #[cfg(not(windows))]
    let _ = path;
    Ok(())
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetDriveTypeW(root_path_name: *const u16) -> u32;
}

fn default_cpu_threads() -> usize {
    thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(4)
        .clamp(1, 8)
}

fn run_worker_loop(app: &AppHandle, manager: &Arc<ImportManager>) -> Result<()> {
    let state = app.state::<AppState>();
    loop {
        let job_id = {
            let mut connection = state
                .database
                .lock()
                .map_err(|_| anyhow::anyhow!("SQLite connection lock is poisoned"))?;
            database::claim_next_import_job(&mut connection)?
        };
        let Some(job_id) = job_id else {
            break;
        };
        emit_job(app, job_id);
        let cancellation = manager.register_cancellation(job_id)?;
        let result = run_import_job(app, &state, manager, job_id, &cancellation);
        manager.unregister_cancellation(job_id);
        if let Err(error) = result {
            let message = error_message(&error);
            if let Ok(mut connection) = state.database.lock() {
                let _ = database::fail_import_job(&mut connection, job_id, &message);
            }
        }
        emit_job(app, job_id);
    }
    Ok(())
}

fn run_import_job(
    app: &AppHandle,
    state: &AppState,
    manager: &ImportManager,
    job_id: i64,
    cancellation: &Arc<AtomicBool>,
) -> Result<()> {
    let config = {
        let connection = state
            .database
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite connection lock is poisoned"))?;
        database::import_job_config(&connection, job_id)?
    };
    if !is_mdx_manifest_path(Path::new(&config.separation_model_path)) {
        state
            .model_manager
            .verify_path_if_managed(Path::new(&config.separation_model_path))
            .context("separator model integrity check failed before inference")?;
    }
    state
        .model_manager
        .verify_path_if_managed(Path::new(&config.whisper_model_path))
        .context("Whisper model integrity check failed before inference")?;
    let mut separator = separator::SeparationEngine::load(
        Path::new(&config.separation_model_path),
        config.cpu_threads,
        config.use_directml,
    )?;
    let whisper =
        whisper::LocalWhisper::load(Path::new(&config.whisper_model_path), config.cpu_threads)?;
    let embedding_path = manager
        .data_dir()
        .join("models")
        .join("audio-embedding-int8.onnx");
    let mut embedding_model = if embedding_path.is_file() {
        match crate::audio::onnx_embedding::OnnxMirEmbedder::load(&embedding_path) {
            Ok(model) => Some(model),
            Err(error) => {
                eprintln!("ChampionVibe optional local MIR ONNX model was not loaded: {error:#}");
                None
            }
        }
    } else {
        None
    };
    let backend = separator.backend();

    loop {
        if cancellation_requested(state, job_id, cancellation)? {
            cancel_remaining_items(state, job_id)?;
            finish_job(state, job_id, true)?;
            return Ok(());
        }
        let item = {
            let mut connection = state
                .database
                .lock()
                .map_err(|_| anyhow::anyhow!("SQLite connection lock is poisoned"))?;
            database::claim_next_import_item(&mut connection, job_id)?
        };
        let Some(item) = item else {
            break;
        };
        emit_job(app, job_id);
        match process_import_item(
            app,
            state,
            manager,
            job_id,
            &item,
            backend,
            &mut separator,
            &whisper,
            &mut embedding_model,
            cancellation,
        ) {
            Ok(()) => {}
            Err(error) if cancellation_requested(state, job_id, cancellation)? => {
                let _ = error;
                let mut connection = state
                    .database
                    .lock()
                    .map_err(|_| anyhow::anyhow!("SQLite connection lock is poisoned"))?;
                database::finish_import_item_cancelled(&connection, item.id)?;
                drop(connection);
                cancel_remaining_items(state, job_id)?;
                finish_job(state, job_id, true)?;
                emit_job(app, job_id);
                return Ok(());
            }
            Err(error) => {
                let message = error_message(&error);
                let connection = state
                    .database
                    .lock()
                    .map_err(|_| anyhow::anyhow!("SQLite connection lock is poisoned"))?;
                database::finish_import_item_failed(&connection, item.id, &message)?;
                emit_job(app, job_id);
            }
        }
    }

    let cancelled = cancellation_requested(state, job_id, cancellation)?;
    if cancelled {
        cancel_remaining_items(state, job_id)?;
    }
    finish_job(state, job_id, cancelled)?;
    if let Err(error) = crate::riddim::rebuild_in_state(state) {
        // A family-index failure must not turn successfully imported tracks into a failed job.
        eprintln!("ChampionVibe riddim indexing failed after import #{job_id}: {error:#}");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn process_import_item(
    app: &AppHandle,
    state: &AppState,
    manager: &ImportManager,
    job_id: i64,
    item: &database::ImportItem,
    backend: &'static str,
    separator: &mut separator::SeparationEngine,
    whisper: &whisper::LocalWhisper,
    embedding_model: &mut Option<crate::audio::onnx_embedding::OnnxMirEmbedder>,
    cancellation: &Arc<AtomicBool>,
) -> Result<()> {
    let workspace = manager
        .data_dir()
        .join("cache")
        .join("imports")
        .join(format!("{job_id}-{}", item.id));
    let stem_root = manager.data_dir().join("library").join("stems");
    let digest = hex::encode(Sha256::digest(item.source_path.as_bytes()));
    let stem_dir = stem_root.join(format!("{digest}-{job_id}-{}", item.id));
    let result = process_import_item_inner(
        app,
        state,
        &workspace,
        &stem_root,
        &stem_dir,
        job_id,
        item,
        backend,
        separator,
        whisper,
        embedding_model,
        cancellation,
    );
    if result.is_err() {
        let _ = fs::remove_dir_all(&workspace);
        let _ = fs::remove_dir_all(&stem_dir);
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn process_import_item_inner(
    app: &AppHandle,
    state: &AppState,
    workspace: &Path,
    stem_root: &Path,
    stem_dir: &Path,
    job_id: i64,
    item: &database::ImportItem,
    backend: &'static str,
    separator: &mut separator::SeparationEngine,
    whisper: &whisper::LocalWhisper,
    embedding_model: &mut Option<crate::audio::onnx_embedding::OnnxMirEmbedder>,
    cancellation: &Arc<AtomicBool>,
) -> Result<()> {
    check_cancel(state, job_id, cancellation)?;
    if workspace.exists() {
        fs::remove_dir_all(workspace)
            .with_context(|| format!("cannot clean import workspace {}", workspace.display()))?;
    }
    if stem_dir.exists() {
        fs::remove_dir_all(stem_dir).with_context(|| {
            format!(
                "cannot clean unfinished stem directory {}",
                stem_dir.display()
            )
        })?;
    }
    fs::create_dir_all(workspace)
        .with_context(|| format!("cannot create import workspace {}", workspace.display()))?;
    fs::create_dir_all(stem_dir)
        .with_context(|| format!("cannot create local stem directory {}", stem_dir.display()))?;

    set_stage(state, job_id, "decoding-source")?;
    emit_job(app, job_id);
    let normalized_wav = workspace.join("normalized-44100-stereo.wav");
    let decoded =
        media::decode_to_wav(Path::new(&item.source_path), &normalized_wav, cancellation)?;
    check_cancel(state, job_id, cancellation)?;

    set_stage(state, job_id, "analyzing-bpm-key-energy")?;
    emit_job(app, job_id);
    let acoustic = analysis::analyze_wav(
        &normalized_wav,
        cancellation,
        decoded.loudness.integrated_lufs,
        decoded.tagged_bpm,
    )?;
    check_cancel(state, job_id, cancellation)?;

    let stem_count = separator.stem_count();
    let separator_stage = format!("separating-{stem_count}-stems-{backend}");
    set_stage(state, job_id, &separator_stage)?;
    emit_job(app, job_id);
    let stem_paths = separator.separate(
        &normalized_wav,
        stem_dir,
        decoded.frames,
        cancellation,
        |progress| {
            update_progress(app, job_id, &separator_stage, 0.10 + progress * 0.65);
        },
    )?;
    anyhow::ensure!(
        stem_paths.len() == stem_count,
        "separator produced {} outputs but its validated profile requires {stem_count}",
        stem_paths.len()
    );
    check_cancel(state, job_id, cancellation)?;

    let acapella_wav = workspace.join("acapella-for-whisper.wav");
    set_stage(state, job_id, "preparing-acapella-for-whisper")?;
    emit_job(app, job_id);
    media::decode_to_wav(&stem_paths[0], &acapella_wav, cancellation)?;
    check_cancel(state, job_id, cancellation)?;

    set_stage(state, job_id, "transcribing-lyrics-locally")?;
    emit_job(app, job_id);
    let transcript = whisper.transcribe(&acapella_wav, cancellation, |progress| {
        update_progress(
            app,
            job_id,
            "transcribing-lyrics-locally",
            0.78 + progress * 0.21,
        );
    })?;
    check_cancel(state, job_id, cancellation)?;

    let stems = ["acapella", "instrumental", "bass", "drums"]
        .into_iter()
        .zip(stem_paths.iter())
        .map(|(kind, path)| database::StemPath {
            kind: kind.to_owned(),
            file_path: path.to_string_lossy().into_owned(),
        })
        .collect();
    let model_output = match embedding_model.as_mut() {
        Some(model) => Some(model.embed(&acoustic.mir.feature_vector)),
        None => None,
    };
    let embedded_vector = match model_output {
        Some(Ok(vector)) => Some(vector),
        Some(Err(error)) => {
            eprintln!("ChampionVibe local MIR ONNX inference failed; using deterministic descriptors: {error:#}");
            *embedding_model = None;
            None
        }
        None => None,
    };
    let (vector_blob, vector_kind) = if let Some(vector) = embedded_vector {
        (
            vector.iter().map(|value| *value as u8).collect::<Vec<_>>(),
            "onnx-mir-int8-v1".to_owned(),
        )
    } else {
        (
            acoustic
                .mir
                .feature_vector
                .iter()
                .map(|value| *value as u8)
                .collect::<Vec<_>>(),
            "mir-dsp-int8-v1".to_owned(),
        )
    };
    let result = database::ImportResult {
        source_path: item.source_path.clone(),
        title: decoded.title,
        artist: decoded.artist,
        duration_ms: decoded.duration_ms,
        source_sample_rate: decoded.source_sample_rate,
        source_channels: decoded.source_channels,
        source_bitrate_bps: decoded.source_bitrate_bps,
        codec: decoded.codec,
        tags_json: decoded.tags_json,
        bpm: acoustic.bpm,
        bpm_confidence: acoustic.bpm_confidence,
        beat_grid_json: serde_json::to_string(&acoustic.beat_grid_seconds)?,
        musical_key: acoustic.musical_key,
        key_confidence: acoustic.key_confidence,
        energy_density: acoustic.energy_density,
        energy_dbfs: acoustic.energy_dbfs,
        dynamic_range_db: acoustic.dynamic_range_db,
        onset_density: acoustic.onset_density,
        spectral_centroid_hz: acoustic.spectral_centroid_hz,
        low_band_ratio: acoustic.low_band_ratio,
        mid_band_ratio: acoustic.mid_band_ratio,
        high_band_ratio: acoustic.high_band_ratio,
        acoustic_tag: acoustic.acoustic_tag,
        acoustic_scores_json: acoustic.acoustic_scores_json,
        lufs_integrated: decoded.loudness.integrated_lufs,
        true_peak_dbtp: decoded.loudness.true_peak_dbtp,
        gain_offset_db: decoded.loudness.gain_offset_db,
        mood_valence: acoustic.mir.mood_valence,
        mood_arousal: acoustic.mir.mood_arousal,
        mood_label: acoustic.mir.mood_label,
        rhythm_pattern: acoustic.mir.rhythm_pattern,
        rhythm_confidence: acoustic.mir.rhythm_confidence,
        style_genre: acoustic.mir.style_genre,
        vector_blob,
        vector_kind,
        transcript: transcript.text,
        transcript_language: transcript.language,
        lyric_sentiment: transcript.sentiment,
        lyric_sentiment_label: transcript.sentiment_label,
        lyric_sentiment_confidence: transcript.sentiment_confidence,
        words: transcript.words,
        stems,
    };
    set_stage(state, job_id, "committing-track-metadata")?;
    let old_paths = {
        let mut connection = state
            .database
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite connection lock is poisoned"))?;
        database::commit_import_result(&mut connection, job_id, item.id, &result)?
    };
    cleanup_replaced_stems(stem_root, old_paths);
    let _ = fs::remove_dir_all(workspace);
    emit_job(app, job_id);
    Ok(())
}

fn cleanup_replaced_stems(stem_root: &Path, old_paths: Vec<String>) {
    let Ok(root) = fs::canonicalize(stem_root) else {
        return;
    };
    for old_path in old_paths {
        let path = PathBuf::from(old_path);
        let Ok(canonical) = fs::canonicalize(&path) else {
            continue;
        };
        if canonical.starts_with(&root) {
            let _ = fs::remove_file(canonical);
        }
    }
}

fn set_stage(state: &AppState, job_id: i64, stage: &str) -> Result<()> {
    let connection = state
        .database
        .lock()
        .map_err(|_| anyhow::anyhow!("SQLite connection lock is poisoned"))?;
    database::set_import_stage(&connection, job_id, stage)?;
    Ok(())
}

fn update_progress(app: &AppHandle, job_id: i64, stage: &str, progress: f64) {
    if let Some(state) = app.try_state::<AppState>() {
        if let Ok(connection) = state.database.lock() {
            let _ = database::set_import_progress(&connection, job_id, stage, progress);
        }
    }
    emit_job(app, job_id);
}

fn check_cancel(state: &AppState, job_id: i64, cancellation: &AtomicBool) -> Result<()> {
    if cancellation_requested(state, job_id, cancellation)? {
        anyhow::bail!("import cancelled");
    }
    Ok(())
}

fn cancellation_requested(
    state: &AppState,
    job_id: i64,
    cancellation: &AtomicBool,
) -> Result<bool> {
    if cancellation.load(Ordering::Acquire) {
        return Ok(true);
    }
    let connection = state
        .database
        .lock()
        .map_err(|_| anyhow::anyhow!("SQLite connection lock is poisoned"))?;
    let requested = database::import_cancel_requested(&connection, job_id)?;
    if requested {
        cancellation.store(true, Ordering::Release);
    }
    Ok(requested)
}

fn cancel_remaining_items(state: &AppState, job_id: i64) -> Result<()> {
    let connection = state
        .database
        .lock()
        .map_err(|_| anyhow::anyhow!("SQLite connection lock is poisoned"))?;
    database::cancel_queued_import_items(&connection, job_id)?;
    Ok(())
}

fn finish_job(state: &AppState, job_id: i64, cancelled: bool) -> Result<()> {
    let mut connection = state
        .database
        .lock()
        .map_err(|_| anyhow::anyhow!("SQLite connection lock is poisoned"))?;
    database::finish_import_job(&mut connection, job_id, cancelled)?;
    Ok(())
}

fn finish_failed_job(state: &AppState, job_id: i64, error: &str) {
    if let Ok(mut connection) = state.database.lock() {
        let _ = database::fail_import_job(&mut connection, job_id, error);
    }
}

fn emit_job(app: &AppHandle, job_id: i64) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let Ok(connection) = state.database.lock() else {
        return;
    };
    let Ok(jobs) = database::import_jobs(&connection, MAX_IMPORT_JOBS_TO_EMIT) else {
        return;
    };
    if let Some(job) = jobs.into_iter().find(|job| job.id == job_id) {
        let _ = app.emit(IMPORT_JOB_UPDATED_EVENT, job);
    }
}

fn has_queued_jobs(app: &AppHandle) -> bool {
    let Some(state) = app.try_state::<AppState>() else {
        return false;
    };
    let Ok(connection) = state.database.lock() else {
        return false;
    };
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM import_jobs WHERE status = 'queued')",
            [],
            |row| row.get(0),
        )
        .unwrap_or(false)
}

fn error_message(error: &anyhow::Error) -> String {
    error.to_string().chars().take(4_096).collect()
}

fn run_import_job_guarded(
    app: &AppHandle,
    state: &AppState,
    manager: &ImportManager,
    job_id: i64,
    cancellation: &Arc<AtomicBool>,
) {
    if let Err(error) = run_import_job(app, state, manager, job_id, cancellation) {
        finish_failed_job(state, job_id, &error_message(&error));
    }
}

#[cfg(test)]
mod tests {
    use super::{is_supported_audio_path, path_sort_key};
    use std::{path::Path, time::SystemTime};

    #[test]
    fn audio_import_discovery_uses_file_contents_instead_of_extensions() {
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "championvibe-audio-sniff-{}-{unique}.unknown",
            std::process::id()
        ));
        std::fs::write(&path, b"fLaC\x00\x00\x00\x22").unwrap();
        assert!(is_supported_audio_path(&path));
        std::fs::write(&path, b"not an audio container").unwrap();
        assert!(!is_supported_audio_path(&path));
        let _ = std::fs::remove_file(path);
    }

    #[cfg(windows)]
    #[test]
    fn windows_path_deduplication_is_case_insensitive() {
        assert_eq!(
            path_sort_key(r"C:\Music\Track.FLAC"),
            path_sort_key(r"c:\music\track.flac")
        );
    }
}
