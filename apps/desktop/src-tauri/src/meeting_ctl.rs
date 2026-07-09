//! Meeting transcription control for the menu-bar app.
//!
//! Owns the in-process [`MeetingHandle`] (stored in [`AppState`]), drives
//! start/stop with user-facing notifications, and runs the background
//! auto-detector ([`flow_core::meeting::detect`]) that turns a detected
//! Zoom/Meet/Teams call into either an immediate transcription ("auto" mode)
//! or a heads-up notification ("ask" mode).
//!
//! ### Audio paths (no cpal contention with dictation)
//!
//! A meeting session opens its **own** microphone stream (via
//! `flow_core::meeting`'s `run_mic_source`), entirely separate from the
//! dictation audio worker's stream (`flow_core::audio`). On macOS CoreAudio
//! permits multiple concurrent HAL input streams on the same device, so
//! hold-to-talk dictation keeps working *during* a meeting — the two streams
//! are independent taps on the input device, not a shared exclusive handle.
//! (If a platform ever couldn't share the input device, the fix would be to
//! serialize the two; on macOS they coexist.)

use flow_core::config::MeetingAuto;
use flow_core::meeting;
use flow_core::meeting::detect::{self, Debouncer, DetectEvent, MeetingApp};
use tauri::{AppHandle, Manager};
use tauri_plugin_notification::NotificationExt;

use crate::state::AppState;
use crate::tray;

/// Environment override for the meeting-output directory (mirrors the MCP
/// server's `FLOW_MEETINGS_DIR`). Falls back to
/// [`meeting::default_meetings_dir`].
const MEETINGS_DIR_ENV: &str = "FLOW_MEETINGS_DIR";

/// Whether a meeting session is currently running (thread alive).
pub fn is_active(app: &AppHandle) -> bool {
    let state = app.state::<AppState>();
    let guard = state.meeting_session.lock().unwrap();
    guard.as_ref().map(|h| h.is_running()).unwrap_or(false)
}

/// Resolves the meeting output directory: `FLOW_MEETINGS_DIR` if set, else the
/// default `~/Documents/vzt-flow/meetings/`.
fn meetings_dir() -> std::path::PathBuf {
    if let Some(dir) = std::env::var_os(MEETINGS_DIR_ENV) {
        if !dir.is_empty() {
            return std::path::PathBuf::from(dir);
        }
    }
    meeting::default_meetings_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
}

/// Best-effort desktop notification. Failures (e.g. Notifications permission
/// not granted) are logged, never fatal — the tray state is the primary UI.
fn notify(app: &AppHandle, title: &str, body: &str) {
    if let Err(e) = app.notification().builder().title(title).body(body).show() {
        eprintln!("[vzt-flow] notification failed ({title}): {e}");
    }
}

/// Starts a meeting session if one isn't already running. `title` seeds the
/// transcript header/filename; `notify_start` shows the "Transcribing meeting…"
/// banner (used by auto-detect and manual start, suppressed when the caller
/// shows its own message).
pub fn start(app: &AppHandle, title: Option<String>, notify_start: bool) {
    {
        let state = app.state::<AppState>();
        let mut guard = state.meeting_session.lock().unwrap();
        // Reap a session that already finished (e.g. errored out on a missing
        // permission) so a stale handle can't block a fresh start.
        if let Some(h) = guard.as_ref() {
            if h.is_running() {
                eprintln!("[vzt-flow] meeting already in progress; ignoring start");
                return;
            }
        }
        let out_dir = Some(meetings_dir());
        *guard = Some(meeting::start(title.clone(), out_dir));
    }
    tray::refresh_menu(app);
    if notify_start {
        let what = title.unwrap_or_else(|| "meeting".to_string());
        notify(
            app,
            "Transcribing meeting…",
            &format!("VZT Flow is transcribing your {what} locally. Stop it from the menu-bar icon."),
        );
    }
    eprintln!("[vzt-flow] meeting transcription started");
}

/// Stops the running meeting session (if any). Joining the session thread
/// blocks while the summary is generated (10-60s), so the join + completion
/// notification happen on a background thread; the tray flips to "not
/// recording" immediately.
pub fn stop(app: &AppHandle) {
    let handle = {
        let state = app.state::<AppState>();
        let mut guard = state.meeting_session.lock().unwrap();
        guard.take()
    };
    let Some(handle) = handle else {
        return;
    };
    tray::refresh_menu(app);

    let app = app.clone();
    std::thread::Builder::new()
        .name("vzt-flow-meeting-stop".into())
        .spawn(move || match handle.stop() {
            Ok(path) => {
                let name = path
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| path.display().to_string());
                eprintln!("[vzt-flow] meeting transcript ready: {}", path.display());
                notify(
                    &app,
                    "Transcript ready",
                    &format!("{name} saved. Open it from the menu-bar icon › Open meetings folder."),
                );
            }
            Err(e) => {
                eprintln!("[vzt-flow] meeting session ended with error: {e}");
                notify(
                    &app,
                    "Meeting transcription stopped",
                    &format!("The session ended with an error: {e}"),
                );
            }
        })
        .expect("failed to spawn meeting-stop thread");
}

