use tauri::menu::{CheckMenuItem, Menu, MenuBuilder, MenuItem, SubmenuBuilder};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, State};
use tauri_plugin_autostart::ManagerExt;

use flow_core::config::MeetingAuto;
use flow_core::meeting::SessionState;

use crate::coordinator::CoordinatorMsg;
use crate::state::{AppState, DictationState, LockRecover, ModelLifecycle};

pub const TRAY_ID: &str = "vzt-flow-tray";

pub fn build_menu(app: &AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    let state = app.state::<AppState>();
    let dictation_state = *state.dictation_state.lock_or_recover();
    let model_lifecycle = *state.model_lifecycle.lock_or_recover();
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

    // --- meeting transcription state ---
    let meeting_state = crate::meeting_ctl::current_state(app);
    let (meeting_toggle_label, meeting_toggle_enabled) = meeting_toggle_label(meeting_state.as_ref());
    let meeting_auto = state.config.lock_or_recover().meeting_auto_mode();
    let interview_on = crate::meeting_ctl::interview_enabled(app);

    let status_item = MenuItem::with_id(app, "status", &status_label, false, None::<&str>)?;
    let toggle_item = MenuItem::with_id(app, "toggle_dictation", toggle_label, true, None::<&str>)?;
    let copy_item = MenuItem::with_id(app, "copy_last", "Copy last transcript", true, None::<&str>)?;

    let meeting_toggle_item = MenuItem::with_id(
        app,
        "toggle_meeting",
        meeting_toggle_label,
        meeting_toggle_enabled,
        None::<&str>,
    )?;
    let meeting_folder_item =
        MenuItem::with_id(app, "open_meetings", "Open meetings folder", true, None::<&str>)?;
    // Manual fallback for opening the notes window — Screen Recording being
    // ungranted or a title-match miss can keep the automatic open from firing.
    let open_notes_item =
        MenuItem::with_id(app, "open_notes", "Open meeting notes", true, None::<&str>)?;
    let toggle_interview_item = CheckMenuItem::with_id(
        app,
        "toggle_interview",
        "Interview mode",
        true,
        interview_on,
        None::<&str>,
    )?;
    // Auto-detect submenu: three checked-radio-style options bound to config.
    let auto_ask = CheckMenuItem::with_id(
        app,
        "meeting_auto_ask",
        "Ask",
        true,
        meeting_auto == MeetingAuto::Ask,
        None::<&str>,
    )?;
    let auto_auto = CheckMenuItem::with_id(
        app,
        "meeting_auto_auto",
        "Auto",
        true,
        meeting_auto == MeetingAuto::Auto,
        None::<&str>,
    )?;
    let auto_off = CheckMenuItem::with_id(
        app,
        "meeting_auto_off",
        "Off",
        true,
        meeting_auto == MeetingAuto::Off,
        None::<&str>,
    )?;
    let auto_submenu = SubmenuBuilder::new(app, "Meeting auto-detect")
        .item(&auto_ask)
        .item(&auto_auto)
        .item(&auto_off)
        .build()?;
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
        .item(&meeting_toggle_item)
        .item(&meeting_folder_item)
        .item(&open_notes_item)
        .item(&toggle_interview_item)
        .item(&auto_submenu)
        .separator()
        .item(&settings_item)
        .item(&test_overlay_item)
        .item(&launch_item)
        .separator()
        .item(&quit_item)
        .build()
}

