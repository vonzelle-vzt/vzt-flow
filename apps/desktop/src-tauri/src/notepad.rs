//! The meeting notepad window: a small, resizable, decorated window bound to
//! one meeting session at a time. Modelled on `overlay.rs`, but — unlike the
//! overlay — this window takes keyboard focus (it has a `<textarea>`), so it
//! uses `NSFloatingWindowLevel` rather than the overlay's
//! `NSScreenSaverWindowLevel`, and is never converted to a non-activating
//! panel: clicking it activates VZT Flow like any other window (see U15).
//!
//! ### Threads (gotcha (h))
//!
//! Every function here that touches the window is called from
//! `meeting_ctl.rs`'s callbacks (`on_started`/`on_line`/`on_tip`, all on
//! `vzt-flow-meeting-session`) or from the tray (main thread) — never assume
//! the caller is on the main thread. Every window operation is therefore
//! wrapped in `app.run_on_main_thread`.

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

use crate::meeting_ctl;

pub const NOTES_LABEL: &str = "notes";

const NOTES_WIDTH: f64 = 360.0;
const NOTES_HEIGHT: f64 = 460.0;
const MARGIN: f64 = 24.0;

/// Creates the notepad window (hidden) if it doesn't already exist, applies
/// the macOS window-level/collection-behavior tweaks, and returns the handle.
///
/// Must be called on the main thread (gotcha (h)) — callers off it should go
/// through [`open`]/[`hide`], which marshal via `run_on_main_thread`.
pub fn ensure_window(app: &AppHandle) -> tauri::Result<WebviewWindow> {
    if let Some(w) = app.get_webview_window(NOTES_LABEL) {
        return Ok(w);
    }

    let (x, y) = top_right_position(app);

    let window = WebviewWindowBuilder::new(app, NOTES_LABEL, WebviewUrl::App("notes.html".into()))
        .title("VZT Flow — Meeting notes")
        .inner_size(NOTES_WIDTH, NOTES_HEIGHT)
        .position(x, y)
        .decorations(true)
        .resizable(true)
        .skip_taskbar(true)
        .always_on_top(true)
        .visible_on_all_workspaces(true)
        .focused(false)
        .visible(false)
        .build()?;

    apply_macos_notes_style(&window);

    let w = window.clone();
    window.on_window_event(move |event| {
        if let tauri::WindowEvent::CloseRequested { api, .. } = event {
            // Closing the notepad hides it and never stops the meeting —
            // capture keeps running until the tray/hotkey stops it.
            api.prevent_close();
            let _ = w.hide();
        }
    });

    Ok(window)
}

/// Computes a top-right position on the primary monitor with a 24pt margin.
fn top_right_position(app: &AppHandle) -> (f64, f64) {
    if let Ok(Some(monitor)) = app.primary_monitor() {
        let size = monitor.size();
        let pos = monitor.position();
        let scale = monitor.scale_factor();
        let logical_w = size.width as f64 / scale;
        let logical_x = pos.x as f64 / scale;
        let logical_y = pos.y as f64 / scale;
        let x = logical_x + logical_w - NOTES_WIDTH - MARGIN;
        let y = logical_y + MARGIN;
        (x, y)
    } else {
        (800.0, 48.0)
    }
}

/// `NSFloatingWindowLevel` (not the overlay's `NSScreenSaverWindowLevel` — a
/// text field at that level fights the input system) plus a collection
/// behavior that follows the user across Spaces/full-screen apps without
/// joining the window cycle.
#[cfg(target_os = "macos")]
fn apply_macos_notes_style(window: &WebviewWindow) {
    use objc2_app_kit::{NSWindow, NSWindowCollectionBehavior};

    let Ok(ns_window_ptr) = window.ns_window() else {
        return;
    };
    if ns_window_ptr.is_null() {
        return;
    }
    unsafe {
        let ns_window: &NSWindow = &*(ns_window_ptr as *const NSWindow);
        ns_window.setLevel(objc2_app_kit::NSFloatingWindowLevel);
        ns_window.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
    }
}

#[cfg(not(target_os = "macos"))]
fn apply_macos_notes_style(_window: &WebviewWindow) {}

/// Fire-and-forget emit to the notepad window. Safe from any thread and a
/// no-op while the window doesn't exist. `meeting_ctl.rs` emits directly via
/// `app.emit_to` today (it already has the app handle in scope everywhere it
/// needs this); kept here as the module's public emit seam for future
/// notepad-only events.
#[allow(dead_code)] // no current caller; part of the module's public API (spec'd signature).
pub fn emit<T: Serialize + Clone>(app: &AppHandle, event: &str, payload: T) {
    let _ = app.emit_to(NOTES_LABEL, event, payload);
}

/// Opens (creating if needed) and shows the notepad, bound to `session_id`.
///
/// `meeting_ctl::bind_payload` rebuilds the `meeting://bind` payload for the
/// session so a re-open (or a first open after `on_started` already emitted
/// it once, to nobody) still binds correctly. Every window op is marshalled
/// through `run_on_main_thread` — this is called from the session thread via
/// `meeting_ctl::maybe_open_notepad` and from the tray.
pub fn open(app: &AppHandle, session_id: &str) {
    let caller = app.clone();
    let app = app.clone();
    let session_id = session_id.to_string();
    if let Err(e) = caller.run_on_main_thread(move || {
        let window = match ensure_window(&app) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("[vzt-flow] could not create the notepad window: {e}");
                return;
            }
        };
        if let Some(payload) = meeting_ctl::bind_payload(&app, &session_id) {
            let _ = window.emit_to(NOTES_LABEL, meeting_ctl::EVENT_BIND, payload);
        }
        let _ = window.show();
    }) {
        eprintln!("[vzt-flow] could not marshal notepad open to the main thread: {e}");
    }
}

/// Shows the notepad without (re)binding it — the manual "Open meeting
/// notes" fallback (tray item / `open_meeting_notes` command), used whether
/// or not a session is currently live. A window already bound to a session
/// keeps showing that session's content; a fresh window shows "Waiting for a
/// meeting" until the next `meeting://bind`.
pub fn show(app: &AppHandle) {
    let caller = app.clone();
    let app = app.clone();
    if let Err(e) = caller.run_on_main_thread(move || match ensure_window(&app) {
        Ok(w) => {
            let _ = w.show();
        }
        Err(e) => eprintln!("[vzt-flow] could not create the notepad window: {e}"),
    }) {
        eprintln!("[vzt-flow] could not marshal notepad show to the main thread: {e}");
    }
}

/// Hides the notepad without closing capture. No-op while the window doesn't
/// exist. Not called within U11 (the window hides itself on
/// `CloseRequested`); kept as the module's public hide seam for U12's tray
/// and any future explicit "hide notes" action.
#[allow(dead_code)] // U12 (tray.rs) seam.
pub fn hide(app: &AppHandle) {
    let caller = app.clone();
    let app = app.clone();
    if let Err(e) = caller.run_on_main_thread(move || {
        if let Some(w) = app.get_webview_window(NOTES_LABEL) {
            let _ = w.hide();
        }
    }) {
        eprintln!("[vzt-flow] could not marshal notepad hide to the main thread: {e}");
    }
}
