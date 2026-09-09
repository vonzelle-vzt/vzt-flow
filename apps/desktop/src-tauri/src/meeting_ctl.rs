//! Meeting transcription control for the menu-bar app.
//!
//! Owns the desktop's meeting **sessions** (the [`Meetings`] slots in
//! [`AppState`]), drives start/stop with user-facing notifications, and runs
//! the background auto-detector ([`flow_core::meeting::detect`]) that turns a
//! detected Zoom/Meet/Teams call into either an immediate transcription
//! ("auto" mode) or a heads-up notification ("ask" mode).
//!
//! ### Session identity
//!
//! `meeting::start_with` reserves the transcript file on *this* thread and
//! fires `on_started` before the session thread exists, so the session id, the
//! transcript path and the notes sidecar are known synchronously. The slot is
//! inserted from inside that callback, which is what makes it impossible for
//! the first transcript line to arrive before the slot it belongs in.
//!
//! A slot outlives its `MeetingHandle`: [`stop`] takes the handle and moves it
//! onto `vzt-flow-meeting-stop`, and the finalize result is written back into
//! the slot **by session id**. That is why a new session never overwrites an
//! old one, and why `Meetings::retire` refuses to evict an unfinished slot.
//!
//! ### Threads (gotchas (h) and (i))
//!
//! - `on_started` runs on the caller's thread, `on_line`/`on_state` on the
//!   session thread, `on_tip` on the coach thread. None of them is the main
//!   thread, so **no window operation may happen in one** — `emit_to` is the
//!   only Tauri call they make, and every tray refresh goes through
//!   [`refresh_tray`] → `run_on_main_thread`.
//! - `on_line` runs *while the transcript writer's mutex is held*: it takes
//!   the `meetings` lock just long enough to push into the slot's ring, and
//!   never blocks on anything else.
//! - `MeetingHandle::stop_detailed` blocks for 10-60s (summary + PDF), so it
//!   only ever runs on `vzt-flow-meeting-stop`, never on the coordinator, the
//!   detector or the main thread.
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

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use flow_core::config::MeetingAuto;
use flow_core::llm::ManagerGenerator;
use flow_core::meeting;
use flow_core::meeting::detect::{self, Debouncer, DetectEvent, MeetingApp};
use flow_core::meeting::{
    notes, pdf, summary, InterviewTip, MeetingOptions, MeetingOutcome, SessionStarted, SessionState,
    TranscriptLine,
};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_notification::NotificationExt;

use crate::state::{should_auto_stop, AppState, LockRecover, Meetings, SessionSlot};
use crate::tray;

/// Environment override for the meeting-output directory (mirrors the MCP
/// server's `FLOW_MEETINGS_DIR`). Falls back to
/// [`meeting::default_meetings_dir`].
const MEETINGS_DIR_ENV: &str = "FLOW_MEETINGS_DIR";

/// The notepad window's label — the fixed contract shared with U11's
/// `notepad::NOTES_LABEL`. Declared here (rather than imported) because this
/// module emits to the window whether or not it has been created yet;
/// `emit_to` for a label with no window is a no-op.
pub const NOTES_WINDOW: &str = "notes";

/// Events emitted to the notepad window. Every one carries `session_id`, so a
/// notepad bound to a finished session can drop anything that isn't its own.
pub const EVENT_BIND: &str = "meeting://bind";
pub const EVENT_STATE: &str = "meeting://state";
pub const EVENT_LINE: &str = "meeting://line";
pub const EVENT_TIP: &str = "meeting://tip";
#[allow(dead_code)] // emitted by `save_notes` (a U11 seam).
pub const EVENT_NOTES_STATUS: &str = "meeting://notes-status";

/// Returned to the notepad when it asks to act on a session this process no
/// longer holds (it was retired, or the window is bound to an older meeting).
#[allow(dead_code)] // returned by the U11 seams below.
pub const FOREIGN_SESSION: &str = "this window is bound to a finished session";

/// Re-export rewrites the transcript in place (`notes::replace_notes_section`
/// renames a new file over it), which would strand the capture writer's open
/// file descriptor — so it is refused until the session has finalized.
#[allow(dead_code)] // returned by `reexport` (a U11 seam).
pub const STILL_RUNNING: &str = "this meeting is still running; stop it before re-exporting";

/// Re-export renders over the PDF the finalize path wrote. Without one there
/// is no filename to update (and no `chrono` in this crate to derive one).
#[allow(dead_code)] // returned by `reexport` (a U11 seam).
pub const NO_PDF_TO_UPDATE: &str =
    "no PDF was exported for this meeting; turn PDF export on before the next one";

/// A session whose thread exited without reaching a terminal state — capture
/// failed (usually a missing Screen Recording grant). Reaped so it can't block
/// every later start.
const DEAD_SESSION: &str = "the session ended before it could finalize";

// ---------------------------------------------------------------------------
// Event payloads (the notepad's wire contract).
// ---------------------------------------------------------------------------

/// `meeting://bind` — tells the notepad which session it now belongs to.
#[derive(Clone, Serialize)]
pub struct BindPayload {
    pub session_id: String,
    pub title: String,
    pub started_at_ms: i64,
    pub interview: bool,
}

