use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use flow_core::audio::AudioCommand;
use flow_core::cleanup_manager::CleanupCommand;
use flow_core::config::Config;
use flow_core::dictionary::DictionaryTerm;
use flow_core::meeting::{
    InterviewTip, MeetingHandle, MeetingOutcome, SessionStarted, SessionState, TranscriptLine,
};
use flow_core::model_manager::ModelCommand;
use flow_core::profiles::Profiles;
use flow_core::snippets::Snippets;
use serde::Serialize;

use crate::coordinator::CoordinatorMsg;

/// Which downloadable model a `start_model_download` request refers to. The
/// wire form (`"parakeet"` / `"cleanup"`) is what the Settings webview sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadKind {
    /// The Parakeet v3 speech-to-text model — required for any transcription.
    Parakeet,
    /// The Qwen3 cleanup LLM — optional (raw mode works without it).
    Cleanup,
}

impl DownloadKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "parakeet" | "parakeet-v3" => Some(DownloadKind::Parakeet),
            "cleanup" => Some(DownloadKind::Cleanup),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            DownloadKind::Parakeet => "parakeet",
            DownloadKind::Cleanup => "cleanup",
        }
    }
}

/// Coarse phase of the active model download, polled by the Settings webview.
/// Serialized to the lowercase strings the JS switches on. `Verifying` covers
/// both the sha check and (for Parakeet) the tar.gz extraction that follow the
/// byte transfer: the underlying `download_*_with_progress` fns are a single
/// blocking call whose progress callback only reports the download itself, so
/// those tail steps can't be told apart from outside — they're reported
/// together as "verifying". `Extracting` is reserved for a future finer split.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DownloadPhase {
    Idle,
    Downloading,
    Verifying,
    /// Reserved: the current worker collapses the post-download sha-verify and
    /// tar.gz extraction into `Verifying` (they can't be told apart from
    /// outside the single blocking download call), so this is never emitted
    /// yet. Kept in the enum so the wire contract and the JS `phaseText`
    /// switch already handle it if a finer split lands. Mirrors the
    /// `#[allow(dead_code)]` on `OverlayEvent::Hidden`.
    #[allow(dead_code)]
    Extracting,
    Done,
    Error,
}

/// Shared, pollable state for the in-app model downloader. A single "slot":
/// at most one download runs at a time (`active_kind`), so the byte/phase
/// fields describe whichever one is in flight. Lives behind an `Arc` in
/// [`AppState`] so the worker thread can own a handle for the (up to 1.1GB,
/// minutes-long) download without holding a Tauri `State` guard.
pub struct ModelDownload {
    /// `Some(kind)` while a download worker is running; `None` when idle. Also
    /// the concurrency guard — `start_model_download` refuses to start while
    /// this is `Some`.
    pub active_kind: Mutex<Option<DownloadKind>>,
    pub phase: Mutex<DownloadPhase>,
    /// Bytes downloaded / total advertised by the server (`total == 0` = the
    /// server didn't send a content-length yet), updated from the worker's
    /// `ProgressFn`.
    pub downloaded: AtomicU64,
    pub total: AtomicU64,
    /// Last error message, set when `phase` becomes `Error`.
    pub error: Mutex<Option<String>>,
    /// Cached "Parakeet model is present". Seeded at startup (cheap dir stat),
    /// flipped `true` by the worker on a successful Parakeet download, and
    /// lazily refreshed by the hotkey gate. The gate reads this on the hot
    /// path so a present model costs one atomic load, not a filesystem walk.
    pub parakeet_present: AtomicBool,
    /// Cached "cleanup model is present + verified". Seeded off-thread at
    /// startup (a first-run verify may hash 1.1GB — kept off the launch path),
    /// flipped `true` by the worker on a successful cleanup download.
    pub cleanup_present: AtomicBool,
}

impl ModelDownload {
    fn new() -> Self {
        Self {
            active_kind: Mutex::new(None),
            phase: Mutex::new(DownloadPhase::Idle),
            downloaded: AtomicU64::new(0),
            total: AtomicU64::new(0),
            error: Mutex::new(None),
            parakeet_present: AtomicBool::new(false),
            cleanup_present: AtomicBool::new(false),
        }
    }

