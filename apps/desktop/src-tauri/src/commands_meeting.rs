//! `#[tauri::command]` handlers invoked from the meeting notepad webview
//! (`apps/desktop/dist/notes.js`). Every state-changing command is a thin
//! wrapper over a `meeting_ctl` seam — the lifecycle, locking and thread
//! discipline live there; this module only shapes the wire payloads and maps
//! errors to strings.

use std::sync::atomic::Ordering;

use flow_core::meeting::{notes, InterviewTip, TranscriptLine};
use serde::Serialize;
use tauri::AppHandle;

use crate::meeting_ctl;

/// The notepad's whole-state snapshot, fetched once on load/resync and
/// reconciled against the live event stream by `seq` (see [`should_apply`]).
///
/// `session_id: None` means no meeting has run yet this process; every other
/// field is then at its default and the notepad shows "Waiting for a
/// meeting".
#[derive(Serialize, Default)]
pub struct MeetingSnapshot {
    pub session_id: Option<String>,
    pub state: String,
    pub title: String,
    pub started_at_ms: i64,
    pub seq: u64,
    pub lines: Vec<TranscriptLine>,
    pub notes: String,
    pub notes_rev: u64,
    pub notes_rev_used: Option<u64>,
    pub last_tip: Option<InterviewTip>,
    pub interview: bool,
    pub transcript_path: Option<String>,
    pub pdf_path: Option<String>,
    pub error: Option<String>,
}

/// Everything the snapshot needs from the slot except the notes text, which
/// is file I/O and must happen outside the `meetings` lock (see
/// `meeting_ctl::with_slot`'s doc comment).
struct SlotSnapshot {
    session_id: String,
    state: String,
    title: String,
    started_at_ms: i64,
    seq: u64,
    lines: Vec<TranscriptLine>,
    notes_path: std::path::PathBuf,
    notes_rev: u64,
    notes_rev_used: Option<u64>,
    last_tip: Option<InterviewTip>,
    interview: bool,
    transcript_path: Option<String>,
    pdf_path: Option<String>,
    error: Option<String>,
}

/// The `state` string the notepad's `kind()` helper expects — mirrors
/// `SessionState`'s `#[serde(tag = "kind", rename_all = "snake_case")]`, but
/// flattened to a bare string (the `Finalizing { step }` and `Failed(msg)`
/// payloads aren't needed client-side: `error` already carries the failure
/// text and the step is cosmetic).
fn state_str(state: &flow_core::meeting::SessionState) -> String {
    use flow_core::meeting::SessionState;
    match state {
        SessionState::Recording => "recording",
        SessionState::Stopping => "stopping",
        SessionState::Finalizing { .. } => "finalizing",
        SessionState::Completed => "completed",
        SessionState::Failed(_) => "failed",
    }
    .to_string()
}

/// Builds a snapshot of the current (or most recent) session. Notes are read
/// from the sidecar *after* the `meetings` lock is released.
#[tauri::command]
pub fn get_meeting_snapshot(app: AppHandle) -> MeetingSnapshot {
    let partial = meeting_ctl::with_slot(&app, None, |slot| SlotSnapshot {
        session_id: slot.session_id.clone(),
        state: state_str(&slot.state),
        title: slot.title.clone(),
        started_at_ms: slot.started_at_ms,
        seq: slot.last_seq,
        lines: slot.lines.iter().cloned().collect(),
        notes_path: slot.notes.clone(),
        notes_rev: slot.notes_rev,
        notes_rev_used: slot.notes_rev_used,
        last_tip: slot.last_tip.clone(),
        interview: slot.interview.load(Ordering::Relaxed),
        transcript_path: slot.outcome.as_ref().map(|o| o.transcript.display().to_string()),
        pdf_path: slot
            .outcome
            .as_ref()
            .and_then(|o| o.pdf.as_ref())
            .map(|p| p.display().to_string()),
        error: slot.last_error.clone(),
    });

    let Some(s) = partial else {
        return MeetingSnapshot::default();
    };
    MeetingSnapshot {
        session_id: Some(s.session_id),
        state: s.state,
        title: s.title,
        started_at_ms: s.started_at_ms,
        seq: s.seq,
        lines: s.lines,
        notes: notes::read(&s.notes_path),
        notes_rev: s.notes_rev,
        notes_rev_used: s.notes_rev_used,
        last_tip: s.last_tip,
        interview: s.interview,
        transcript_path: s.transcript_path,
        pdf_path: s.pdf_path,
        error: s.error,
    }
}

