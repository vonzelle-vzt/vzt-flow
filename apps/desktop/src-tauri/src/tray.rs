use tauri::menu::{CheckMenuItem, Menu, MenuBuilder, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, State};
use tauri_plugin_autostart::ManagerExt;

use crate::coordinator::CoordinatorMsg;
use crate::state::{AppState, DictationState, ModelLifecycle};

pub const TRAY_ID: &str = "vzt-flow-tray";

pub fn build_menu(app: &AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    let state = app.state::<AppState>();
    let dictation_state = *state.dictation_state.lock().unwrap();
    let model_lifecycle = *state.model_lifecycle.lock().unwrap();
    let launch_at_login = app.autolaunch().is_enabled().unwrap_or(false);

    let status_label = format!(
        "Status: {}  ·  model {}",
        dictation_state.label(),
        match model_lifecycle {
            ModelLifecycle::Unloaded => "unloaded",
            ModelLifecycle::Loading => "loading…",
            ModelLifecycle::Loaded => "loaded",
        }
    );
    let toggle_label = if dictation_state == DictationState::Idle {
        "Start dictation"
    } else {
        "Stop dictation"
    };

    let status_item = MenuItem::with_id(app, "status", &status_label, false, None::<&str>)?;
    let toggle_item = MenuItem::with_id(app, "toggle_dictation", toggle_label, true, None::<&str>)?;
    let copy_item = MenuItem::with_id(app, "copy_last", "Copy last transcript", true, None::<&str>)?;
    let settings_item = MenuItem::with_id(app, "settings", "Settings…", true, None::<&str>)?;
    let test_overlay_item =
        MenuItem::with_id(app, "test_overlay", "Test overlay", true, None::<&str>)?;
    let launch_item = CheckMenuItem::with_id(
        app,
        "launch_at_login",
        "Launch at login",
        true,
        launch_at_login,
        None::<&str>,
    )?;
    let quit_item = MenuItem::with_id(app, "quit", "Quit VZT Flow", true, None::<&str>)?;

    MenuBuilder::new(app)
        .item(&status_item)
        .separator()
        .item(&toggle_item)
        .item(&copy_item)
        .separator()
        .item(&settings_item)
        .item(&test_overlay_item)
        .item(&launch_item)
        .separator()
        .item(&quit_item)
        .build()
}

/// A monochrome (alpha-only) mic glyph on a transparent background — the
/// shape template-mode tray icons need. The app's main `.icns`/`.ico` icon
/// is a flat-colored square with no transparency, which under
/// `icon_as_template` renders as one solid opaque block instead of a
/// glyph, so the tray uses this dedicated asset instead.
const TRAY_ICON_BYTES: &[u8] = include_bytes!("../icons/tray-icon.png");

pub fn setup_tray(app: &AppHandle) -> tauri::Result<()> {
    let menu = build_menu(app)?;

    let icon = tauri::image::Image::from_bytes(TRAY_ICON_BYTES)?;

    TrayIconBuilder::with_id(TRAY_ID)
        .icon(icon)
        .icon_as_template(true)
        .tooltip("VZT Flow")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(handle_menu_event)
        .build(app)?;

    Ok(())
}

/// Rebuilds and re-applies the tray menu, e.g. after dictation state or
/// model lifecycle changes. Cheap enough (a handful of small NSMenuItems)
/// to just rebuild wholesale instead of tracking per-item handles.
pub fn refresh_menu(app: &AppHandle) {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        if let Ok(menu) = build_menu(app) {
            let _ = tray.set_menu(Some(menu));
        }
    }
}

fn handle_menu_event(app: &AppHandle, event: tauri::menu::MenuEvent) {
    let state = app.state::<AppState>();
    match event.id().as_ref() {
        "toggle_dictation" => {
            if let Some(tx) = state.coordinator_tx.lock().unwrap().as_ref() {
                let _ = tx.send(CoordinatorMsg::TrayToggleDictation);
            }
        }
        "copy_last" => {
            copy_last_transcript(app, &state);
        }
        "settings" => {
            crate::settings::show_settings(app);
        }
        "test_overlay" => {
            if let Some(tx) = state.coordinator_tx.lock().unwrap().as_ref() {
                let _ = tx.send(CoordinatorMsg::TestOverlay);
            }
        }
        "launch_at_login" => {
            let enabled = app.autolaunch().is_enabled().unwrap_or(false);
            let result = if enabled {
                app.autolaunch().disable()
            } else {
                app.autolaunch().enable()
            };
            if let Err(e) = result {
                eprintln!("[vzt-flow] failed to toggle launch-at-login: {e}");
            }
            {
                let mut cfg = state.config.lock().unwrap();
                cfg.launch_at_login = !enabled;
                let _ = cfg.save();
            }
            refresh_menu(app);
        }
        "quit" => {
            app.exit(0);
        }
        _ => {}
    }
}

fn copy_last_transcript(app: &AppHandle, state: &State<AppState>) {
    if let Some(text) = state.last_transcript.lock().unwrap().clone() {
        if let Ok(mut clipboard) = arboard::Clipboard::new() {
            let _ = clipboard.set_text(text);
        }
    } else {
        let _ = app; // nothing to copy yet; menu item stays a no-op
    }
}