    /// Test-only constructor so `run_model_download` can be driven without a
    /// full `AppState`/`AppHandle`.
    #[cfg(test)]
    pub fn new_for_test() -> Arc<Self> {
        Arc::new(Self::new())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DictationState {
    Idle,
    Recording,
    Transcribing,
    /// Briefly shown after a successful paste before the overlay fades.
    Done,
}

impl DictationState {
    pub fn label(&self) -> &'static str {
        match self {
            DictationState::Idle => "Idle",
            DictationState::Recording => "Recording",
            DictationState::Transcribing => "Transcribing",
            DictationState::Done => "Done",
        }
    }

    /// Lowercase label for the daemon socket's `status` command, per the
    /// protocol's `idle|recording|transcribing` enum — `Done` (the brief
    /// post-paste flash before the coordinator returns to `Idle`) reports
    /// as `"idle"` since it isn't one of the three wire states.
    pub fn daemon_label(&self) -> &'static str {
        match self {
            DictationState::Idle | DictationState::Done => "idle",
            DictationState::Recording => "recording",
            DictationState::Transcribing => "transcribing",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelLifecycle {
    Unloaded,
    Loading,
    Loaded,
}

// ---------------------------------------------------------------------------
// Meeting sessions.
// ---------------------------------------------------------------------------

/// How many transcript lines a slot keeps for the notepad's initial snapshot.
/// A 90-minute meeting produces a few thousand lines; the notepad only ever
/// renders a scrollback, so the ring is bounded rather than unbounded growth
/// in a long call.
pub const LINES_CAP: usize = 500;

/// How many sessions [`Meetings`] retains. Old sessions stay addressable so a
/// notepad bound to a finished meeting can still save notes and re-export.
pub const RETAIN_SESSIONS: usize = 4;

/// One meeting session, alive or finished.
///
/// The desktop — not flow-core — owns session identity: `session_id` is the
/// transcript file's stem (assigned by `meeting::start_with`), and everything
/// the notepad, tray and finalize path need is keyed by it. A slot outlives
/// its `handle`: `stop` takes the handle to hand it to the finalize thread,
/// which comes back later and writes the outcome into the slot by id.
// Every field is read by `meeting_ctl` and, from U11/U12, by the notepad
// commands and the tray; those readers are not wired up yet, and dead-code
// analysis walks from reachable roots, so the whole struct reads as unused
// until they are.
#[allow(dead_code)]
pub struct SessionSlot {
    pub session_id: String,
    pub title: String,
    pub started_at_ms: i64,
    pub state: SessionState,
    /// `Some` only while the session thread is ours to stop. Taken by
    /// `meeting_ctl::stop`, which moves it onto the finalize thread — so a
    /// `None` handle on a non-terminal state means "finalize is in flight",
    /// never "there is nothing running".
    pub handle: Option<MeetingHandle>,
    pub transcript: PathBuf,
    pub notes: PathBuf,
    /// The *live* interview flag — the same `Arc` the running session reads,
    /// so flipping it here takes effect mid-meeting.
    pub interview: Arc<AtomicBool>,
    /// True when the auto-detector started this session. Only a
    /// detection-owned session is stopped by the detector's `Ended` event; a
    /// manually started one keeps running (see [`should_auto_stop`]).
    pub detection_owned: bool,
    /// The notepad is opened at most once per session, so closing it doesn't
    /// make the next transcript line pop it back up.
    pub notepad_opened: bool,
    /// Revision counter for the `.notes.txt` sidecar. flow-core has no
    /// counter of its own (`MeetingOutcome::notes_rev_used` always arrives as
    /// `0`), so this is the authority and the finalize path overwrites the
    /// outcome's field from it.
    pub notes_rev: u64,
    /// The revision that was last *merged into the transcript* — set on
    /// finalize and on re-export. `notes_rev > notes_rev_used` means the
    /// sidecar has edits the exported files don't have yet.
    pub notes_rev_used: Option<u64>,
    /// Bounded scrollback for the notepad's initial snapshot (cap
    /// [`LINES_CAP`]); the live view is driven by `meeting://line` events.
    pub lines: VecDeque<TranscriptLine>,
    pub last_tip: Option<InterviewTip>,
    /// Sequence number of the newest line seen, so a snapshot can tell the
    /// frontend which events it already contains.
    pub last_seq: u64,
    pub outcome: Option<MeetingOutcome>,
    pub last_error: Option<String>,
    /// The summary markdown as written into the transcript, kept so a
    /// re-export can re-render the PDF without a second LLM pass.
    pub summary_markdown: Option<String>,
}

impl SessionSlot {
    /// Builds a slot from the `SessionStarted` that `meeting::start_with`
    /// fires synchronously on the caller's thread. The handle is attached
    /// afterwards (it doesn't exist until `start_with` returns).
    pub fn new(started: &SessionStarted, interview: Arc<AtomicBool>, detection_owned: bool) -> Self {
        Self {
            session_id: started.session_id.clone(),
            title: started.title.clone(),
            started_at_ms: started.started_at_ms,
            state: SessionState::Recording,
            handle: None,
            transcript: started.transcript.clone(),
            notes: started.notes.clone(),
            interview,
            detection_owned,
            notepad_opened: false,
            notes_rev: 0,
            notes_rev_used: None,
            lines: VecDeque::new(),
            last_tip: None,
            last_seq: 0,
            outcome: None,
            last_error: None,
            summary_markdown: None,
        }
    }

    /// Appends one line to the bounded ring and advances `last_seq`.
    ///
    /// Called from `on_line`, which runs on the session thread *while the
    /// transcript writer's mutex is held* — so it must stay O(1) and must
    /// never block on anything but this lock.
    pub fn push_line(&mut self, line: TranscriptLine) {
        self.last_seq = self.last_seq.max(line.seq);
        self.lines.push_back(line);
        while self.lines.len() > LINES_CAP {
            self.lines.pop_front();
        }
    }

    /// Records one saved notes revision and returns the new value.
    #[allow(dead_code)] // called by `meeting_ctl::save_notes` (a U11 seam).
    pub fn bump_notes_rev(&mut self) -> u64 {
        self.notes_rev += 1;
        self.notes_rev
    }

    /// True while the session is capturing audio.
    pub fn is_recording(&self) -> bool {
        matches!(self.state, SessionState::Recording)
    }

    /// True once the session has reached a terminal state. A `Stopping` or
    /// `Finalizing` slot is **not** finished — its finalize thread is still
    /// going to write into it.
    pub fn is_finished(&self) -> bool {
        matches!(self.state, SessionState::Completed | SessionState::Failed(_))
    }
}

/// Whether the detector's `Ended` event should stop this session.
///
/// Only a session the detector itself started is the detector's to stop: a
/// meeting the user started from the tray keeps running when Zoom quits, so a
/// mis-detection can't silently end a recording somebody is relying on.
pub fn should_auto_stop(slot: &SessionSlot) -> bool {
    slot.detection_owned && slot.is_recording()
}

/// The desktop's meeting sessions: the current one plus a short history.
///
/// Pure data — no Tauri, no threads — so the lifecycle rules are unit
/// testable. Held behind `AppState::meetings` and always taken with
/// `lock_or_recover` (gotcha (i)).
#[derive(Default)]
pub struct Meetings {
    pub slots: Vec<SessionSlot>,
    /// `session_id` of the newest session, alive or not.
    pub current: Option<String>,
}

impl Meetings {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, session_id: &str) -> Option<&SessionSlot> {
        self.slots.iter().find(|s| s.session_id == session_id)
    }

    pub fn get_mut(&mut self, session_id: &str) -> Option<&mut SessionSlot> {
        self.slots.iter_mut().find(|s| s.session_id == session_id)
    }

    pub fn current(&self) -> Option<&SessionSlot> {
        let id = self.current.as_deref()?;
        self.get(id)
    }

    pub fn current_mut(&mut self) -> Option<&mut SessionSlot> {
        let id = self.current.clone()?;
        self.get_mut(&id)
    }

    /// Adds a session and makes it current, retiring old ones.
    ///
    /// Deliberately does not touch any other slot: a session that is still
    /// finalizing keeps its state, its notes revision and its pending outcome
    /// while the next meeting records.
    pub fn insert(&mut self, slot: SessionSlot) {
        self.current = Some(slot.session_id.clone());
        self.slots.push(slot);
        self.retire();
    }

    /// Drops the oldest sessions beyond [`RETAIN_SESSIONS`].
    ///
    /// Only *finished*, non-current slots are evictable. Dropping a slot that
    /// still holds a `MeetingHandle` would drop its `JoinHandle` — detaching a
    /// live session thread that then never stops or finalizes — and dropping
    /// one that is mid-finalize would strand its outcome, since the finalize
    /// thread writes back by id. So a pathological run of unfinished sessions
    /// grows past the cap rather than losing one.
    pub fn retire(&mut self) {
        while self.slots.len() > RETAIN_SESSIONS {
            let current = self.current.clone();
            let evictable = self.slots.iter().position(|s| {
                s.is_finished() && s.handle.is_none() && Some(&s.session_id) != current.as_ref()
            });
            match evictable {
                Some(i) => {
                    self.slots.remove(i);
                }
                None => break,
            }
        }
    }
}

/// Shared state accessed from the tray, the coordinator thread, and Tauri
/// commands invoked by the webview.
pub struct AppState {
    pub dictation_state: Mutex<DictationState>,
    pub model_lifecycle: Mutex<ModelLifecycle>,
    /// Mirrors `model_lifecycle` for the cleanup LLM, populated from
    /// `CleanupStatusEvent`s the coordinator receives — used by the daemon
    /// socket's `status` command (`cleanup_loaded`).
    pub cleanup_lifecycle: Mutex<ModelLifecycle>,
    /// Set by `CoordinatorMsg::DaemonListen` while a daemon-triggered
    /// recording is in flight: the reply channel to send the final
    /// `ListenOutcome` on (instead of pasting) plus an optional mode
    /// override for that one recording. Taken (cleared) as soon as the
    /// recording stops and pipeline processing begins.
    pub pending_listen: Mutex<Option<(Sender<Result<crate::coordinator::ListenOutcome, String>>, Option<String>)>>,
    /// The most recent *final* (dictionary-corrected, code-mode/cleaned)
    /// text — what "Copy last transcript" copies and what Settings shows.
    pub last_transcript: Mutex<Option<String>>,
    pub config: Mutex<Config>,
    pub dictionary: Mutex<Vec<DictionaryTerm>>,
    pub profiles: Mutex<Profiles>,
    pub snippets: Mutex<Snippets>,
    pub cleanup_cmd_tx: Mutex<Option<Sender<CleanupCommand>>>,
    /// True while a hands-free (tap-to-toggle) recording is active, as
    /// opposed to a hold-to-talk recording.
    pub hands_free_active: AtomicBool,
    /// Flips while a dictation is being recorded so the hotkey monitor
    /// knows Escape should currently act as "cancel".
    pub is_recording: Arc<AtomicBool>,
    /// Live-updatable hold-key virtual keycode the hotkey tap reads. `None`
    /// until the CGEventTap installs successfully.
    pub hotkey_keycode_handle: Mutex<Option<Arc<AtomicU16>>>,
    pub audio_cmd_tx: Mutex<Option<Sender<AudioCommand>>>,
    pub model_cmd_tx: Mutex<Option<Sender<ModelCommand>>>,
    pub coordinator_tx: Mutex<Option<Sender<CoordinatorMsg>>>,
    /// True once the CGEventTap installed successfully (false usually means
    /// Input Monitoring permission hasn't been granted).
    pub hotkey_monitor_active: AtomicBool,
    /// When the current recording started, set by `start_recording` and
    /// read on each `AudioReply::Level` tick to drive the overlay's
    /// elapsed-time display. `None` while idle.
    pub recording_started: Mutex<Option<std::time::Instant>>,
    /// The duration cap (`max_hold_secs`/`max_handsfree_secs`, whichever
    /// applies) for the current recording, set alongside `recording_started`
    /// — used to compute the overlay's last-30s warning state.
    pub recording_max_secs: Mutex<Option<u64>>,
    /// The meeting-transcription sessions (owned by `meeting_ctl`): the
    /// current one plus a short history, keyed by `session_id`. Independent
    /// of the dictation state machine above — a meeting and hold-to-talk
    /// dictation can be active at the same time.
    ///
    /// Locked on the session thread from `on_line` (which runs while the
    /// transcript writer's mutex is held), so nothing may hold this lock
    /// across a blocking call — never across `stop_detailed`, a file write or
    /// a notification.
    pub meetings: Mutex<Meetings>,
    /// In-app model downloader state (see [`ModelDownload`]). `Arc` so a
    /// download worker thread can own a handle across the whole transfer.
    pub model_download: Arc<ModelDownload>,
}

/// Poison-tolerant `Mutex::lock`.
///
/// Every `AppState` mutex is guarded state for a *single-threaded* state
/// machine, never a data structure whose invariants a panic could leave
/// half-written. So a poisoned lock here carries no information worth
/// propagating — but `.lock_or_recover()` on it panics, which is how one bad
/// dictation used to become a permanently dead hotkey: the first panic
/// poisons the mutex, and every subsequent press panics on the same line
/// before it can do anything. The coordinator supervisor
/// (`coordinator::spawn`) restarts the loop after a panic, and this is what
/// makes that restart able to make progress instead of re-panicking forever.
///
/// Deliberately NOT `unwrap_or_else(|_| panic!(..))` and not a `Result` — the
/// caller has nothing useful to do with the poison flag.
pub trait LockRecover<T> {
    /// Lock, taking the guard even if a previous holder panicked.
    fn lock_or_recover(&self) -> std::sync::MutexGuard<'_, T>;
}

impl<T> LockRecover<T> for Mutex<T> {
    fn lock_or_recover(&self) -> std::sync::MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl AppState {
    /// Return the dictation state machine to a usable Idle state after the
    /// coordinator thread panicked mid-dictation.
    ///
    /// Without this the restart is useless: `run_coordinator` refuses a new
    /// press unless `dictation_state == Idle` (and hands-free/`is_recording`
    /// gate other paths), so a panic during Recording/Transcribing would leave
    /// every future hotkey press silently ignored — the same dead-hotkey
    /// symptom the supervisor exists to prevent, just latched in a different
    /// variable. Any guard we add has to have a path back to its clear state.
    ///
    /// A daemon `listen` waiting on a reply is answered with an error rather
    /// than dropped, so the CLI caller fails fast instead of blocking forever.
    ///
    /// `meetings` is deliberately **not** touched: a meeting runs on its own
    /// threads and is not part of the dictation state machine, so a
    /// coordinator panic must not end a recording the user is relying on —
    /// and resetting the slot would strand the live `MeetingHandle`.
    pub fn reset_after_panic(&self) {
        *self.dictation_state.lock_or_recover() = DictationState::Idle;
        *self.recording_started.lock_or_recover() = None;
        *self.recording_max_secs.lock_or_recover() = None;
        self.is_recording.store(false, Ordering::Relaxed);
        self.hands_free_active.store(false, Ordering::Relaxed);
        if let Some((reply, _mode)) = self.pending_listen.lock_or_recover().take() {
            let _ = reply.send(Err(
                "dictation failed unexpectedly; the recorder has been reset".to_string(),
            ));
        }
    }
}

impl AppState {
    pub fn new(config: Config, is_recording: Arc<AtomicBool>) -> Self {
        let dictionary = flow_core::dictionary::load_or_seed().unwrap_or_else(|e| {
            eprintln!("[vzt-flow] failed to load dictionary, using seed defaults: {e}");
            flow_core::dictionary::seed_dictionary()
        });
        let profiles = flow_core::profiles::load_or_seed().unwrap_or_else(|e| {
            eprintln!("[vzt-flow] failed to load profiles, using seed defaults: {e}");
            flow_core::profiles::seed_profiles()
        });
        let snippets = flow_core::snippets::load_or_seed().unwrap_or_else(|e| {
            eprintln!("[vzt-flow] failed to load snippets, using seed defaults: {e}");
            flow_core::snippets::seed_snippets()
        });

        // Seed model-presence caches. Parakeet is a cheap directory stat, done
        // inline so the hotkey gate is accurate from the first press. The
        // cleanup verify can hash a 1.1GB file on a first-run/migration config
        // (see `models::check_cleanup_model`), so it's pushed off the launch
        // path onto a background thread — `cleanup_present` reads `false` until
        // it completes (a few seconds at most), which only understates an
        // optional model in the Settings dot briefly.
        let model_download = Arc::new(ModelDownload::new());
        let parakeet_present = flow_core::models::check_parakeet_model()
            .map(|s| s.present)
            .unwrap_or(false);
        model_download.parakeet_present.store(parakeet_present, Ordering::Relaxed);
        {
            let md = model_download.clone();
            std::thread::spawn(move || {
                let present = flow_core::models::check_cleanup_model().unwrap_or(false);
                md.cleanup_present.store(present, Ordering::Relaxed);
            });
        }

        Self {
            dictation_state: Mutex::new(DictationState::Idle),
            model_lifecycle: Mutex::new(ModelLifecycle::Unloaded),
            cleanup_lifecycle: Mutex::new(ModelLifecycle::Unloaded),
            pending_listen: Mutex::new(None),
            last_transcript: Mutex::new(None),
            config: Mutex::new(config),
            dictionary: Mutex::new(dictionary),
            profiles: Mutex::new(profiles),
            snippets: Mutex::new(snippets),
            cleanup_cmd_tx: Mutex::new(None),
            hands_free_active: AtomicBool::new(false),
            is_recording,
            hotkey_keycode_handle: Mutex::new(None),
            audio_cmd_tx: Mutex::new(None),
            model_cmd_tx: Mutex::new(None),
            coordinator_tx: Mutex::new(None),
            hotkey_monitor_active: AtomicBool::new(false),
            recording_started: Mutex::new(None),
            recording_max_secs: Mutex::new(None),
            meetings: Mutex::new(Meetings::new()),
            model_download,
        }
    }

    pub fn set_dictation_state(&self, s: DictationState) {
        *self.dictation_state.lock_or_recover() = s;
        self.is_recording
            .store(s == DictationState::Recording, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod lock_recover_tests {
    use super::*;

    /// The anti-latching property the coordinator supervisor depends on.
    ///
    /// Before `LockRecover`, one panic while an `AppState` mutex was held
    /// poisoned it permanently, so every later `.lock().unwrap()` panicked on
    /// the same line — restarting the coordinator would just panic again, and
    /// the hotkey stayed dead until the user quit and relaunched the app.
    #[test]
    fn a_poisoned_mutex_is_still_usable() {
        let m = Arc::new(Mutex::new(41));

        // Poison it exactly the way a panicking dictation would.
        let m2 = m.clone();
        let _ = std::thread::spawn(move || {
            let _guard = m2.lock().unwrap();
            panic!("simulated panic while holding the lock");
        })
        .join();
        assert!(m.is_poisoned(), "precondition: the mutex must be poisoned");

        // The plain path is what used to strand us.
        assert!(m.lock().is_err(), "control: .lock() reports the poison");

        // The recovering path still hands over the guard, and the value is
        // intact — so the restarted coordinator can make progress.
        *m.lock_or_recover() += 1;
        assert_eq!(*m.lock_or_recover(), 42);
    }
}

#[cfg(test)]
mod meetings_tests {
    use super::*;
    use flow_core::meeting::transcriber::Source;

    fn started(id: &str) -> SessionStarted {
        SessionStarted {
            session_id: id.to_string(),
            title: format!("{id} meeting"),
            started_at_ms: 1_700_000_000_000,
            transcript: PathBuf::from(format!("/tmp/{id}.md")),
            notes: PathBuf::from(format!("/tmp/{id}.notes.txt")),
        }
    }

    fn slot(id: &str) -> SessionSlot {
        SessionSlot::new(&started(id), Arc::new(AtomicBool::new(false)), false)
    }

    fn line(id: &str, seq: u64) -> TranscriptLine {
        TranscriptLine {
            session_id: id.to_string(),
            seq,
            source: Source::Them,
            offset_secs: seq as f32,
            text: format!("line {seq}"),
        }
    }

    /// The history is a fixed-size window: a fifth session evicts the first,
    /// and the evicted one is no longer addressable by id.
    #[test]
    fn retire_keeps_the_last_four_sessions_and_drops_the_oldest() {
        let mut meetings = Meetings::new();
        for id in ["A", "B", "C", "D", "E"] {
            // Only one meeting runs at a time, so the previous one is always
            // finished by the time the next starts.
            if let Some(prev) = meetings.current_mut() {
                prev.state = SessionState::Completed;
            }
            meetings.insert(slot(id));
        }

        let ids: Vec<&str> = meetings.slots.iter().map(|s| s.session_id.as_str()).collect();
        assert_eq!(ids, vec!["B", "C", "D", "E"]);
        assert!(meetings.get("A").is_none(), "the oldest session is dropped");
        assert_eq!(meetings.current.as_deref(), Some("E"));
    }

    /// A slot that is still finalizing is never evicted, even past the cap —
    /// its finalize thread is going to write the outcome back into it by id,
    /// and dropping it would strand that write (and, if it still held the
    /// handle, detach the session thread).
    #[test]
    fn retire_never_evicts_an_unfinished_session() {
        let mut meetings = Meetings::new();
        for id in ["A", "B", "C", "D", "E", "F"] {
            let mut s = slot(id);
            s.state = SessionState::Finalizing { step: "summarizing 1/1".to_string() };
            meetings.insert(s);
        }
        assert_eq!(meetings.slots.len(), 6, "unfinished slots grow past the cap");
        assert!(meetings.get("A").is_some());
    }

    /// Starting the next meeting must not disturb one that is still writing
    /// its summary — the new session takes `current`, nothing else changes.
    #[test]
    fn a_new_session_does_not_disturb_a_finalizing_one() {
        let mut meetings = Meetings::new();
        let mut a = slot("A");
        a.state = SessionState::Finalizing { step: "writing pdf".to_string() };
        a.notes_rev = 7;
        meetings.insert(a);

        meetings.insert(slot("B"));

        let a = meetings.get("A").expect("the finalizing session is still addressable");
        assert!(
            matches!(a.state, SessionState::Finalizing { .. }),
            "A must still be Finalizing, was {:?}",
            a.state
        );
        assert_eq!(a.notes_rev, 7, "A keeps its notes revision");
        assert_eq!(meetings.current.as_deref(), Some("B"));
        assert!(meetings.current().map(|s| s.is_recording()).unwrap_or(false));
    }

    /// Requirement 6: the detector may only stop what the detector started.
    #[test]
    fn detector_end_only_stops_a_detection_owned_session() {
        let mut auto = slot("auto");
        auto.detection_owned = true;
        assert!(should_auto_stop(&auto), "a detected session is the detector's to stop");

        let manual = slot("manual");
        assert!(!should_auto_stop(&manual), "a manually started session keeps running");

        // Neither is stoppable once it has left Recording.
        auto.state = SessionState::Stopping;
        assert!(!should_auto_stop(&auto), "a stopping session is not stopped twice");
    }

    /// A long meeting must not grow the slot without bound; the newest lines
    /// are the ones kept.
    #[test]
    fn lines_ring_is_bounded_at_500() {
        let mut s = slot("A");
        for seq in 1..=(LINES_CAP as u64 + 250) {
            s.push_line(line("A", seq));
        }

        assert_eq!(s.lines.len(), LINES_CAP);
        assert_eq!(s.lines.front().map(|l| l.seq), Some(251), "oldest lines are dropped");
        assert_eq!(s.lines.back().map(|l| l.seq), Some(750));
        assert_eq!(s.last_seq, 750, "last_seq tracks the newest line, not the ring");
    }

    /// The desktop owns the notes revision counter (flow-core always reports
    /// `notes_rev_used: 0`), and an export snapshots whichever revision it
    /// merged — later edits bump `notes_rev` without touching it.
    #[test]
    fn notes_rev_increments_monotonically_and_export_snapshots_it() {
        let mut s = slot("A");
        assert_eq!(s.notes_rev, 0);
        assert_eq!(s.notes_rev_used, None);

        assert_eq!(s.bump_notes_rev(), 1);
        assert_eq!(s.bump_notes_rev(), 2);
        assert_eq!(s.bump_notes_rev(), 3);

        // Finalize/export snapshots the revision it actually merged.
        s.notes_rev_used = Some(s.notes_rev);
        assert_eq!(s.notes_rev_used, Some(3));

        // A late edit after Completed still saves, and is visibly unexported.
        assert_eq!(s.bump_notes_rev(), 4);
        assert_eq!(s.notes_rev_used, Some(3), "the export snapshot does not move on its own");
        assert!(s.notes_rev > s.notes_rev_used.unwrap_or(0));
    }
}
