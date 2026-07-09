mod commands;
mod coordinator;
mod daemon;
mod meeting_ctl;
mod overlay;
mod settings;
mod state;
mod tray;

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use flow_core::config::Config;
use state::AppState;
use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[allow(unused_mut)]
    let mut builder = tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ));

    // Windows hold-to-talk hotkey uses this plugin (see coordinator.rs's
    // `spawn_hotkey_monitor`); macOS uses flow-core's CGEventTap instead and
    // never registers it, so it's only added to the builder here.
    #[cfg(target_os = "windows")]
    {
        builder = builder.plugin(tauri_plugin_global_shortcut::Builder::new().build());
    }

    builder
        .invoke_handler(tauri::generate_handler![
            commands::get_config,
            commands::set_config,
            commands::get_permission_status,
            commands::open_accessibility_settings,
            commands::get_last_transcript,
            commands::copy_last_transcript,
            commands::get_history,
            commands::get_profiles_path,
            commands::copy_text,
            commands::test_overlay,
        ])
        .setup(|app| {
            let handle = app.handle().clone();

            // Menu-bar only: no Dock icon, no app-switcher entry.
            #[cfg(target_os = "macos")]
            handle.set_activation_policy(tauri::ActivationPolicy::Accessory)?;

            let config = Config::load().unwrap_or_else(|e| {
                eprintln!("[vzt-flow] failed to load config, using defaults: {e}");
                Config::default()
            });

            let is_recording = Arc::new(AtomicBool::new(false));
            app.manage(AppState::new(config.clone(), is_recording.clone()));

            tray::setup_tray(&handle)?;

            let (coordinator_tx, hotkey_active) =
                coordinator::spawn(handle.clone(), config, is_recording);
            *app.state::<AppState>().coordinator_tx.lock().unwrap() = Some(coordinator_tx);
            if !hotkey_active {
                eprintln!(
                    "[vzt-flow] global hold-to-talk key is NOT active. Use the tray's \
                     \"Start/Stop dictation\" item, then grant Input Monitoring permission \
                     and restart the app to enable the hardware hotkey."
                );
            }

            // Daemon control socket: started after the coordinator so
            // `AppState.coordinator_tx` is already populated for the
            // toggle/cancel/listen handlers. A bind failure (e.g. another
            // instance already running) is logged but not fatal — the app
            // still works, just not scriptably.
            if let Err(e) = daemon::spawn(handle.clone()) {
                eprintln!("[vzt-flow] daemon control socket failed to start: {e}");
            }

            // Pre-create (hidden) so the first `show_overlay` call has no
            // window-creation latency mid-recording.
            let _ = overlay::ensure_overlay(&handle);

            // Background meeting auto-detector (Zoom/Meet/Teams). Always
            // spawned; it no-ops when `meeting_auto = "off"` and reads the
            // mode live so the tray submenu takes effect immediately.
            meeting_ctl::spawn_detector(handle.clone());

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building vzt-flow desktop app")
        .run(|_app_handle, event| {
            // `code: None` means the exit was requested by user interaction
            // (e.g. all windows closing) rather than our own tray "Quit"
            // handler calling `app.exit(0)` (which reports `Some(0)`).
            // Since this is a menu-bar app with no real windows to close,
            // only the tray's Quit should ever end the process.
            match event {
                tauri::RunEvent::ExitRequested { api, code, .. } => {
                    if code.is_none() {
                        api.prevent_exit();
                    }
                }
                tauri::RunEvent::Exit => {
                    daemon::cleanup();
                }
                _ => {}
            }
        });
}
