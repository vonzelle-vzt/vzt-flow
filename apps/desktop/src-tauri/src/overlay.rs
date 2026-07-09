//! The small frameless pill window: hidden while idle, shows recording
//! level bars / a spinner / a checkmark depending on dictation state.

use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

pub const OVERLAY_LABEL: &str = "overlay";

const OVERLAY_WIDTH: f64 = 220.0;
const OVERLAY_HEIGHT: f64 = 64.0;
const BOTTOM_MARGIN: f64 = 48.0;

/// Creates the overlay window (hidden) if it doesn't already exist, applies
/// the macOS window-level/collection-behavior tweaks needed to float above
/// full-screen apps without stealing focus, and returns the handle.
pub fn ensure_overlay(app: &AppHandle) -> tauri::Result<WebviewWindow> {
    if let Some(w) = app.get_webview_window(OVERLAY_LABEL) {
        return Ok(w);
    }

    let (x, y) = bottom_center_position(app);

    let window = WebviewWindowBuilder::new(app, OVERLAY_LABEL, WebviewUrl::App("overlay.html".into()))
        .title("VZT Flow Overlay")
        .inner_size(OVERLAY_WIDTH, OVERLAY_HEIGHT)
        .position(x, y)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .resizable(false)
        .maximizable(false)
        .minimizable(false)
        .closable(false)
        .skip_taskbar(true)
        .always_on_top(true)
        .visible_on_all_workspaces(true)
        .focused(false)
        .accept_first_mouse(false)
        .visible(false)
        .build()?;

    apply_macos_overlay_style(&window);

    Ok(window)
}

/// Computes a bottom-center position on the primary monitor. Multi-monitor
/// "active screen" tracking (screen under the cursor / with the frontmost
/// window) is not implemented — this always targets the primary monitor.
fn bottom_center_position(app: &AppHandle) -> (f64, f64) {
    if let Ok(Some(monitor)) = app.primary_monitor() {
        let size = monitor.size();
        let pos = monitor.position();
        let scale = monitor.scale_factor();
        let logical_w = size.width as f64 / scale;
        let logical_h = size.height as f64 / scale;
        let logical_x = pos.x as f64 / scale;
        let logical_y = pos.y as f64 / scale;
        let x = logical_x + (logical_w - OVERLAY_WIDTH) / 2.0;
        let y = logical_y + logical_h - OVERLAY_HEIGHT - BOTTOM_MARGIN;
        (x, y)
    } else {
        (600.0, 800.0)
    }
}

/// Raises the overlay above full-screen apps (NSScreenSaverWindowLevel) and
/// lets it follow the user across Spaces/full-screen apps without joining
/// the window cycle (Cmd+`, Mission Control window picker, etc.).
#[cfg(target_os = "macos")]
fn apply_macos_overlay_style(window: &WebviewWindow) {
    use objc2_app_kit::{NSWindow, NSWindowCollectionBehavior};

    let Ok(ns_window_ptr) = window.ns_window() else {
        return;
    };
    if ns_window_ptr.is_null() {
        return;
    }
    unsafe {
        let ns_window: &NSWindow = &*(ns_window_ptr as *const NSWindow);
        ns_window.setLevel(objc2_app_kit::NSScreenSaverWindowLevel);
        ns_window.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::IgnoresCycle
                | NSWindowCollectionBehavior::Stationary,
        );
    }
}

#[cfg(not(target_os = "macos"))]
fn apply_macos_overlay_style(_window: &WebviewWindow) {}

