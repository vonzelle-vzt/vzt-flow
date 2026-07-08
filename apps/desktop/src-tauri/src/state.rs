use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use flow_core::audio::AudioCommand;
use flow_core::config::Config;
use flow_core::model_manager::ModelCommand;

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
    pub last_transcript: Mutex<Option<String>>,
    pub config: Mutex<Config>,
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
}

impl AppState {
    pub fn new(config: Config, is_recording: Arc<AtomicBool>) -> Self {
        Self {
            dictation_state: Mutex::new(DictationState::Idle),
            model_lifecycle: Mutex::new(ModelLifecycle::Unloaded),
            last_transcript: Mutex::new(None),
            config: Mutex::new(config),
            hands_free_active: AtomicBool::new(false),
            is_recording,
            hotkey_keycode_handle: Mutex::new(None),
            audio_cmd_tx: Mutex::new(None),
            model_cmd_tx: Mutex::new(None),
            coordinator_tx: Mutex::new(None),
            hotkey_monitor_active: AtomicBool::new(false),
        }
    }

    pub fn set_dictation_state(&self, s: DictationState) {
        *self.dictation_state.lock().unwrap() = s;
        self.is_recording
            .store(s == DictationState::Recording, Ordering::Relaxed);
    }
}
