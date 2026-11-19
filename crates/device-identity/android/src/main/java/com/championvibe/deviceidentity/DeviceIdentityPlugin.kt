package com.championvibe.deviceidentity

import android.app.Activity
import android.content.Context
import android.provider.Settings
import app.tauri.annotation.Command
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin

@TauriPlugin
class DeviceIdentityPlugin(private val activity: Activity) : Plugin(activity) {
    @Command
    fun getAndroidId(invoke: Invoke) {
        try {
            val id = Settings.Secure.getString(
                activity.contentResolver,
                Settings.Secure.ANDROID_ID
            )
            if (id.isNullOrBlank()) {
                invoke.reject("Android Settings.Secure.ANDROID_ID is unavailable")
                return
            }
            val result = JSObject()
            result.put("id", id)
            invoke.resolve(result)
        } catch (error: Exception) {
            invoke.reject("Could not read Android device identifier: ${error.message}")
        }
    }

    @Command
    fun getExternalFilesDir(invoke: Invoke) {
        try {
            val directory = activity.getExternalFilesDir(null)
            if (directory == null) {
                invoke.reject("Android external app files directory is unavailable")
                return
            }
            val result = JSObject()
            result.put("path", directory.absolutePath)
            invoke.resolve(result)
        } catch (error: Exception) {
            invoke.reject("Could not resolve Android app files directory: ${error.message}")
        }
    }

    @Command
    fun initializeAudioContext(invoke: Invoke) {
        try {
            if (!nativeInitializeAudioContext(activity.applicationContext)) {
                invoke.reject("Could not initialize the native Android audio context")
                return
            }
            val result = JSObject()
            result.put("ready", true)
            invoke.resolve(result)
        } catch (error: UnsatisfiedLinkError) {
            invoke.reject("Native ChampionVibe audio context bridge is unavailable: ${error.message}")
        }
    }

    private external fun nativeInitializeAudioContext(context: Context): Boolean
}
