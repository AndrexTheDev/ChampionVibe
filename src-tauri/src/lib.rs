mod audio;
mod commands;
mod database;
mod device_binding;
mod import;
mod localization;
mod media_source;
mod mix_engine;
mod riddim;
mod symphonia_support;
mod tts;

use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_device_identity::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_android_fs::init())
        .setup(|app| {
            let app_data_dir = app.path().app_data_dir()?;
            let database_path = app_data_dir.join("championvibe.sqlite");
            let mut connection = database::open(&database_path)?;
            database::recover_import_jobs(&mut connection)?;
            let (audio_engine, mix_spare_deck, recording) = audio::AudioEngine::new_pair()?;
            app.manage(commands::AppState::new(
                connection,
                audio_engine,
                mix_spare_deck,
                recording,
                app_data_dir,
            )?);
            let state = app.state::<commands::AppState>();
            state.import_manager.ensure_worker(app.handle().clone())?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_device_binding,
            commands::get_license_status,
            commands::get_localized_catalog,
            commands::create_payment_quote,
            commands::activate_license,
            commands::authorize_recording,
            commands::get_recording_formats,
            commands::start_master_recording,
            commands::get_master_recording_status,
            commands::stop_master_recording,
            commands::save_recording_metadata,
            commands::upsert_track,
            commands::search_tracks,
            commands::search_tracks_page,
            commands::get_track_waveform_packet,
            commands::get_track_manual_tags,
            commands::set_track_manual_tags,
            commands::rebuild_riddim_families,
            commands::get_riddim_families,
            commands::get_track_riddim_family,
            commands::get_model_catalog,
            commands::download_managed_model,
            commands::read_managed_kokoro_asset,
            commands::validate_local_model,
            commands::import_custom_separator_models,
            commands::get_tts_engine_status,
            commands::push_tts_audio_buffer,
            commands::generate_dubplate,
            commands::start_import_job,
            commands::get_import_jobs,
            commands::cancel_import_job,
            commands::pick_android_import_paths,
            commands::pick_android_sample_folder,
            commands::pick_android_model,
            commands::load_track_audio,
            commands::play_audio,
            commands::pause_audio,
            commands::stop_audio,
            commands::get_audio_status,
            commands::set_audio_controls,
            commands::set_deck_audio_controls,
            commands::get_sample_catalog,
            commands::load_sample_folder,
            commands::trigger_sample,
            commands::wheel_and_come_again,
            commands::set_auto_rewind,
            commands::get_sample_directory,
            commands::start_mix_engine,
            commands::stop_mix_engine,
            commands::set_mix_engine_preset,
            commands::set_mix_engine_intensity,
            commands::force_next_track,
            commands::get_mix_engine_status,
        ])
        .run(tauri::generate_context!())
        .expect("ChampionVibe Tauri runtime failed");
}