#[tauri::command]
pub fn save_meeting_notes(app: AppHandle, session_id: String, text: String) -> Result<u64, String> {
    meeting_ctl::save_notes(&app, &session_id, &text)
}

#[tauri::command]
pub fn set_interview_mode(app: AppHandle, session_id: String, on: bool) -> Result<(), String> {
    meeting_ctl::set_interview(&app, &session_id, on)
}

/// The manual "Open meeting notes" fallback: opens (creating if needed) and
/// shows the notepad, binding it to the current session when one exists —
/// used whether or not a session is currently live (e.g. Screen Recording
/// permission missing so auto-detect can't run, or title matching failed).
#[tauri::command]
pub fn open_meeting_notes(app: AppHandle) {
    match meeting_ctl::current_session_id(&app) {
        Some(session_id) => crate::notepad::open(&app, &session_id),
        None => crate::notepad::show(&app),
    }
}

#[tauri::command]
pub fn reexport_meeting(app: AppHandle, session_id: String) -> Result<String, String> {
    meeting_ctl::reexport(&app, &session_id).map(|p| p.display().to_string())
}

#[tauri::command]
pub fn get_interview_context() -> String {
    flow_core::meeting::interview::load_context()
}

#[tauri::command]
pub fn set_interview_context(text: String) -> Result<(), String> {
    flow_core::meeting::interview::save_context(&text).map_err(|e| e.to_string())
}

/// Reveals a file in Finder, mirroring `meeting_ctl::open_folder`'s
/// `open`-based approach. Best-effort: a failure is logged, never fatal —
/// the notepad's caller already has the path and can show it in an error
/// line if this silently no-ops.
#[tauri::command]
pub fn reveal_in_finder(path: String) {
    #[cfg(target_os = "macos")]
    {
        if let Err(e) = std::process::Command::new("open").arg("-R").arg(&path).spawn() {
            eprintln!("[vzt-flow] failed to reveal {path} in Finder: {e}");
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        eprintln!("[vzt-flow] reveal_in_finder is not implemented on this platform: {path}");
    }
}

/// Whether the frontend should apply an event carrying `event_seq` against a
/// snapshot that already included lines up to `snapshot_seq`. Pure so the
/// missed-event-window logic (mirrored in `notes.js`'s `apply`) has a tested
/// Rust reference, even though the actual discarding happens client-side.
#[allow(dead_code)] // reference implementation for notes.js's `apply`; exercised by tests.
pub fn should_apply(event_seq: u64, snapshot_seq: u64) -> bool {
    event_seq > snapshot_seq
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `get_meeting_snapshot` returns the seq of the last line it included;
    /// the frontend must drop anything at or below that, and apply anything
    /// strictly newer.
    #[test]
    fn snapshot_seq_lets_the_frontend_drop_replayed_events() {
        assert!(!should_apply(3, 5), "an event no newer than the snapshot is a replay");
        assert!(!should_apply(5, 5), "equal seq is the snapshot's own line, not a new one");
        assert!(should_apply(6, 5), "strictly newer than the snapshot must be applied");
        assert!(should_apply(1, 0), "the very first event after an empty snapshot applies");
    }

    #[test]
    fn state_str_matches_the_serde_tag_the_frontend_switches_on() {
        use flow_core::meeting::SessionState;
        assert_eq!(state_str(&SessionState::Recording), "recording");
        assert_eq!(state_str(&SessionState::Stopping), "stopping");
        assert_eq!(state_str(&SessionState::Finalizing { step: "pdf".into() }), "finalizing");
        assert_eq!(state_str(&SessionState::Completed), "completed");
        assert_eq!(state_str(&SessionState::Failed("boom".into())), "failed");
    }
}