/// `meeting://state` — one lifecycle transition. The paths are populated only
/// on the terminal transition, where they are known.
#[derive(Clone, Serialize)]
pub struct StatePayload {
    pub session_id: String,
    pub state: SessionState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pdf_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `meeting://notes-status` — the result of one notes save.
#[derive(Clone, Serialize)]
#[allow(dead_code)] // constructed by `save_notes` (a U11 seam).
pub struct NotesStatus {
    pub session_id: String,
    pub ok: bool,
    pub rev: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Small shared helpers.
// ---------------------------------------------------------------------------

/// Resolves the meeting output directory: `FLOW_MEETINGS_DIR` if set, else the
/// default `~/Documents/vzt-flow/meetings/`.
fn meetings_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(MEETINGS_DIR_ENV) {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    meeting::default_meetings_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Best-effort desktop notification. Failures (e.g. Notifications permission
/// not granted) are logged, never fatal — the tray state is the primary UI.
fn notify(app: &AppHandle, title: &str, body: &str) {
    if let Err(e) = app.notification().builder().title(title).body(body).show() {
        eprintln!("[vzt-flow] notification failed ({title}): {e}");
    }
}

/// Rebuilds the tray menu **on the main thread**.
///
/// Every caller in this module can be a worker: the detector thread, the
/// session thread (via `on_state`) and `vzt-flow-meeting-stop` all refresh the
/// menu. Menu construction is an AppKit operation, so it is marshalled rather
/// than called inline (gotcha (h)).
fn refresh_tray(app: &AppHandle) {
    let handle = app.clone();
    if let Err(e) = app.run_on_main_thread(move || tray::refresh_menu(&handle)) {
        eprintln!("[vzt-flow] could not refresh the tray menu: {e}");
    }
}

/// Fire-and-forget emit to the notepad window. Safe from any thread and a
/// no-op while the window doesn't exist.
fn emit_notes<T: Serialize + Clone>(app: &AppHandle, event: &str, payload: T) {
    let _ = app.emit_to(NOTES_WINDOW, event, payload);
}

/// Emits one `meeting://state`.
fn emit_state(app: &AppHandle, session_id: &str, state: SessionState, outcome: Option<&MeetingOutcome>) {
    let error = match &state {
        SessionState::Failed(e) => Some(e.clone()),
        _ => None,
    };
    emit_notes(
        app,
        EVENT_STATE,
        StatePayload {
            session_id: session_id.to_string(),
            state,
            transcript_path: outcome.map(|o| o.transcript.display().to_string()),
            pdf_path: outcome.and_then(|o| o.pdf.as_ref()).map(|p| p.display().to_string()),
            error: error.or_else(|| outcome.and_then(|o| o.pdf_error.clone())),
        },
    );
}

/// Looks a session up for a notepad-initiated action, refusing an id we no
/// longer hold. Pure (takes the map, not the app), so the guard is testable
/// without a Tauri runtime.
#[allow(dead_code)] // used by the U11 seams below.
fn slot_for<'a>(meetings: &'a mut Meetings, session_id: &str) -> Result<&'a mut SessionSlot, String> {
    meetings
        .get_mut(session_id)
        .ok_or_else(|| FOREIGN_SESSION.to_string())
}

// ---------------------------------------------------------------------------
// Read-only accessors (used by the tray and, from U11, the notepad commands).
// ---------------------------------------------------------------------------

/// Whether a meeting is currently capturing audio. `Stopping`/`Finalizing`
/// deliberately read as **not** active — the microphone is closed by then and
/// the tray must not offer "stop" a second time.
pub fn is_active(app: &AppHandle) -> bool {
    with_slot(app, None, |slot| slot.is_recording()).unwrap_or(false)
}

/// The id of the newest session, alive or finished.
#[allow(dead_code)] // U12 (tray) / U11 seam.
pub fn current_session_id(app: &AppHandle) -> Option<String> {
    with_slot(app, None, |slot| slot.session_id.clone())
}

/// The newest session's lifecycle state, for the tray's meeting item.
#[allow(dead_code)] // U12 (tray) seam.
pub fn current_state(app: &AppHandle) -> Option<SessionState> {
    with_slot(app, None, |slot| slot.state.clone())
}

/// Interview mode as the UI should show it: the live session's flag while one
/// exists, otherwise the persisted config default.
#[allow(dead_code)] // U12 (tray) seam.
pub fn interview_enabled(app: &AppHandle) -> bool {
    if let Some(on) = with_slot(app, None, |slot| slot.interview.load(Ordering::Relaxed)) {
        return on;
    }
    app.state::<AppState>().config.lock_or_recover().meeting_interview
}

/// Runs `f` over one session slot under the `meetings` lock, returning `None`
/// when there is no such session. `session_id: None` means the current one.
///
/// This is the read seam for U11's `get_meeting_snapshot`: build the snapshot
/// inside the closure (`slot.lines`, `slot.last_seq`, `slot.notes_rev`, …) and
/// read the notes sidecar with `notes::read(&slot.notes)` *after* it returns —
/// the lock is on the session thread's `on_line` path and must not be held
/// across file I/O.
pub fn with_slot<R>(
    app: &AppHandle,
    session_id: Option<&str>,
    f: impl FnOnce(&SessionSlot) -> R,
) -> Option<R> {
    let state = app.state::<AppState>();
    let meetings = state.meetings.lock_or_recover();
    let slot = match session_id {
        Some(id) => meetings.get(id)?,
        None => meetings.current()?,
    };
    Some(f(slot))
}

/// The `meeting://bind` payload for a session, for U11's `notepad::open`.
#[allow(dead_code)] // U11 (notepad::open) seam.
pub fn bind_payload(app: &AppHandle, session_id: &str) -> Option<BindPayload> {
    with_slot(app, Some(session_id), |slot| BindPayload {
        session_id: slot.session_id.clone(),
        title: slot.title.clone(),
        started_at_ms: slot.started_at_ms,
        interview: slot.interview.load(Ordering::Relaxed),
    })
}

// ---------------------------------------------------------------------------
// Start.
// ---------------------------------------------------------------------------

/// What [`start`] may do, given what is already in the slots.
enum StartGate {
    /// No session is in the way.
    Clear,
    /// The named session's thread has already exited without finalizing;
    /// mark it failed and start anyway.
    Reap(String),
    /// Refuse, with the reason to log.
    Busy(&'static str),
}

/// Decides whether a new session may start. Pure: takes the map, not the app.
fn start_gate(meetings: &Meetings) -> StartGate {
    let Some(slot) = meetings.current() else {
        return StartGate::Clear;
    };
    if slot.is_finished() {
        return StartGate::Clear;
    }
    match slot.handle.as_ref() {
        // Recording, and the session thread is alive.
        Some(h) if h.is_running() => StartGate::Busy("meeting already in progress"),
        // The handle is still ours but the thread is gone: capture failed
        // (usually a missing Screen Recording grant). Reap it rather than let
        // a dead session block every later start, as the pre-slot code did.
        Some(_) => StartGate::Reap(slot.session_id.clone()),
        // The handle was taken by `stop`: finalize is in flight on
        // `vzt-flow-meeting-stop` and will write back into this slot.
        None => StartGate::Busy("a meeting is still finalizing"),
    }
}

/// Starts a meeting session unless one is already running.
///
/// `title` seeds the transcript header/filename; `notify_start` shows the
/// "Transcribing meeting…" banner; `detection_owned` marks the session as the
/// auto-detector's, which is the only kind its `Ended` event may stop.
///
/// A live session is **never** overwritten: unlike the pre-U10 code (which
/// replaced `Option<MeetingHandle>` and could drop a running handle — the
/// session thread would then keep writing forever, unstoppable and never
/// finalized), this refuses the start and logs why.
pub fn start(app: &AppHandle, title: Option<String>, notify_start: bool, detection_owned: bool) {
    let state = app.state::<AppState>();

    let gate = { start_gate(&state.meetings.lock_or_recover()) };
    match gate {
        StartGate::Clear => {}
        StartGate::Busy(why) => {
            eprintln!("[vzt-flow] {why}; ignoring start");
            return;
        }
        StartGate::Reap(id) => {
            {
                let mut meetings = state.meetings.lock_or_recover();
                if let Some(slot) = meetings.get_mut(&id) {
                    slot.handle = None;
                    slot.state = SessionState::Failed(DEAD_SESSION.to_string());
                    slot.last_error = Some(DEAD_SESSION.to_string());
                }
            }
            eprintln!("[vzt-flow] meeting session {id} exited on its own; reaping it");
            emit_state(app, &id, SessionState::Failed(DEAD_SESSION.to_string()), None);
        }
    }

    // --- options from config ----------------------------------------------
    let (interview_on, pdf_opts, summary_opts) = {
        let cfg = state.config.lock_or_recover();
        let pdf_opts = cfg
            .meeting_pdf_dir_resolved()
            .map(|dir| pdf::PdfOptions { dir, enabled: cfg.meeting_pdf });
        let summary_opts = summary::SummaryOptions {
            window_chars: cfg.meeting_summary_window_chars,
            partial_timeout_ms: cfg.meeting_summary_partial_timeout_ms,
            ..Default::default()
        };
        (cfg.meeting_interview, pdf_opts, summary_opts)
    };

    // One resident model per process: the session summarizes and coaches
    // through the desktop's own cleanup manager rather than loading a second
    // copy of the LLM (`generator: None` would make flow-core build its own).
    let generator = state
        .cleanup_cmd_tx
        .lock_or_recover()
        .clone()
        .map(|tx| Arc::new(ManagerGenerator::new(tx)) as Arc<dyn flow_core::llm::TextGenerator>);
    if generator.is_none() {
        eprintln!(
            "[vzt-flow] no cleanup manager yet; the meeting will load its own generator for the summary"
        );
    }

    let interview = Arc::new(AtomicBool::new(interview_on));

    // `on_started` fires synchronously inside `start_with`, before the session
    // thread exists. Inserting the slot from there is what guarantees the slot
    // is present before the first `on_line`; the cell hands the identity back
    // to this thread so the handle can be attached afterwards.
    let started_cell: Arc<Mutex<Option<SessionStarted>>> = Arc::new(Mutex::new(None));

    let on_started = {
        let app = app.clone();
        let cell = Arc::clone(&started_cell);
        let interview = Arc::clone(&interview);
        Arc::new(move |started: &SessionStarted| {
            *cell.lock_or_recover() = Some(started.clone());
            let state = app.state::<AppState>();
            state
                .meetings
                .lock_or_recover()
                .insert(SessionSlot::new(started, Arc::clone(&interview), detection_owned));
            emit_notes(
                &app,
                EVENT_BIND,
                BindPayload {
                    session_id: started.session_id.clone(),
                    title: started.title.clone(),
                    started_at_ms: started.started_at_ms,
                    interview: interview.load(Ordering::Relaxed),
                },
            );
        })
    };

    // Runs on the session thread while the transcript writer's mutex is held:
    // a short lock and an emit, nothing that can block.
    let on_line = {
        let app = app.clone();
        Arc::new(move |line: &TranscriptLine| {
            {
                let state = app.state::<AppState>();
                let mut meetings = state.meetings.lock_or_recover();
                if let Some(slot) = meetings.get_mut(&line.session_id) {
                    slot.push_line(line.clone());
                }
            }
            emit_notes(&app, EVENT_LINE, line.clone());
        })
    };

    let on_state = {
        let app = app.clone();
        let cell = Arc::clone(&started_cell);
        Arc::new(move |st: &SessionState| {
            let Some(session_id) = cell.lock_or_recover().as_ref().map(|s| s.session_id.clone())
            else {
                return;
            };
            {
                let state = app.state::<AppState>();
                let mut meetings = state.meetings.lock_or_recover();
                if let Some(slot) = meetings.get_mut(&session_id) {
                    slot.state = st.clone();
                }
            }
            // The authoritative terminal event (with the transcript/PDF paths)
            // is emitted by `finish_session` once the outcome is in hand; this
            // one carries the progress steps the notepad renders meanwhile.
            emit_state(&app, &session_id, st.clone(), None);
            refresh_tray(&app);
        })
    };

    // The coach thread is spawned whenever `on_tip` is set, and gates each
    // question on the live `interview` flag — so it is wired unconditionally,
    // which is what makes toggling interview mode mid-meeting work.
    let on_tip = {
        let app = app.clone();
        Arc::new(move |tip: &InterviewTip| {
            {
                let state = app.state::<AppState>();
                let mut meetings = state.meetings.lock_or_recover();
                if let Some(slot) = meetings.get_mut(&tip.session_id) {
                    slot.last_tip = Some(tip.clone());
                }
            }
            // Deliberately only an emit: a tip must never show or focus the
            // notepad, or an arriving suggestion would steal focus from Zoom.
            emit_notes(&app, EVENT_TIP, tip.clone());
        })
    };

    let opts = MeetingOptions {
        title: title.clone(),
        out_dir: Some(meetings_dir()),
        transcript_path: None,
        on_started: Some(on_started),
        on_line: Some(on_line),
        on_state: Some(on_state),
        on_tip: Some(on_tip),
        generator,
        interview,
        interview_context: Some(meeting::interview::load_context()),
        pdf: pdf_opts,
        summary: summary_opts,
    };

    let handle = match meeting::start_with(opts) {
        Ok(handle) => handle,
        Err(e) => {
            // `on_started` fires after the transcript is reserved, so a
            // failure here can still have left a slot behind (only the thread
            // spawn can fail that late). Mark it rather than leak a phantom
            // "recording" session.
            let started = started_cell.lock_or_recover().clone();
            if let Some(started) = started {
                let mut meetings = state.meetings.lock_or_recover();
                if let Some(slot) = meetings.get_mut(&started.session_id) {
                    slot.state = SessionState::Failed(e.to_string());
                    slot.last_error = Some(e.to_string());
                }
            }
            eprintln!("[vzt-flow] failed to start meeting transcription: {e}");
            notify(app, "Meeting transcription failed", &e.to_string());
            return;
        }
    };

    let session_id = handle.session_id().to_string();
    {
        let mut meetings = state.meetings.lock_or_recover();
        match meetings.get_mut(&session_id) {
            Some(slot) => slot.handle = Some(handle),
            None => {
                // `on_started` never ran (it is panic-guarded in flow-core) —
                // rebuild the slot from the handle so the session is still
                // stoppable rather than orphaned.
                eprintln!("[vzt-flow] meeting {session_id} had no slot; rebuilding it from the handle");
                let started = SessionStarted {
                    session_id: session_id.clone(),
                    title: title.clone().unwrap_or_else(|| "meeting".to_string()),
                    started_at_ms: now_ms(),
                    transcript: handle.transcript_path().to_path_buf(),
                    notes: handle.notes_path().to_path_buf(),
                };
                let mut slot = SessionSlot::new(&started, handle.interview_flag(), detection_owned);
                slot.handle = Some(handle);
                meetings.insert(slot);
            }
        }
    }

    refresh_tray(app);
    maybe_open_notepad(app, &session_id);

    if notify_start {
        let what = title.unwrap_or_else(|| "meeting".to_string());
        notify(
            app,
            "Transcribing meeting…",
            &format!("VZT Flow is transcribing your {what} locally. Stop it from the menu-bar icon."),
        );
    }
    eprintln!("[vzt-flow] meeting transcription started ({session_id})");
}

/// Opens the live notepad for a session, at most once per session.
///
/// U11 owns the window itself; this is the single place that decides *when* it
/// opens, so the policy (config-gated, once per session, only after a session
/// actually starts — never on an `ask`/`off` detection) lives with the
/// lifecycle rather than in the window module.
fn maybe_open_notepad(app: &AppHandle, session_id: &str) {
    let state = app.state::<AppState>();
    let wanted = state.config.lock_or_recover().meeting_notepad;
    if !wanted {
        return;
    }
    {
        let mut meetings = state.meetings.lock_or_recover();
        let Some(slot) = meetings.get_mut(session_id) else {
            return;
        };
        if slot.notepad_opened {
            return;
        }
        slot.notepad_opened = true;
    }
    // U11: open the notepad here — `crate::notepad::open(app, session_id)`.
    // It must marshal every window operation through `run_on_main_thread`
    // (gotcha (h)); this function is called from `start`, which the detector
    // thread also calls. The `meeting://bind` payload is already emitted by
    // `on_started`, and [`bind_payload`] rebuilds it for a re-open.
}

// ---------------------------------------------------------------------------
// Stop / finalize.
// ---------------------------------------------------------------------------

/// Stops the running meeting session (if any).
///
/// Finalize (flush tails, merge notes, summarize, export the PDF) blocks for
/// 10-60s, so the handle is moved onto `vzt-flow-meeting-stop`; the tray and
/// the notepad flip to `Stopping` immediately. The completion notification is
/// sent by [`finish_session`] **after** the outcome is known, so it can name
/// the files that were actually written.
pub fn stop(app: &AppHandle) {
    let state = app.state::<AppState>();
    let taken = {
        let mut meetings = state.meetings.lock_or_recover();
        match meetings.current_mut() {
            Some(slot) => match slot.handle.take() {
                Some(handle) => {
                    slot.state = SessionState::Stopping;
                    Some((slot.session_id.clone(), handle))
                }
                // Already finalizing (or long finished) — nothing to stop.
                None => None,
            },
            None => None,
        }
    };
    let Some((session_id, handle)) = taken else {
        return;
    };

    emit_state(app, &session_id, SessionState::Stopping, None);
    refresh_tray(app);

    let app = app.clone();
    std::thread::Builder::new()
        .name("vzt-flow-meeting-stop".into())
        .spawn(move || {
            let result = handle.stop_detailed();
            finish_session(&app, &session_id, result);
        })
        .expect("failed to spawn meeting-stop thread");
}

/// Records a finished session: outcome into the slot, terminal state out to
/// the notepad and the tray, and only then the notification.
fn finish_session(app: &AppHandle, session_id: &str, result: anyhow::Result<MeetingOutcome>) {
    let state = app.state::<AppState>();

    let outcome = match result {
        Ok(mut outcome) => {
            // flow-core has no notes revision counter (it always reports 0);
            // the slot is the authority, so the outcome is corrected from it.
            let rev = with_slot(app, Some(session_id), |slot| slot.notes_rev).unwrap_or(0);
            outcome.notes_rev_used = rev;
            Some(outcome)
        }
        Err(e) => {
            eprintln!("[vzt-flow] meeting session ended with error: {e}");
            let msg = e.to_string();
            {
                let mut meetings = state.meetings.lock_or_recover();
                if let Some(slot) = meetings.get_mut(session_id) {
                    slot.handle = None;
                    slot.state = SessionState::Failed(msg.clone());
                    slot.last_error = Some(msg.clone());
                }
                meetings.retire();
            }
            emit_state(app, session_id, SessionState::Failed(msg.clone()), None);
            refresh_tray(app);
            notify(
                app,
                "Meeting transcription stopped",
                &format!("The session ended with an error: {msg}"),
            );
            return;
        }
    };
    let outcome = outcome.expect("the error path returned above");

    // The summary markdown is read back out of the transcript so a re-export
    // can re-render the PDF without a second LLM pass.
    let summary_markdown = std::fs::read_to_string(&outcome.transcript)
        .ok()
        .and_then(|md| extract_summary_section(&md).0);

    {
        let mut meetings = state.meetings.lock_or_recover();
        if let Some(slot) = meetings.get_mut(session_id) {
            slot.handle = None;
            slot.state = SessionState::Completed;
            slot.notes_rev_used = Some(outcome.notes_rev_used);
            slot.summary_markdown = summary_markdown;
            slot.outcome = Some(outcome.clone());
        }
        meetings.retire();
    }

    eprintln!(
        "[vzt-flow] meeting transcript ready: {}",
        outcome.transcript.display()
    );
    emit_state(app, session_id, SessionState::Completed, Some(&outcome));
    refresh_tray(app);
    notify(
        app,
        "Transcript ready",
        &completion_body(&outcome.transcript, outcome.pdf.as_deref(), outcome.pdf_error.as_deref()),
    );
}

/// Body of the "Transcript ready" notification.
///
/// Names both files when both exist, and says plainly what happened when the
/// PDF didn't — a notification that claimed a PDF that was never written is
/// exactly the kind of unverifiable claim this project doesn't ship. Pure so
/// the wording is testable.
fn completion_body(transcript: &Path, pdf: Option<&Path>, pdf_error: Option<&str>) -> String {
    let md = file_name_of(transcript);
    match (pdf, pdf_error) {
        (Some(pdf_path), _) => {
            let name = file_name_of(pdf_path);
            let where_ = match pdf_path.parent() {
                Some(dir) if dir.file_name().map(|d| d == "Desktop").unwrap_or(false) => {
                    "on your Desktop".to_string()
                }
                Some(dir) => format!("in {}", dir.display()),
                None => "saved".to_string(),
            };
            format!("{md} saved · {name} {where_}")
        }
        (None, Some(err)) => format!("{md} saved. The PDF could not be written: {err}"),
        (None, None) => {
            format!("{md} saved. Open it from the menu-bar icon › Open meetings folder.")
        }
    }
}

fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Milliseconds since the epoch. Only a fallback for the (unreachable in
/// practice) case where `on_started` didn't run — `chrono` isn't a dependency
/// of this crate, and the real timestamp comes from `SessionStarted`.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Tray toggle: stop if a session is recording, otherwise start a manual one.
/// A session that is already finalizing is left alone — stopping it twice is
/// not a thing, and starting the next meeting on top of it is refused anyway.
pub fn toggle(app: &AppHandle) {
    match with_slot(app, None, |slot| (slot.is_recording(), slot.is_finished())) {
        Some((true, _)) => stop(app),
        Some((false, false)) => {
            eprintln!("[vzt-flow] meeting is still finalizing; ignoring toggle")
        }
        _ => start(app, None, true, false),
    }
}

// ---------------------------------------------------------------------------
// Notepad-initiated actions (U11's commands call these).
// ---------------------------------------------------------------------------

/// Saves the notepad's text to the session's `.notes.txt` sidecar and returns
/// the new revision.
///
/// Works after `Completed` too — a late edit is never refused, it just doesn't
/// move `notes_rev_used` (the exported files then visibly trail the sidecar
/// until a re-export). The write happens **outside** the `meetings` lock: the
/// session thread's `on_line` takes that lock on every transcript line.
#[allow(dead_code)] // U11 (save_meeting_notes) seam.
pub fn save_notes(app: &AppHandle, session_id: &str, text: &str) -> Result<u64, String> {
    let state = app.state::<AppState>();
    let path = {
        let mut meetings = state.meetings.lock_or_recover();
        slot_for(&mut meetings, session_id)?.notes.clone()
    };

    if let Err(e) = notes::save_atomic(&path, text) {
        let msg = e.to_string();
        eprintln!("[vzt-flow] failed to save meeting notes: {msg}");
        emit_notes(
            app,
            EVENT_NOTES_STATUS,
            NotesStatus {
                session_id: session_id.to_string(),
                ok: false,
                rev: with_slot(app, Some(session_id), |s| s.notes_rev).unwrap_or(0),
                error: Some(msg.clone()),
            },
        );
        return Err(msg);
    }

    let rev = {
        let mut meetings = state.meetings.lock_or_recover();
        slot_for(&mut meetings, session_id)?.bump_notes_rev()
    };
    emit_notes(
        app,
        EVENT_NOTES_STATUS,
        NotesStatus { session_id: session_id.to_string(), ok: true, rev, error: None },
    );
    Ok(rev)
}

/// Flips interview mode for a session and persists it as the new default.
///
/// The slot's flag is the *same* `Arc<AtomicBool>` the running session reads,
/// so this takes effect mid-meeting: the coach starts (or stops) answering
/// questions and the `Them` silence hold tightens without a restart.
#[allow(dead_code)] // U11 (set_interview_mode) / U12 seam.
pub fn set_interview(app: &AppHandle, session_id: &str, on: bool) -> Result<(), String> {
    let state = app.state::<AppState>();
    {
        let mut meetings = state.meetings.lock_or_recover();
        slot_for(&mut meetings, session_id)?
            .interview
            .store(on, Ordering::SeqCst);
    }
    {
        let mut cfg = state.config.lock_or_recover();
        cfg.meeting_interview = on;
        if let Err(e) = cfg.save() {
            eprintln!("[vzt-flow] failed to save meeting_interview: {e}");
        }
    }
    refresh_tray(app);
    Ok(())
}

/// Re-merges the notes sidecar into the transcript and re-renders the PDF from
/// the **stored** summary — no LLM call, so it is fast enough for a button.
///
/// Refused while the session is still running: `notes::replace_notes_section`
/// publishes by rename, which would leave the capture writer appending to the
/// replaced inode (flow-core's own merge runs only after the writer closes).
#[allow(dead_code)] // U11 (reexport_meeting) seam.
pub fn reexport(app: &AppHandle, session_id: &str) -> Result<PathBuf, String> {
    let state = app.state::<AppState>();
    let (transcript, notes_path, title, stored_summary, pdf_path, rev) = {
        let mut meetings = state.meetings.lock_or_recover();
        let slot = slot_for(&mut meetings, session_id)?;
        if !slot.is_finished() {
            return Err(STILL_RUNNING.to_string());
        }
        let pdf_path = slot.outcome.as_ref().and_then(|o| o.pdf.clone());
        (
            slot.transcript.clone(),
            slot.notes.clone(),
            slot.title.clone(),
            slot.summary_markdown.clone(),
            pdf_path,
            slot.notes_rev,
        )
    };
    let pdf_path = pdf_path.ok_or_else(|| NO_PDF_TO_UPDATE.to_string())?;

    let typed = notes::read(&notes_path);
    let markdown = std::fs::read_to_string(&transcript).map_err(|e| e.to_string())?;
    let merged = notes::replace_notes_section(&markdown, &typed);
    notes::save_atomic(&transcript, &merged).map_err(|e| e.to_string())?;

    let (extracted, coverage) = extract_summary_section(&merged);
    let (transcript_lines, duration) = transcript_lines_and_duration(&merged);
    let doc = pdf::MeetingDoc {
        title,
        date_line: header_date(&merged).unwrap_or_default(),
        duration,
        summary_md: stored_summary.or(extracted),
        notes: (!typed.trim().is_empty()).then(|| typed.clone()),
        transcript_lines,
        coverage_note: coverage,
        source_path: transcript.clone(),
    };
    let bytes = pdf::render(&doc);
    write_over_atomically(&pdf_path, &bytes)?;

    {
        let mut meetings = state.meetings.lock_or_recover();
        if let Some(slot) = meetings.get_mut(session_id) {
            slot.notes_rev_used = Some(rev);
            if let Some(outcome) = slot.outcome.as_mut() {
                outcome.notes_rev_used = rev;
                outcome.notes_merged = true;
            }
        }
    }
    eprintln!("[vzt-flow] re-exported meeting {session_id} -> {}", pdf_path.display());
    Ok(pdf_path)
}

/// Writes `bytes` over an existing file via a fsynced sibling temp + rename,
/// so a reader never sees a half-written PDF.
///
/// Deliberately not `pdf::write_atomic`, which reserves a *new* name with
/// `create_new` (a re-export would pile up "… -2.pdf", "… -3.pdf"); a
/// re-export updates the file the user already has.
#[allow(dead_code)] // used by `reexport`.
fn write_over_atomically(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;

    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = parent.join(format!(".{name}.{}.{nanos}.tmp", std::process::id()));

    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result.map_err(|e| format!("failed to write {}: {e}", path.display()))
}

// ---------------------------------------------------------------------------
// Transcript parsing (pure) — the re-export path's half of what `finalize`
// does internally with private helpers.
// ---------------------------------------------------------------------------

/// Extracts the `HH:MM:SS` from a `[HH:MM:SS] Speaker: ...` transcript line.
/// Mirrors `meeting::parse_leading_timestamp`, which is private to flow-core.
fn leading_timestamp(line: &str) -> Option<&str> {
    let rest = line.strip_prefix('[')?;
    let close = rest.find(']')?;
    let ts = &rest[..close];
    let parts: Vec<&str> = ts.split(':').collect();
    if parts.len() == 3 && parts.iter().all(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_digit())) {
        Some(ts)
    } else {
        None
    }
}

/// The timestamped transcript lines and the meeting length (the last
/// timestamp seen), as the PDF body wants them.
#[allow(dead_code)] // used by `reexport`.
fn transcript_lines_and_duration(markdown: &str) -> (Vec<String>, String) {
    let mut lines = Vec::new();
    let mut duration = None;
    for line in markdown.lines() {
        if let Some(ts) = leading_timestamp(line) {
            duration = Some(ts.to_string());
            lines.push(line.to_string());
        }
    }
    (lines, duration.unwrap_or_else(|| "00:00:00".to_string()))
}

/// The date part of the `# Meeting: <title> — <datetime>` header.
#[allow(dead_code)] // used by `reexport`.
fn header_date(markdown: &str) -> Option<String> {
    markdown
        .lines()
        .find_map(|l| l.strip_prefix("# Meeting: "))
        .and_then(|h| h.split_once(" — "))
        .map(|(_, dt)| dt.trim().to_string())
}

/// Splits a finalized transcript's tail into `(summary markdown, coverage
/// note)`.
///
/// `finalize` appends the summary after everything else — after the merged
/// notes section when there is one, otherwise after the last transcript line —
/// and closes with an italic coverage line. Recovering it from the file is how
/// a re-export re-renders the PDF without asking the model again;
/// `MeetingOutcome` carries only the section count, not the text.
fn extract_summary_section(markdown: &str) -> (Option<String>, Option<String>) {
    let mut tail_start = 0usize;
    let mut offset = 0usize;
    for line in markdown.split_inclusive('\n') {
        let exact = line.strip_suffix('\n').unwrap_or(line);
        let exact = exact.strip_suffix('\r').unwrap_or(exact);
        if leading_timestamp(exact).is_some() || exact == notes::NOTES_END {
            tail_start = offset + line.len();
        }
        offset += line.len();
    }

    let tail = markdown[tail_start..].trim();
    if tail.is_empty() {
        return (None, None);
    }

    let mut body: Vec<&str> = tail.lines().collect();
    let mut coverage = None;
    if let Some(last) = body.last().map(|l| l.trim()) {
        if last.len() >= 2 && last.starts_with('_') && last.ends_with('_') {
            coverage = Some(last[1..last.len() - 1].to_string());
            body.pop();
        }
    }
    let summary = body.join("\n").trim().to_string();
    ((!summary.is_empty()).then_some(summary), coverage)
}

// ---------------------------------------------------------------------------
// Folder / mode / detector.
// ---------------------------------------------------------------------------

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
        let mut cfg = state.config.lock_or_recover();
        cfg.meeting_auto = mode.as_str().to_string();
        if let Err(e) = cfg.save() {
            eprintln!("[vzt-flow] failed to save meeting_auto: {e}");
        }
    }
    refresh_tray(app);
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
            let cfg = state.config.lock_or_recover();
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
            DetectEvent::Ended => on_detected_end(&app),
        }
    }
}