/// Tray toggle: stop if a session is running, otherwise start a manual one.
pub fn toggle(app: &AppHandle) {
    if is_active(app) {
        stop(app);
    } else {
        start(app, None, true);
    }
}

/// Opens the meetings output folder in Finder (macOS `open`, falls back to the
/// platform opener elsewhere). Creates the folder first so `open` never fails
/// on a first run before any meeting has been recorded.
pub fn open_folder(app: &AppHandle) {
    let _ = app; // reserved for a future platform opener; not needed on macOS
    let dir = meetings_dir();
    let _ = std::fs::create_dir_all(&dir);
    #[cfg(target_os = "macos")]
    {
        if let Err(e) = std::process::Command::new("open").arg(&dir).spawn() {
            eprintln!("[vzt-flow] failed to open meetings folder: {e}");
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        eprintln!("[vzt-flow] meetings folder: {}", dir.display());
    }
}

/// Sets the meeting auto-detect mode and persists it. Called from the tray
/// submenu.
pub fn set_auto_mode(app: &AppHandle, mode: MeetingAuto) {
    let state = app.state::<AppState>();
    {
        let mut cfg = state.config.lock().unwrap();
        cfg.meeting_auto = mode.as_str().to_string();
        if let Err(e) = cfg.save() {
            eprintln!("[vzt-flow] failed to save meeting_auto: {e}");
        }
    }
    tray::refresh_menu(app);
}

/// Spawns the background auto-detector thread. It polls the two local signals
/// every [`detect::POLL_INTERVAL`], runs the [`Debouncer`], and reacts to
/// Started/Ended events according to the *current* `meeting_auto` config
/// (re-read each poll, so changing the mode from the tray takes effect
/// immediately without restarting the thread).
///
/// The thread always runs; when the mode is `Off` it simply takes no action on
/// events. This keeps mode-switching instant and the polling cost is trivial
/// (two cheap OS reads every 5s).
pub fn spawn_detector(app: AppHandle) {
    std::thread::Builder::new()
        .name("vzt-flow-meeting-detector".into())
        .spawn(move || detector_loop(app))
        .expect("failed to spawn meeting detector thread");
}

fn detector_loop(app: AppHandle) {
    let mut debouncer = Debouncer::new();
    // Warn once if we can't read window titles — Signal A is inert without the
    // Screen Recording grant, so auto-detect can't work until it's granted.
    let mut warned_no_perm = false;

    loop {
        std::thread::sleep(detect::POLL_INTERVAL);

        let mode = {
            let state = app.state::<AppState>();
            let cfg = state.config.lock().unwrap();
            cfg.meeting_auto_mode()
        };
        if mode == MeetingAuto::Off {
            // Keep the machine from accumulating stale streaks while disabled:
            // reset by feeding it a neutral (no-meeting) poll's worth of state.
            debouncer = Debouncer::new();
            continue;
        }

        if !detect::screen_capture_permitted() {
            if !warned_no_perm {
                eprintln!(
                    "[vzt-flow] meeting auto-detect needs Screen Recording permission to read \
                     window titles (System Settings › Privacy & Security › Screen Recording). \
                     Detection is inactive until it's granted."
                );
                warned_no_perm = true;
            }
            continue;
        }
        warned_no_perm = false;

        let app_match = detect::match_meeting(&detect::list_windows());
        let mic_live = detect::mic_in_use();

        match debouncer.poll(app_match, mic_live) {
            DetectEvent::None => {}
            DetectEvent::Started(which) => on_detected_start(&app, mode, which),
            DetectEvent::Ended => {
                // Only stop a session we (or the user) actually have running.
                if is_active(&app) {
                    eprintln!("[vzt-flow] meeting ended (detector); stopping transcription");
                    stop(&app);
                }
            }
        }
    }
}

/// Handles a detector `Started` event per the active mode.
fn on_detected_start(app: &AppHandle, mode: MeetingAuto, which: MeetingApp) {
    if is_active(app) {
        // A session (manual or prior) is already running — don't double-start
        // or nag. The detector's Ended will stop it later.
        return;
    }
    let label = format!("{} meeting", which.label());
    match mode {
        MeetingAuto::Auto => {
            eprintln!("[vzt-flow] {} detected; auto-starting transcription", which.label());
            start(app, Some(label), true);
        }
        MeetingAuto::Ask => {
            eprintln!("[vzt-flow] {} detected; prompting to transcribe", which.label());
            // The bundled Tauri notification plugin has no reliable
            // cross-version action-button/click callback, so the "ask" prompt
            // instructs the user to click the tray item rather than offering
            // an in-notification button. (Documented in docs/MEETINGS.md.)
            notify(
                app,
                &format!("{} call detected", which.label()),
                "Start transcribing? Click the VZT Flow menu-bar icon › Start meeting transcription.",
            );
        }
        MeetingAuto::Off => {}
    }
}
