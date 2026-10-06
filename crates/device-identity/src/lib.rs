use tauri::{
    plugin::{Builder, TauriPlugin},
    Manager, Runtime, State,
};

#[cfg(not(target_os = "android"))]
mod desktop;
#[cfg(target_os = "android")]
mod mobile;

#[cfg(not(target_os = "android"))]
pub use desktop::DeviceIdentity;
#[cfg(target_os = "android")]
pub use mobile::DeviceIdentity;

/// Access the plugin's native Android identity bridge from Rust.
pub trait DeviceIdentityExt<R: Runtime> {
    fn device_identity(&self) -> State<'_, DeviceIdentity<R>>;
}

impl<R: Runtime, T: Manager<R>> DeviceIdentityExt<R> for T {
    fn device_identity(&self) -> State<'_, DeviceIdentity<R>> {
        self.state::<DeviceIdentity<R>>()
    }
}

pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("device-identity")
        .setup(|app, api| {
            #[cfg(target_os = "android")]
            let identity = mobile::init(app, api)?;
            #[cfg(not(target_os = "android"))]
            let identity = desktop::init();
            app.manage(identity);
            Ok(())
        })
        .build()
}
