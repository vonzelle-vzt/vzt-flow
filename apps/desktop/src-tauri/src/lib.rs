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
use state::{AppState, LockRecover};
use tauri::Manager;

/// Log every panic with its location before the default handler runs.
///
/// There was no panic hook at all before this, so a panic on any background
/// thread vanished silently: Rust's default handler writes to stderr, which a
/// GUI app launched from Finder does not have anywhere useful. The coordinator
/// supervisor in `coordinator::spawn` recovers from those panics, but recovery
/// without a record just means the bug is invisible instead of fatal — and the
/// thing that made the 2026-07-29 investigation slow was precisely that a
/// broken app and a working one looked identical from the outside.
///
/// Chains to the previous hook rather than replacing it, so Tauri's own
/// reporting (if any) still runs.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown location>".to_string());
        let thread = std::thread::current();
        let thread_name = thread.name().unwrap_or("<unnamed>");
        eprintln!("[vzt-flow] PANIC on thread '{thread_name}' at {location}: {info}");
        previous(info);
    }));
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    install_panic_hook();

    #[allow(unused_mut)]
    let mut builder = tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ));

    // Windows and Linux (X11) hold-to-talk hotkey uses this plugin (see
    // coordinator.rs's `spawn_hotkey_monitor`); macOS uses flow-core's
    // CGEventTap instead and never registers it, so it's only added to the
    // builder for those two platforms.
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    {
        builder = builder.plugin(tauri_plugin_global_shortcut::Builder::new().build());
    }

    builder
        .invoke_handler(tauri::generate_handler![
            commands::get_config,
            commands::set_config,
            commands::get_permission_status,
            commands::open_accessibility_settings,
            commands::open_input_monitoring_settings,
            commands::request_input_monitoring,
            commands::get_last_transcript,
            commands::copy_last_transcript,
            commands::get_history,
            commands::get_profiles_path,
            commands::copy_text,
            commands::test_overlay,
            commands::get_model_status,
            commands::start_model_download,
        ])
        .setup(|app| {
            let handle = app.handle().clone();

            // Menu-bar only: no Dock icon, no app-switcher entry.
            #[cfg(target_os = "macos")]
            handle.set_activation_policy(tauri::ActivationPolicy::Accessory)?;

            let mut config = Config::load().unwrap_or_else(|e| {
                eprintln!("[vzt-flow] failed to load config, using defaults: {e}");
                Config::default()
            });

            let is_recording = Arc::new(AtomicBool::new(false));
            app.manage(AppState::new(config.clone(), is_recording.clone()));

            tray::setup_tray(&handle)?;

            // First-run onboarding. `ActivationPolicy::Accessory` means a fresh
            // install shows NOTHING on launch — no Dock icon, no window — so a
            // stranger who just installed via .dmg/brew has no way to discover
            // they must download a model and grant permissions. Open the
            // Settings/Setup window once, the first time we see `onboarded ==
            // false`, then flip the flag and persist so we never auto-nag
            // again. Rule chosen: flip on *first display*, not on completion —
            // simplest, deterministic, and if they close it early the tray's
            // "Settings…" item reopens it. Both the on-disk config and the
            // managed in-memory copy are updated so a later `set_config` from
            // the webview can't resurrect `onboarded = false`.
            if !config.onboarded {
                config.onboarded = true;
                if let Err(e) = config.save() {
                    eprintln!("[vzt-flow] failed to persist onboarded flag: {e}");
                }
                *app.state::<AppState>().config.lock_or_recover() = config.clone();
                settings::show_settings(&handle);
            }

            let (coordinator_tx, hotkey_active) =
                coordinator::spawn(handle.clone(), config, is_recording);
            *app.state::<AppState>().coordinator_tx.lock_or_recover() = Some(coordinator_tx);
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
