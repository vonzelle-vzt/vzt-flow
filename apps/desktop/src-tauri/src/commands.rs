//! `#[tauri::command]` handlers invoked from the Settings webview. The
//! overlay window is driven entirely by events (see `overlay.rs`) and
//! doesn't call back into Rust.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use flow_core::config::Config;
use flow_core::history::HistoryEntry;
use flow_core::permissions;
use serde::Serialize;
use tauri::{AppHandle, Manager, State};

use crate::state::{AppState, DownloadKind, DownloadPhase, ModelDownload};

/// Peak on-disk footprint of a Parakeet download. `download_parakeet_v3` never
/// deletes the archive before unpacking, so the 478,517,071-byte `.tar.gz` and
/// the extracted tree coexist under `.staging-parakeet-v3/` until the final
/// `fs::rename`. Measured: archive 456MB + extracted tree 640MB (`du -sk` on a
/// real install) = ~1.1GB peak, not the ~700MB an earlier comment claimed. At
/// 750MB a user with 800MB free passed the preflight and then died mid-extract
/// with a half-unpacked staging dir. Sized with ~10% margin.
const PARAKEET_PEAK_BYTES: u64 = 1_200_000_000;
/// Peak on-disk footprint of the cleanup GGUF: 1,107,409,472 bytes, staged to
/// a `.partial` and atomically renamed (same filesystem — no doubling), so the
/// peak is essentially the file itself plus a margin.
const CLEANUP_PEAK_BYTES: u64 = 1_200_000_000;

/// How many rows the Settings History section shows.
const HISTORY_DISPLAY_COUNT: usize = 20;

#[derive(Serialize)]
pub struct PermissionStatus {
    pub microphone_reachable: bool,
    pub accessibility_trusted: bool,
    pub secure_input_active: bool,
    /// Whether the CGEventTap for the hold-to-talk key is currently armed.
    /// Now a *live* reading, not a launch-time snapshot: the macOS re-arm
    /// driver flips it true the moment a late Input Monitoring grant lets the
    /// tap install (see `coordinator::spawn_hotkey_rearm_driver`). The Settings
    /// dot is green exactly when this is true.
    pub hotkey_monitor_active: bool,
    /// Whether Input Monitoring is *granted* (`IOHIDCheckAccess`). Lets the UI
    /// tell "granted but the tap hasn't armed yet" (a brief transient) apart
    /// from "denied — open Settings". `true` off macOS (no such gate).
    pub input_monitoring_trusted: bool,
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
        input_monitoring_trusted: permissions::input_monitoring_trusted(),
    }
}

#[tauri::command]
pub fn open_accessibility_settings() {
    permissions::open_accessibility_settings();
}

/// Opens System Settings to the Input Monitoring pane (the permission the
/// global hold-to-talk tap needs). Wired to the Setup row's "Open Settings"
/// button.
#[tauri::command]
pub fn open_input_monitoring_settings() {
    permissions::open_input_monitoring_settings();
}

