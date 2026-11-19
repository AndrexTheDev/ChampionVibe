# ChampionVibe device identity bridge

A small Tauri v2 plugin that reads `Settings.Secure.ANDROID_ID` in Kotlin and returns it directly to the Rust plugin handle. The raw value is not exposed through a frontend-facing Tauri command. Rust applies the app-specific SHA-256 binding before storing or transmitting it. This avoids relying on `ndk_context::android_context()`, which has been reported as uninitialized in some recent Tauri/tao Android builds.

On desktop, the plugin is a no-op; Windows binding is read by the app's Rust implementation.