/// Handles a detector `Started` event per the active mode.
fn on_detected_start(app: &AppHandle, mode: MeetingAuto, which: MeetingApp) {
    if with_slot(app, None, |slot| !slot.is_finished()).unwrap_or(false) {
        // A session (manual or prior) is already running or finalizing — don't
        // double-start or nag. The detector's Ended will stop it later if it
        // is ours to stop.
        return;
    }
    let label = format!("{} meeting", which.label());
    match mode {
        MeetingAuto::Auto => {
            eprintln!("[vzt-flow] {} detected; auto-starting transcription", which.label());
            start(app, Some(label), true, /* detection_owned = */ true);
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

/// Handles a detector `Ended` event: the detector may only stop what the
/// detector started (requirement 6). A meeting the user started from the tray
/// survives Zoom quitting — a mis-detection must never end a recording
/// somebody is relying on.
fn on_detected_end(app: &AppHandle) {
    let Some((auto, recording)) =
        with_slot(app, None, |slot| (should_auto_stop(slot), slot.is_recording()))
    else {
        return;
    };
    if auto {
        eprintln!("[vzt-flow] meeting ended (detector); stopping transcription");
        stop(app);
    } else if recording {
        eprintln!(
            "[vzt-flow] meeting ended (detector) but the session was started manually; \
             leaving it running"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flow_core::meeting::notes::{NOTES_END, NOTES_START};

    fn meetings_with(id: &str) -> Meetings {
        let started = SessionStarted {
            session_id: id.to_string(),
            title: "standup".to_string(),
            started_at_ms: 1_700_000_000_000,
            transcript: PathBuf::from(format!("/tmp/{id}.md")),
            notes: PathBuf::from(format!("/tmp/{id}.notes.txt")),
        };
        let mut meetings = Meetings::new();
        meetings.insert(SessionSlot::new(&started, Arc::new(AtomicBool::new(false)), false));
        meetings
    }

    /// The notepad stays bound to one session id for its whole life, so a
    /// window left open across meetings (or one whose session has been
    /// retired) must be refused rather than silently writing its notes into
    /// somebody else's transcript.
    #[test]
    fn save_refuses_a_foreign_session_id() {
        let mut meetings = meetings_with("2026-09-08-standup-141203");

        let err = slot_for(&mut meetings, "2026-09-08-standup-090000")
            .err()
            .expect("a session we do not hold must be refused");
        assert_eq!(err, FOREIGN_SESSION);

        // The session we do hold still resolves — including after it finished,
        // which is what keeps late note edits working.
        assert!(slot_for(&mut meetings, "2026-09-08-standup-141203").is_ok());
        meetings.current_mut().unwrap().state = SessionState::Completed;
        let slot = slot_for(&mut meetings, "2026-09-08-standup-141203").unwrap();
        assert_eq!(slot.bump_notes_rev(), 1, "a finished session still accepts notes");
    }

    /// `start` refuses rather than replacing a live handle — dropping one
    /// detaches the session thread, which then records forever with no way to
    /// stop or finalize it.
    #[test]
    fn start_is_refused_while_a_session_is_finalizing() {
        let mut meetings = meetings_with("A");
        // `stop` has taken the handle; finalize is in flight.
        meetings.current_mut().unwrap().state = SessionState::Stopping;
        assert!(matches!(start_gate(&meetings), StartGate::Busy(_)));

        meetings.current_mut().unwrap().state = SessionState::Completed;
        assert!(matches!(start_gate(&meetings), StartGate::Clear));

        assert!(matches!(start_gate(&Meetings::new()), StartGate::Clear));
    }

    /// The notification may only claim what was actually written.
    #[test]
    fn completion_body_names_both_files_or_says_why_it_cannot() {
        let md = PathBuf::from("/m/2026-09-08-standup.md");
        let pdf = PathBuf::from("/Users/x/Desktop/Meeting - standup - 2026-09-08 1412.pdf");
        assert_eq!(
            completion_body(&md, Some(&pdf), None),
            "2026-09-08-standup.md saved · Meeting - standup - 2026-09-08 1412.pdf on your Desktop"
        );
        assert_eq!(
            completion_body(&md, None, Some("read-only file system")),
            "2026-09-08-standup.md saved. The PDF could not be written: read-only file system"
        );
        assert!(completion_body(&md, None, None).starts_with("2026-09-08-standup.md saved."));
    }

    /// Re-export re-renders from the summary already in the transcript, so it
    /// has to find it after both the transcript lines and the notes section.
    #[test]
    fn summary_is_recovered_from_the_finalized_transcript() {
        let md = format!(
            "# Meeting: standup — 2026-09-08 14:12\n\n\
             [00:00:01] Them: are we shipping today\n\
             [00:00:09] Me: yes\n\n\
             ## My notes\n{NOTES_START}\n\n> _typed_\n\nship it\n{NOTES_END}\n\n\
             - Decision: ship today\n- Owner: Vonzelle\n\n\
             _Summary covers the full transcript (1 sections)._\n"
        );

        let (summary, coverage) = extract_summary_section(&md);
        assert_eq!(summary.as_deref(), Some("- Decision: ship today\n- Owner: Vonzelle"));
        assert_eq!(coverage.as_deref(), Some("Summary covers the full transcript (1 sections)."));

        let (lines, duration) = transcript_lines_and_duration(&md);
        assert_eq!(lines.len(), 2, "only timestamped lines go in the PDF body");
        assert_eq!(duration, "00:00:09");
        assert_eq!(header_date(&md).as_deref(), Some("2026-09-08 14:12"));

        // A transcript that never got a summary must not report one.
        let bare = "# Meeting: standup — 2026-09-08 14:12\n\n[00:00:01] Them: hi\n";
        assert_eq!(extract_summary_section(bare), (None, None));
    }
}
