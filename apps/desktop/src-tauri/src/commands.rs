//! `#[tauri::command]` handlers invoked from the Settings webview. The
//! overlay window is driven entirely by events (see `overlay.rs`) and
//! doesn't call back into Rust.

use std::sync::atomic::Ordering;

use flow_core::config::Config;
use flow_core::permissions;
use serde::Serialize;
use tauri::{AppHandle, Manager, State};

use crate::state::AppState;

#[derive(Serialize)]
pub struct PermissionStatus {
    pub microphone_reachable: bool,
    pub accessibility_trusted: bool,
    pub secure_input_active: bool,
    /// Whether the CGEventTap for the hold-to-talk key installed. `false`
    /// almost always means Input Monitoring permission is missing — a
    /// permission `AXIsProcessTrustedWithOptions` can't detect directly, so
    /// we surface it as "did the tap come up" instead.
    pub hotkey_monitor_active: bool,
}

#[tauri::command]
pub fn get_config(state: State<AppState>) -> Config {
    state.config.lock().unwrap().clone()
}

#[tauri::command]
pub fn set_config(state: State<AppState>, config: Config) -> Result<(), String> {
    config.save().map_err(|e| e.to_string())?;

    // Apply what can change live; hotkey keycode is the main one — the tap
    // reads it on every event. hold_threshold_ms is also read live by the
    // coordinator each press. idle_unload_secs would require restarting the
    // model-manager thread, so it only takes effect after an app restart.
    if let Some(handle) = state.hotkey_keycode_handle.lock().unwrap().as_ref() {
        handle.store(config.hotkey_keycode, Ordering::Relaxed);
    }
    *state.config.lock().unwrap() = config;
    Ok(())
}

#[tauri::command]
pub fn get_permission_status(state: State<AppState>) -> PermissionStatus {
    PermissionStatus {
        microphone_reachable: permissions::probe_microphone(),
        accessibility_trusted: permissions::accessibility_trusted(),
        secure_input_active: permissions::secure_input_enabled(),
        hotkey_monitor_active: state.hotkey_monitor_active.load(Ordering::Relaxed),
    }
}

#[tauri::command]
pub fn open_accessibility_settings() {
    permissions::open_accessibility_settings();
}

#[tauri::command]
pub fn get_last_transcript(state: State<AppState>) -> Option<String> {
    state.last_transcript.lock().unwrap().clone()
}

#[tauri::command]
pub fn copy_last_transcript(state: State<AppState>) -> bool {
    if let Some(text) = state.last_transcript.lock().unwrap().clone() {
        if let Ok(mut clipboard) = arboard::Clipboard::new() {
            return clipboard.set_text(text).is_ok();
        }
    }
    false
}

/// Used by the "Test overlay" debug flow when driven from a webview instead
/// of the tray (kept for completeness; the tray item is the primary path).
#[tauri::command]
pub fn test_overlay(app: AppHandle) {
    if let Some(tx) = app.state::<AppState>().coordinator_tx.lock().unwrap().as_ref() {
        let _ = tx.send(crate::coordinator::CoordinatorMsg::TestOverlay);
    }
}
