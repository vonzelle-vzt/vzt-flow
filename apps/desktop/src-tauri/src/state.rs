use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use flow_core::audio::AudioCommand;
use flow_core::cleanup_manager::CleanupCommand;
use flow_core::config::Config;
use flow_core::dictionary::DictionaryTerm;
use flow_core::model_manager::ModelCommand;
use flow_core::profiles::Profiles;
use flow_core::snippets::Snippets;

use crate::coordinator::CoordinatorMsg;

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
    /// The in-progress meeting-transcription session, if any (owned by
    /// `meeting_ctl`). `Some` while a meeting is being transcribed; taken and
    /// joined on stop. Independent of the dictation state machine above — a
    /// meeting and hold-to-talk dictation can be active at the same time.
    pub meeting_session: Mutex<Option<flow_core::meeting::MeetingHandle>>,
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
            meeting_session: Mutex::new(None),
        }
    }

    pub fn set_dictation_state(&self, s: DictationState) {
        *self.dictation_state.lock().unwrap() = s;
        self.is_recording
            .store(s == DictationState::Recording, Ordering::Relaxed);
    }
}
