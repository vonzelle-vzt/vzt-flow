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
#[derive(Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OverlayEvent {
    /// Not currently emitted (`hide_overlay` calls `window.hide()`
    /// directly instead) but kept so the webview's state machine has an
    /// explicit "hidden" case to mirror this enum 1:1.
    #[allow(dead_code)]
    Hidden,
    Recording { level: f32 },
    /// `mode` is the resolved pipeline mode ("raw"/"clean"/"polish"/"code")
    /// for the frontmost app, shown as a small badge while transcribing.
    Transcribing { mode: String },
    Done,
    Message { text: String },
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
