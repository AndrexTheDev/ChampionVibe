use serde::Deserialize;
use tauri::{
    plugin::{PluginApi, PluginHandle},
    AppHandle, Runtime,
};

/// Runtime-bound handle for the Android device identity plugin.
pub struct DeviceIdentity<R: Runtime>(PluginHandle<R>);

#[derive(Debug, Deserialize)]
struct AndroidIdResponse {
    id: String,
}

#[derive(Debug, Deserialize)]
struct AudioContextResponse {
    ready: bool,
}

#[derive(Debug, Deserialize)]
struct ExternalFilesResponse {
    path: String,
}

pub fn init<R: Runtime, C: serde::de::DeserializeOwned>(
    _app: &AppHandle<R>,
    api: PluginApi<R, C>,
) -> Result<DeviceIdentity<R>, Box<dyn std::error::Error>> {
    let handle =
        api.register_android_plugin("com.championvibe.deviceidentity", "DeviceIdentityPlugin")?;
    Ok(DeviceIdentity(handle))
}

impl<R: Runtime> DeviceIdentity<R> {
    /// Reads Android's app/signing-key-scoped ANDROID_ID through the Tauri
    /// mobile plugin bridge; this does not rely on ndk-context globals.
    pub fn android_id(&self) -> Result<String, String> {
        self.0
            .run_mobile_plugin::<AndroidIdResponse>("getAndroidId", ())
            .map(|response| response.id)
            .map_err(|error| error.to_string())
    }

    /// Resolves Android's app-scoped external files directory for local exports.
    pub fn external_files_dir(&self) -> Result<String, String> {
        let response = self
            .0
            .run_mobile_plugin::<ExternalFilesResponse>("getExternalFilesDir", ())
            .map_err(|error| error.to_string())?;
        if response.path.trim().is_empty() {
            return Err("Android external app files directory is unavailable".into());
        }
        Ok(response.path)
    }

    /// Initializes the Android context required by CPAL's AAudio host. The
    /// Kotlin bridge retains an Application Context, not a short-lived Activity.
    pub fn initialize_audio_context(&self) -> Result<(), String> {
        let response = self
            .0
            .run_mobile_plugin::<AudioContextResponse>("initializeAudioContext", ())
            .map_err(|error| error.to_string())?;
        if response.ready {
            Ok(())
        } else {
            Err("Android audio context was not initialized".to_owned())
        }
    }
}