/// Triggers the native Input Monitoring permission prompt (first run only;
/// macOS won't re-prompt afterwards). Mirrors how `probe_microphone` doubles
/// as the mic-prompt trigger. Returns whether access is granted.
#[tauri::command]
pub fn request_input_monitoring() -> bool {
    permissions::request_input_monitoring()
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

/// Last 20 dictations, newest first, for the Settings History section.
#[tauri::command]
pub fn get_history() -> Vec<HistoryEntry> {
    flow_core::history::read_recent(HISTORY_DISPLAY_COUNT).unwrap_or_default()
}

/// The on-disk path to `profiles.toml`, shown (read-only) in Settings so
/// the user knows where to hand-edit per-app mode/tone rules.
#[tauri::command]
pub fn get_profiles_path() -> Option<String> {
    flow_core::profiles::profiles_path().ok().map(|p| p.display().to_string())
}

#[tauri::command]
pub fn copy_text(text: String) -> bool {
    if let Ok(mut clipboard) = arboard::Clipboard::new() {
        return clipboard.set_text(text).is_ok();
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

/// Snapshot of the model-download subsystem for the Settings webview to poll.
/// `phase`/`downloaded`/`total`/`error` describe the single active download
/// (if any); the two `_present` flags say whether each model is installed.
#[derive(Serialize)]
pub struct ModelDownloadStatus {
    pub parakeet_present: bool,
    pub cleanup_present: bool,
    pub phase: DownloadPhase,
    pub downloaded: u64,
    pub total: u64,
    pub error: Option<String>,
}

#[tauri::command]
pub fn get_model_status(state: State<AppState>) -> ModelDownloadStatus {
    let dl = &state.model_download;
    // Parakeet presence is a cheap directory stat — check it live and refresh
    // the hot-path cache the hotkey gate reads. Cleanup presence would cost a
    // 1.1GB hash on an unsentineled config, so we serve the cached flag
    // instead (seeded off-thread at startup, set by the worker on download).
    let parakeet_present = flow_core::models::check_parakeet_model()
        .map(|s| s.present)
        .unwrap_or(false);
    dl.parakeet_present.store(parakeet_present, Ordering::Relaxed);

    ModelDownloadStatus {
        parakeet_present,
        cleanup_present: dl.cleanup_present.load(Ordering::Relaxed),
        phase: *dl.phase.lock().unwrap(),
        downloaded: dl.downloaded.load(Ordering::Relaxed),
        total: dl.total.load(Ordering::Relaxed),
        error: dl.error.lock().unwrap().clone(),
    }
}

/// Kick off a model download on a worker thread. `kind` is `"parakeet"` or
/// `"cleanup"`. Refuses to start while any download is already in flight (the
/// status slot describes a single download), and preflights free disk so we
/// fail with an actionable message instead of filling the volume. Returns
/// immediately — progress is observed via [`get_model_status`].
#[tauri::command]
pub fn start_model_download(state: State<AppState>, kind: String) -> Result<(), String> {
    let dk = DownloadKind::parse(&kind)
        .ok_or_else(|| format!("unknown model kind {kind:?} (expected \"parakeet\" or \"cleanup\")"))?;
    let dl = state.model_download.clone();

    {
        let mut active = dl.active_kind.lock().unwrap();
        if let Some(current) = *active {
            // Refuse a second concurrent download of the same kind explicitly;
            // and, since the status slot is single, of any kind.
            return Err(if current == dk {
                format!("{} download already in progress", dk.as_str())
            } else {
                format!("a {} download is already in progress", current.as_str())
            });
        }

        // Preflight free disk. If we can't measure it (e.g. `df` unavailable),
        // skip the check rather than block a legitimate download.
        let required = peak_bytes(dk);
        if let Some(available) = available_disk_bytes(&disk_check_dir()) {
            check_disk(available, required)?;
        }

        *active = Some(dk);
        *dl.phase.lock().unwrap() = DownloadPhase::Downloading;
        dl.downloaded.store(0, Ordering::Relaxed);
        dl.total.store(0, Ordering::Relaxed);
        *dl.error.lock().unwrap() = None;
    }

    std::thread::spawn(move || run_model_download(dl, dk));
    Ok(())
}

/// The download worker body, factored out of [`start_model_download`] so it can
/// be driven directly from a test with a temp `VZT_FLOW_CONFIG_DIR` (no Tauri
/// runtime). Blocks for the whole (up to 1.1GB) transfer + verify + extract,
/// updating `dl` as it goes, then clears `active_kind`.
pub(crate) fn run_model_download(dl: Arc<ModelDownload>, kind: DownloadKind) {
    let progress_dl = dl.clone();
    // `download_verified` calls this after every chunk (and once when done);
    // the last call has `downloaded == total`, which is our only observable
    // signal that the byte transfer finished and the sha-verify/extract tail
    // has begun — reported as `Verifying`.
    let progress = move |done: u64, total: u64| {
        progress_dl.downloaded.store(done, Ordering::Relaxed);
        progress_dl.total.store(total, Ordering::Relaxed);
        if total > 0 && done >= total {
            *progress_dl.phase.lock().unwrap() = DownloadPhase::Verifying;
        }
    };

    let result = match kind {
        DownloadKind::Parakeet => {
            flow_core::models::download_parakeet_v3_with_progress(false, &progress).map(|_| ())
        }
        DownloadKind::Cleanup => {
            flow_core::models::download_cleanup_model_with_progress(false, &progress).map(|_| ())
        }
    };

    match result {
        Ok(()) => {
            match kind {
                DownloadKind::Parakeet => dl.parakeet_present.store(true, Ordering::Relaxed),
                DownloadKind::Cleanup => dl.cleanup_present.store(true, Ordering::Relaxed),
            }
            *dl.phase.lock().unwrap() = DownloadPhase::Done;
        }
        Err(e) => {
            *dl.error.lock().unwrap() = Some(format!("{e:#}"));
            *dl.phase.lock().unwrap() = DownloadPhase::Error;
        }
    }

    *dl.active_kind.lock().unwrap() = None;
}

/// Peak on-disk footprint required to install `kind`.
fn peak_bytes(kind: DownloadKind) -> u64 {
    match kind {
        DownloadKind::Parakeet => PARAKEET_PEAK_BYTES,
        DownloadKind::Cleanup => CLEANUP_PEAK_BYTES,
    }
}

/// Pure preflight arithmetic (unit-tested): reject when the volume can't hold
/// the download's peak footprint. Kept separate from the `df` probe so the
/// decision is testable without touching a real filesystem.
fn check_disk(available: u64, required: u64) -> Result<(), String> {
    if available < required {
        Err(format!(
            "Not enough free disk space: this model needs about {:.1} GB but only {:.1} GB is free. \
             Free up some space and try again.",
            required as f64 / 1_000_000_000.0,
            available as f64 / 1_000_000_000.0
        ))
    } else {
        Ok(())
    }
}

/// Free bytes on the volume holding `dir`, via `df -Pk` (POSIX output: the
/// "Available" column is 1024-byte blocks). Returns `None` if `df` is missing
/// or its output can't be parsed — callers treat that as "can't check, don't
/// block".
fn available_disk_bytes(dir: &Path) -> Option<u64> {
    let out = std::process::Command::new("df").arg("-Pk").arg(dir).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // Line 0 is the header; line 1 is the data row for `dir`'s volume.
    let row = text.lines().nth(1)?;
    let cols: Vec<&str> = row.split_whitespace().collect();
    // Filesystem 1024-blocks Used Available Capacity Mounted-on
    let avail_kb: u64 = cols.get(3)?.parse().ok()?;
    Some(avail_kb.saturating_mul(1024))
}

/// The nearest existing ancestor of the model directory — `df` needs a path
/// that exists, and the models dir may not have been created yet on a fresh
/// install.
fn disk_check_dir() -> PathBuf {
    let start = flow_core::models::model_root_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut p: &Path = &start;
    loop {
        if p.exists() {
            return p.to_path_buf();
        }
        match p.parent() {
            Some(parent) => p = parent,
            None => return PathBuf::from("/"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_preflight_rejects_when_insufficient_and_accepts_when_enough() {
        // Strictly less than required → reject.
        assert!(check_disk(100, 200).is_err());
        // Exactly enough → accept (boundary is inclusive).
        assert!(check_disk(200, 200).is_ok());
        // Comfortable headroom → accept.
        assert!(check_disk(10_000_000_000, PARAKEET_PEAK_BYTES).is_ok());
        // Realistic "almost full" case with the real Parakeet peak.
        assert!(check_disk(500_000_000, PARAKEET_PEAK_BYTES).is_err());
    }

    #[test]
    fn download_kind_round_trips() {
        assert_eq!(DownloadKind::parse("parakeet"), Some(DownloadKind::Parakeet));
        assert_eq!(DownloadKind::parse("PARAKEET-V3"), Some(DownloadKind::Parakeet));
        assert_eq!(DownloadKind::parse("cleanup"), Some(DownloadKind::Cleanup));
        assert_eq!(DownloadKind::parse("nonsense"), None);
    }

    /// End-to-end download of the real 478MB Parakeet archive into a throwaway
    /// `VZT_FLOW_CONFIG_DIR`, asserting the progress callback saw a nonzero
    /// total and the model ended up present. `#[ignore]` because it hits the
    /// network for hundreds of MB — run explicitly, bounded by a perl alarm.
    #[test]
    #[ignore = "downloads ~478MB from the network; run explicitly"]
    fn download_worker_installs_parakeet_into_temp_config() {
        let tmp = std::env::temp_dir().join(format!(
            "vzt-flow-dlworker-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var(flow_core::config::CONFIG_DIR_ENV, &tmp);

        // Fresh temp config → the model is absent → a real download runs.
        assert!(!flow_core::models::check_parakeet_model().unwrap().present);

        let dl = ModelDownload::new_for_test();
        run_model_download(dl.clone(), DownloadKind::Parakeet);

        assert_eq!(
            *dl.phase.lock().unwrap(),
            DownloadPhase::Done,
            "worker error: {:?}",
            dl.error.lock().unwrap()
        );
        assert!(dl.total.load(Ordering::Relaxed) > 0, "progress must have seen a nonzero total");
        assert!(dl.downloaded.load(Ordering::Relaxed) > 0);
        assert!(dl.parakeet_present.load(Ordering::Relaxed), "cache must flip to present");
        assert!(
            flow_core::models::check_parakeet_model().unwrap().present,
            "model must be installed on disk"
        );
        assert!(dl.active_kind.lock().unwrap().is_none(), "slot must be released");

        std::env::remove_var(flow_core::config::CONFIG_DIR_ENV);
        std::fs::remove_dir_all(&tmp).ok();
    }
}
