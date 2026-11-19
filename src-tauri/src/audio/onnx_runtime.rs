//! Load the native inference library before `ort` can take its implicit panic path.

use anyhow::{Context, Result};
use ort::session::{builder::SessionBuilder, Session};
use std::{env, path::PathBuf, sync::OnceLock};

static LOADED: OnceLock<()> = OnceLock::new();

pub(crate) fn session_builder() -> Result<SessionBuilder> {
    if LOADED.get().is_none() {
        let path = library_path()?;
        ort::init_from(&path)
            .map_err(|error| {
                anyhow::anyhow!(
                    "Could not load the native ONNX Runtime (API {}) from {}. \
                     Reinstall ChampionVibe or check that the developer runtime is installed: {error}",
                    ort::MINOR_VERSION,
                    path.display()
                )
            })?
            .commit();
        let _ = LOADED.set(());
    }
    Session::builder().map_err(|error| anyhow::anyhow!("could not create ONNX session: {error}"))
}

fn library_path() -> Result<PathBuf> {
    if let Some(value) = env::var_os("ORT_DYLIB_PATH").filter(|value| !value.is_empty()) {
        let path = PathBuf::from(value);
        anyhow::ensure!(
            path.is_absolute(),
            "ORT_DYLIB_PATH must be an absolute path"
        );
        return Ok(path);
    }

    // Windows installers put the DLLs beside the app executable. Never search the
    // working directory for the main runtime DLL.
    if cfg!(target_os = "windows") {
        let executable = env::current_exe().context("could not locate ChampionVibe")?;
        return Ok(executable.with_file_name("onnxruntime.dll"));
    }
    if cfg!(any(target_os = "macos", target_os = "ios")) {
        return Ok("libonnxruntime.dylib".into());
    }
    // On Android, the app linker namespace resolves native libraries in the APK.
    Ok("libonnxruntime.so".into())
}