/// The meeting toggle item's label and enabled state for a session's
/// lifecycle. Pure so every state is testable without a Tauri app. `None`
/// (no session yet, or the map has already retired a finished one) reads the
/// same as `Completed`/`Failed` — there is nothing running to stop.
///
/// `Stopping`/`Finalizing` disable the item rather than offering "stop" a
/// second time: the microphone is already closed by then (`meeting_ctl::
/// is_active` treats both as not-active) and a second click can't do
/// anything but confuse a summary/PDF export already in flight.
pub fn meeting_toggle_label(state: Option<&SessionState>) -> (&'static str, bool) {
    match state {
        None | Some(SessionState::Completed) | Some(SessionState::Failed(_)) => {
            ("Start meeting transcription", true)
        }
        Some(SessionState::Recording) => ("Stop meeting transcription (\u{25cf} recording)", true),
        Some(SessionState::Stopping) | Some(SessionState::Finalizing { .. }) => {
            ("Finalizing meeting\u{2026}", false)
        }
    }
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
            if let Some(tx) = state.coordinator_tx.lock_or_recover().as_ref() {
                let _ = tx.send(CoordinatorMsg::TrayToggleDictation);
            }
        }
        "copy_last" => {
            copy_last_transcript(app, &state);
        }
        "toggle_meeting" => {
            crate::meeting_ctl::toggle(app);
        }
        "open_meetings" => {
            crate::meeting_ctl::open_folder(app);
        }
        "open_notes" => {
            open_notes(app);
        }
        "toggle_interview" => {
            toggle_interview(app);
        }
        "meeting_auto_ask" => {
            crate::meeting_ctl::set_auto_mode(app, MeetingAuto::Ask);
        }
        "meeting_auto_auto" => {
            crate::meeting_ctl::set_auto_mode(app, MeetingAuto::Auto);
        }
        "meeting_auto_off" => {
            crate::meeting_ctl::set_auto_mode(app, MeetingAuto::Off);
        }
        "settings" => {
            crate::settings::show_settings(app);
        }
        "test_overlay" => {
            if let Some(tx) = state.coordinator_tx.lock_or_recover().as_ref() {
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
                let mut cfg = state.config.lock_or_recover();
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

/// Opens the notepad — the manual fallback for when the automatic open never
/// fired (missing Screen Recording grant, or a title-match miss). Binds to
/// the newest session if one exists so the notepad shows which meeting it
/// belongs to; opens unbound (empty session id) otherwise. `notepad::open`
/// (U11) owns the window itself and marshals every operation onto the main
/// thread (gotcha (h)) — this is only the tray's dispatch point.
fn open_notes(app: &AppHandle) {
    let session_id = crate::meeting_ctl::current_session_id(app).unwrap_or_default();
    crate::notepad::open(app, &session_id);
}

/// Toggles interview mode from the tray.
///
/// A live (or still-finalizing) session flips its slot's flag via
/// `meeting_ctl::set_interview`, which also persists the new default and
/// refreshes the tray itself. With no session in the map there is nothing to
/// flip, so the config default is persisted directly here and the menu is
/// refreshed to pick up the new checkbox state.
fn toggle_interview(app: &AppHandle) {
    let want = !crate::meeting_ctl::interview_enabled(app);
    match crate::meeting_ctl::current_session_id(app) {
        Some(session_id) => {
            if let Err(e) = crate::meeting_ctl::set_interview(app, &session_id, want) {
                eprintln!("[vzt-flow] failed to toggle interview mode: {e}");
            }
        }
        None => {
            let state = app.state::<AppState>();
            {
                let mut cfg = state.config.lock_or_recover();
                cfg.meeting_interview = want;
                if let Err(e) = cfg.save() {
                    eprintln!("[vzt-flow] failed to save meeting_interview: {e}");
                }
            }
            refresh_menu(app);
        }
    }
}

fn copy_last_transcript(app: &AppHandle, state: &State<AppState>) {
    if let Some(text) = state.last_transcript.lock_or_recover().clone() {
        if let Ok(mut clipboard) = arboard::Clipboard::new() {
            let _ = clipboard.set_text(text);
        }
    } else {
        let _ = app; // nothing to copy yet; menu item stays a no-op
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// All five lifecycle states, plus the "no session yet" case, map to the
    /// label/enabled pair the tray must show — in particular, `Stopping` and
    /// `Finalizing` must read as "Finalizing meeting…" and disabled so the
    /// item never offers "stop" a second time while a summary/PDF export is
    /// in flight.
    #[test]
    fn meeting_toggle_label_reflects_each_session_state() {
        assert_eq!(meeting_toggle_label(None), ("Start meeting transcription", true));
        assert_eq!(
            meeting_toggle_label(Some(&SessionState::Completed)),
            ("Start meeting transcription", true)
        );
        assert_eq!(
            meeting_toggle_label(Some(&SessionState::Failed("boom".to_string()))),
            ("Start meeting transcription", true)
        );
        assert_eq!(
            meeting_toggle_label(Some(&SessionState::Recording)),
            ("Stop meeting transcription (\u{25cf} recording)", true)
        );
        assert_eq!(
            meeting_toggle_label(Some(&SessionState::Stopping)),
            ("Finalizing meeting\u{2026}", false)
        );
        assert_eq!(
            meeting_toggle_label(Some(&SessionState::Finalizing { step: "writing pdf".to_string() })),
            ("Finalizing meeting\u{2026}", false)
        );
    }
}