/// Overlay lifecycle events emitted to the overlay webview.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OverlayEvent {
    /// Not currently emitted (`hide_overlay` calls `window.hide()`
    /// directly instead) but kept so the webview's state machine has an
    /// explicit "hidden" case to mirror this enum 1:1.
    #[allow(dead_code)]
    Hidden,
    /// `elapsed_secs` drives the overlay's mm:ss readout during long-form
    /// holds; `warning` flips true inside the last
    /// [`RECORDING_WARNING_WINDOW_SECS`] before `max_secs` so the pill can
    /// switch to a heads-up appearance before the cap auto-stops the
    /// recording. Construct via [`recording_event`] rather than the struct
    /// literal so that threshold logic lives in one place.
    Recording { level: f32, elapsed_secs: f64, warning: bool },
    /// `mode` is the resolved pipeline mode ("raw"/"clean"/"polish"/"code")
    /// for the frontmost app, shown as a small badge while transcribing.
    Transcribing { mode: String },
    Done,
    Message { text: String },
}

/// How long before a recording's duration cap the overlay switches to its
/// warning appearance — long-form holds (up to 10min) get a heads-up before
/// the cap silently auto-stops them, rather than the transcript just ending
/// mid-sentence with no warning.
const RECORDING_WARNING_WINDOW_SECS: f64 = 30.0;

/// Builds a `Recording` event, computing `elapsed_secs`/`warning` from a
/// start instant and the recording's duration cap. `max_secs == 0` (no cap
/// known yet, e.g. the very first frame) disables the warning rather than
/// firing it spuriously.
pub fn recording_event(level: f32, elapsed: std::time::Duration, max_secs: u64) -> OverlayEvent {
    let elapsed_secs = elapsed.as_secs_f64();
    let remaining_secs = max_secs as f64 - elapsed_secs;
    OverlayEvent::Recording {
        level,
        elapsed_secs,
        warning: max_secs > 0 && remaining_secs <= RECORDING_WARNING_WINDOW_SECS,
    }
}

pub const OVERLAY_EVENT: &str = "overlay://state";

pub fn emit_overlay(app: &AppHandle, event: OverlayEvent) {
    let _ = app.emit_to(OVERLAY_LABEL, OVERLAY_EVENT, event);
}

pub fn show_overlay(app: &AppHandle) {
    if let Ok(w) = ensure_overlay(app) {
        let _ = w.show();
    }
}

pub fn hide_overlay(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(OVERLAY_LABEL) {
        let _ = w.hide();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn unwrap_recording(event: OverlayEvent) -> (f32, f64, bool) {
        match event {
            OverlayEvent::Recording { level, elapsed_secs, warning } => (level, elapsed_secs, warning),
            other => panic!("expected Recording, got {other:?}"),
        }
    }

    #[test]
    fn no_warning_well_before_the_cap() {
        let (level, elapsed, warning) = unwrap_recording(recording_event(0.5, Duration::from_secs(10), 600));
        assert_eq!(level, 0.5);
        assert_eq!(elapsed, 10.0);
        assert!(!warning);
    }

    #[test]
    fn warning_fires_inside_the_last_30s() {
        let (_, _, warning) = unwrap_recording(recording_event(0.5, Duration::from_secs(571), 600));
        assert!(warning, "571s of 600 leaves 29s remaining, inside the warning window");
    }

    #[test]
    fn warning_boundary_is_inclusive_at_exactly_30s_remaining() {
        let (_, _, warning) = unwrap_recording(recording_event(0.5, Duration::from_secs(570), 600));
        assert!(warning, "exactly 30s remaining should already warn");
        let (_, _, not_yet) = unwrap_recording(recording_event(0.5, Duration::from_secs(569), 600));
        assert!(!not_yet, "31s remaining should not warn yet");
    }

    #[test]
    fn no_warning_when_max_secs_is_unknown() {
        // max_secs == 0 means "no cap known yet" (e.g. the very first
        // frame before start_recording has set it) — must never warn.
        let (_, _, warning) = unwrap_recording(recording_event(0.5, Duration::from_secs(9999), 0));
        assert!(!warning);
    }

    #[test]
    fn warning_still_true_past_the_cap() {
        // Elapsed can briefly exceed max_secs between the cap firing and
        // the coordinator resetting state; remaining goes negative, which
        // must still read as "warning", not silently flip back off.
        let (_, _, warning) = unwrap_recording(recording_event(0.5, Duration::from_secs(605), 600));
        assert!(warning);
    }
}
