//! The dictation state machine. Owns no UI directly — it drives the tray
//! label, the overlay window, and the paste/history pipeline in response
//! to hotkey and audio/model events. All state transitions happen on one
//! thread so there's a single source of truth for "what state are we in".

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::time::Duration;

use flow_core::audio::{AudioCommand, AudioReply};
use flow_core::cleanup::{CleanupContext, Mode};
use flow_core::cleanup_manager::{CleanupCommand, CleanupResult, CleanupStatusEvent};
use flow_core::config::Config;
use flow_core::hotkey::HotkeyEvent;
use flow_core::model_manager::{ModelCommand, ModelStatusEvent};
use flow_core::profiles::ProfileRule;
use flow_core::rolling::{self, RollingInput, RollingOutput};
use flow_core::{codemode, dictionary, history, insert, permissions, snippets};
use tauri::{AppHandle, Manager};
use tauri_plugin_notification::NotificationExt;

use crate::overlay::{self, OverlayEvent};
use crate::state::{AppState, DictationState, ModelLifecycle};
use crate::tray;

pub enum CoordinatorMsg {
    Hotkey(HotkeyEvent),
    Audio(AudioReply),
    Model(ModelStatusEvent),
    Cleanup(CleanupStatusEvent),
    TranscribeResult {
        result: Result<flow_core::Transcript, String>,
        audio_duration: Duration,
        /// Frontmost app + resolved profile, captured when the recording
        /// stopped (not after transcription) so the overlay's mode badge
        /// can show immediately and "frontmost app at paste time" reflects
        /// where the user actually was when they finished talking.
        app_bundle_id: Option<String>,
        profile: ProfileRule,
        /// Set when this recording was triggered by the daemon socket's
        /// `listen` command: the pipeline finishes by replying here instead
        /// of pasting.
        listen_reply: Option<Sender<Result<ListenOutcome, String>>>,
    },
    /// The cleanup pipeline (LLM or timeout/fallback) finished; carries
    /// everything needed to paste + log history.
    CleanupDone {
        raw_text: String,
        result: CleanupResult,
        mode_label: String,
        audio_duration: Duration,
        app_bundle_id: Option<String>,
        listen_reply: Option<Sender<Result<ListenOutcome, String>>>,
    },
    /// A message from the current recording's rolling-transcription worker
    /// (Feature B), tagged with the recording `epoch` so any output from an
    /// abandoned or older recording is ignored.
    Rolling { epoch: u64, output: RollingOutput },
    /// Watchdog: a rolling finalize for `epoch` has taken too long; force the
    /// state machine out of Transcribing (the rolling counterpart of the batch
    /// path's F2 never-hang timeout).
    RollingTimeout { epoch: u64 },
    /// Manual toggle from the tray menu item — behaves like a hotkey tap.
    TrayToggleDictation,
    /// Cycles the overlay through its states for visual verification,
    /// without touching the microphone or transcriber.
    TestOverlay,
    /// Daemon socket `listen` command: record now (hands-free semantics —
    /// RMS auto-stop, duration cap), run the full pipeline, and reply with
    /// the result instead of pasting. `mode` overrides the resolved
    /// profile's mode for this one recording; `max_secs` overrides the
    /// hands-free duration cap.
    DaemonListen {
        mode: Option<String>,
        max_secs: Option<u64>,
        reply: Sender<Result<ListenOutcome, String>>,
    },
}

/// Result of a daemon-triggered `listen` — mirrors what would have been
/// pasted, but handed back over the socket instead.
#[derive(Debug, Clone)]
pub struct ListenOutcome {
    pub raw: String,
    pub text: String,
    pub mode: String,
    pub duration_s: f64,
}

/// Installs the platform hold-to-talk hotkey monitor and forwards its
/// press/release events onto `tx`. Returns whether installation succeeded.
///
/// macOS uses `flow_core::hotkey`'s `CGEventTap` (see that module's docs for
/// why — modifier-only bindings can't go through
/// `tauri-plugin-global-shortcut`). Windows *does* use that plugin, since
/// its default binding is a normal key combo rather than a bare modifier,
/// and registering one only needs the `AppHandle` this function already has
/// — no reason to duplicate an OS-level tap for it.
#[cfg(target_os = "macos")]
fn spawn_hotkey_monitor(
    app: &AppHandle,
    keycode: u16,
    is_recording: Arc<AtomicBool>,
    tx: Sender<HotkeyEvent>,
) -> bool {
    let hotkey_result = flow_core::hotkey::spawn_monitor(keycode, is_recording, tx);
    let active = hotkey_result.is_ok();
    if let Ok(keycode_handle) = &hotkey_result {
        *app.state::<AppState>().hotkey_keycode_handle.lock().unwrap() = Some(keycode_handle.clone());
    } else {
        eprintln!(
            "[vzt-flow] hotkey monitor failed to install a CGEventTap — this almost always means \
             Input Monitoring permission hasn't been granted (System Settings > Privacy & Security \
             > Input Monitoring). The tray's manual Start/Stop item still works."
        );
    }
    active
}

/// Windows and Linux (X11) hold-to-talk binding: Ctrl+Shift+Space via
/// `tauri-plugin-global-shortcut` (registered here at runtime; the plugin
/// itself is added to the Tauri builder in `lib.rs` for both platforms).
///
/// Not Right Option/Alt like macOS's default — the plugin does not support
/// modifier-only shortcuts on Windows, and its X11 backend
/// (`global-hotkey` 0.8) grabs a specific keycode + modifier mask via
/// `XGrabKey`, so a bare modifier can't be a binding there either. A normal
/// key combo is therefore used on both platforms. The plugin *does* deliver
/// clean press/release transitions the hold logic needs:
///   - Windows: `RegisterHotKey` → `WM_HOTKEY` press + a synthetic release.
///   - X11: `global-hotkey` enables xkb `DETECTABLE_AUTO_REPEAT` and latches
///     a `pressed` flag, so a held key yields exactly one `Pressed` on press
///     and one `Released` on physical release (no auto-repeat chatter).
///     Verified against `tauri-apps/global-hotkey` v0.8.0
///     (`src/platform_impl/x11/mod.rs`) before writing this.
///
/// Wayland caveat: `global-hotkey`'s only Linux backend is X11. Under a
/// Wayland session it connects to the X server exposed by XWayland (via
/// `DISPLAY`), so the grab only fires while an X11/XWayland-backed window is
/// focused, not globally across native Wayland apps; if no X server is
/// reachable at all, `on_shortcut` returns `Err` and this returns `false`
/// (tray toggle still works). `global-hotkey` does not implement the
/// `org.freedesktop.portal.GlobalShortcuts` XDG portal as of v0.8.0, so a
/// portal-based Wayland-native global hotkey is out of scope this pass and
/// documented in docs/USAGE-Linux.md.
///
/// Escape-to-cancel is not wired up here: unlike the macOS tap (which is
/// `ListenOnly` and never consumes Escape for other apps), a globally
/// *registered* Escape shortcut would swallow Escape everywhere, which is
/// unacceptable UX. On Linux the Unix daemon socket is available, so
/// `flow cancel` ends a recording early; on Windows the socket is Unix-only
/// (see `flow_core::ipc`), leaving the tray's "Start/Stop dictation" item as
/// the early-stop path. Documented as a known gap in the README.
#[cfg(any(target_os = "windows", target_os = "linux"))]
fn spawn_hotkey_monitor(
    app: &AppHandle,
    _keycode: u16,
    _is_recording: Arc<AtomicBool>,
    tx: Sender<HotkeyEvent>,
) -> bool {
    use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

    let shortcut = Shortcut::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::Space);
    let result = app.global_shortcut().on_shortcut(shortcut, move |_app, _shortcut, event| {
        let hk = match event.state {
            ShortcutState::Pressed => HotkeyEvent::HoldKeyPressed,
            ShortcutState::Released => HotkeyEvent::HoldKeyReleased,
        };
        let _ = tx.send(hk);
    });
    match result {
        Ok(()) => true,
        Err(e) => {
            eprintln!(
                "[vzt-flow] failed to register global hotkey Ctrl+Shift+Space: {e}. \
                 On Wayland this is expected when no X server (XWayland) is reachable. \
                 The tray's manual Start/Stop item still works."
            );
            false
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn spawn_hotkey_monitor(
    _app: &AppHandle,
    _keycode: u16,
    _is_recording: Arc<AtomicBool>,
    _tx: Sender<HotkeyEvent>,
) -> bool {
    false
}

/// Spawns the audio worker, model manager, and hotkey monitor threads, then
/// the coordinator thread itself. Returns the sender used to feed it
/// messages (stored in `AppState` for the tray/commands to reach it) and
/// whether the hotkey monitor installed successfully. `is_recording` is the
/// same flag already stored in `AppState` — shared so the hotkey tap can
/// read it without going through the coordinator channel.
pub fn spawn(
    app: AppHandle,
    config: Config,
    is_recording: Arc<AtomicBool>,
) -> (Sender<CoordinatorMsg>, bool) {
    let (unified_tx, unified_rx) = mpsc::channel::<CoordinatorMsg>();

    // --- audio worker ---
    let (audio_cmd_tx, audio_cmd_rx) = mpsc::channel::<AudioCommand>();
    let (audio_reply_tx, audio_reply_rx) = mpsc::channel::<AudioReply>();
    flow_core::audio::spawn_audio_worker(audio_cmd_rx, audio_reply_tx);
    {
        let tx = unified_tx.clone();
        std::thread::spawn(move || {
            while let Ok(reply) = audio_reply_rx.recv() {
                if tx.send(CoordinatorMsg::Audio(reply)).is_err() {
                    break;
                }
            }
        });
    }

    // --- model manager ---
    let model_dir = flow_core::models::parakeet_model_dir()
        .expect("could not determine model directory (no home dir?)");
    let (model_cmd_tx, model_cmd_rx) = mpsc::channel::<ModelCommand>();
    let (model_status_tx, model_status_rx) = mpsc::channel::<ModelStatusEvent>();
    flow_core::model_manager::spawn(
        model_dir,
        Duration::from_secs(config.idle_unload_secs),
        model_cmd_rx,
        model_status_tx,
    );
    {
        let tx = unified_tx.clone();
        std::thread::spawn(move || {
            while let Ok(status) = model_status_rx.recv() {
                if tx.send(CoordinatorMsg::Model(status)).is_err() {
                    break;
                }
            }
        });
    }

    // --- cleanup manager ---
    let cleanup_model_path = flow_core::models::cleanup_model_path()
        .expect("could not determine cleanup model path (no home dir?)");
    let (cleanup_cmd_tx, cleanup_cmd_rx) = mpsc::channel::<CleanupCommand>();
    let (cleanup_status_tx, cleanup_status_rx) = mpsc::channel::<CleanupStatusEvent>();
    flow_core::cleanup_manager::spawn(
        cleanup_model_path,
        Duration::from_secs(config.idle_unload_secs),
        cleanup_cmd_rx,
        cleanup_status_tx,
    );
    {
        let tx = unified_tx.clone();
        std::thread::spawn(move || {
            while let Ok(status) = cleanup_status_rx.recv() {
                if tx.send(CoordinatorMsg::Cleanup(status)).is_err() {
                    break;
                }
            }
        });
    }
    *app.state::<AppState>().cleanup_cmd_tx.lock().unwrap() = Some(cleanup_cmd_tx);

    // --- hotkey monitor ---
    let (hotkey_tx, hotkey_rx) = mpsc::channel::<HotkeyEvent>();
    let hotkey_active =
        spawn_hotkey_monitor(&app, config.hotkey_keycode, is_recording.clone(), hotkey_tx);
    {
        let tx = unified_tx.clone();
        std::thread::spawn(move || {
            while let Ok(ev) = hotkey_rx.recv() {
                if tx.send(CoordinatorMsg::Hotkey(ev)).is_err() {
                    break;
                }
            }
        });
    }

    *app.state::<AppState>().audio_cmd_tx.lock().unwrap() = Some(audio_cmd_tx.clone());
    *app.state::<AppState>().model_cmd_tx.lock().unwrap() = Some(model_cmd_tx.clone());
    app.state::<AppState>()
        .hotkey_monitor_active
        .store(hotkey_active, Ordering::Relaxed);

    // --- coordinator thread ---
    {
        let app = app.clone();
        std::thread::spawn(move || {
            run_coordinator(app, unified_rx, audio_cmd_tx, model_cmd_tx);
        });
    }

    (unified_tx, hotkey_active)
}

/// Tracks whether the key is currently physically down and, while down,
/// which "press generation" it belongs to — lets a delayed hold-check
/// ignore itself if the key was released (or pressed again) in the
/// meantime.
///
/// `consumed` guards the tap-vs-hold decision at release time (F4): a press
/// whose recording was cancelled, capped, or otherwise already resolved is
/// marked consumed, so its eventual key-release is a no-op instead of being
/// misread as a fresh short tap that arms hands-free. Only a genuine short
/// tap (press→release under the hold threshold, still unconsumed) toggles
/// hands-free.
struct HoldTracker {
    key_down: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    consumed: Arc<AtomicBool>,
    /// Accidental-press guard (Feature A): set when a keyDown of some other
    /// key arrived while the hold key was physically down. On the default
    /// Right Option binding that means the user is typing a special character
    /// (Option+e = ´ …), not push-to-talking — so this hold must never start
    /// or arm a recording, and any false start already under way is discarded.
    /// Reset on each fresh press. Distinct from `consumed` because it also
    /// forces the *release* to a no-op even if a recording is still mid-cancel
    /// — `consumed` alone leaves a release-while-Recording resolving to
    /// stop-and-transcribe, which would salvage the very audio the guard means
    /// to throw away.
    other_key: Arc<AtomicBool>,
}

/// Captured at release for a rolling recording (Feature B): everything the
/// post-transcription pipeline needs, held until the worker delivers the
/// assembled transcript via [`RollingOutput::Final`]. `epoch` matches the
/// recording that produced it so a late `Final` from an abandoned recording is
/// ignored.
struct PendingRollingFinal {
    app_bundle_id: Option<String>,
    profile: ProfileRule,
    listen_reply: Option<Sender<Result<ListenOutcome, String>>>,
    epoch: u64,
}

/// Action a hold-key *release* resolves to. Pure so the tap-vs-hold decision
/// is unit-testable without a live `AppHandle`; the `HoldKeyReleased` handler
/// drives the side effects for each variant.
#[derive(Debug, PartialEq, Eq)]
enum ReleaseAction {
    /// End the recording under way — a hold-to-talk release, or a hands-free
    /// tap-off.
    StopAndTranscribe,
    /// Arm a hands-free (tap-to-toggle) recording — a genuine short tap.
    ArmHandsFree,
    /// Do nothing: the press was already resolved (cancel/cap/accidental
    /// guard) or we're mid-transcription/paste.
    Noop,
}

/// Decides what a hold-key release does, from the mode flag, the current
/// dictation state, whether the press was already `consumed`, and whether the
/// accidental-press guard fired (`other_key`).
fn decide_release(
    hands_free: bool,
    state: DictationState,
    was_consumed: bool,
    other_key: bool,
) -> ReleaseAction {
    if other_key {
        // Accidental-press guard fired mid-hold (Feature A): any recording is
        // being discarded and the hold must resolve to nothing, whether or not
        // the discard has flipped the state back to Idle yet.
        return ReleaseAction::Noop;
    }
    if hands_free {
        ReleaseAction::StopAndTranscribe
    } else if state == DictationState::Recording {
        ReleaseAction::StopAndTranscribe
    } else if state == DictationState::Idle && !was_consumed {
        ReleaseAction::ArmHandsFree
    } else {
        ReleaseAction::Noop
    }
}

/// Whether the delayed hold-threshold timer should actually begin recording —
/// a pure mirror of the guards the spawned timer applies: still the same
/// press, key still physically down, and the press not already consumed (by an
/// Escape-cancel, a cap, or the accidental-press guard, which all set
/// `consumed`).
fn should_start_after_hold(same_press: bool, still_down: bool, consumed: bool) -> bool {
    same_press && still_down && !consumed
}

fn run_coordinator(
    app: AppHandle,
    rx: mpsc::Receiver<CoordinatorMsg>,
    audio_cmd_tx: Sender<AudioCommand>,
    model_cmd_tx: Sender<ModelCommand>,
) {
    let hold = HoldTracker {
        key_down: Arc::new(AtomicBool::new(false)),
        generation: Arc::new(AtomicU64::new(0)),
        consumed: Arc::new(AtomicBool::new(false)),
        other_key: Arc::new(AtomicBool::new(false)),
    };
    // Rolling-transcription (Feature B) state for the current recording. All
    // live on this one thread, so no locking is needed. `rolling_epoch` is
    // bumped every recording so stale worker output is discarded.
    let mut rolling_in: Option<Sender<RollingInput>> = None;
    let mut rolling_epoch: u64 = 0;
    let mut rolling_preview = String::new();
    let mut pending_rolling: Option<PendingRollingFinal> = None;
    while let Ok(msg) = rx.recv() {
        let state = app.state::<AppState>();
        match msg {
            CoordinatorMsg::Hotkey(HotkeyEvent::HoldKeyPressed) => {
                hold.key_down.store(true, Ordering::Relaxed);
                let gen = hold.generation.fetch_add(1, Ordering::Relaxed) + 1;
                // Fresh press: nothing resolved yet, and no accidental-press
                // (Feature A) guard has fired for it.
                hold.consumed.store(false, Ordering::Relaxed);
                hold.other_key.store(false, Ordering::Relaxed);

                let hands_free = state.hands_free_active.load(Ordering::Relaxed);
                if hands_free {
                    // Already recording in hands-free mode; this press
                    // (whose *release* will decide the action) doesn't
                    // start anything new.
                    continue;
                }
                if *state.dictation_state.lock().unwrap() != DictationState::Idle {
                    continue; // mid-transcription/paste; ignore new presses
                }

                let app2 = app.clone();
                let key_down = hold.key_down.clone();
                let generation = hold.generation.clone();
                let consumed = hold.consumed.clone();
                let (threshold, max_hold_secs) = {
                    let st = app2.state::<AppState>();
                    let cfg = st.config.lock().unwrap();
                    (
                        Duration::from_millis(cfg.hold_threshold_ms),
                        cfg.max_hold_secs,
                    )
                };
                std::thread::spawn(move || {
                    std::thread::sleep(threshold);
                    let same_press = generation.load(Ordering::Relaxed) == gen;
                    let still_down = key_down.load(Ordering::Relaxed);
                    let consumed = consumed.load(Ordering::Relaxed);
                    if should_start_after_hold(same_press, still_down, consumed) {
                        let state = app2.state::<AppState>();
                        if *state.dictation_state.lock().unwrap() == DictationState::Idle {
                            start_recording(&app2, max_hold_secs);
                        }
                    }
                });
            }
            CoordinatorMsg::Hotkey(HotkeyEvent::HoldKeyReleased) => {
                hold.key_down.store(false, Ordering::Relaxed);
                // This release resolves the current press no matter what;
                // mark it consumed so any later re-entry can't reuse it.
                let was_consumed = hold.consumed.swap(true, Ordering::Relaxed);
                let other_key = hold.other_key.load(Ordering::Relaxed);
                let hands_free = state.hands_free_active.load(Ordering::Relaxed);
                let current = *state.dictation_state.lock().unwrap();

                // The `!was_consumed` guard (F4) stops an Idle reached via
                // Escape-cancel / cap from being misread as a fresh tap that
                // silently arms hands-free; the `other_key` guard (Feature A)
                // additionally forces a no-op even while a discarded recording
                // is still mid-cancel. See [`decide_release`].
                match decide_release(hands_free, current, was_consumed, other_key) {
                    ReleaseAction::StopAndTranscribe => {
                        state.hands_free_active.store(false, Ordering::Relaxed);
                        stop_and_transcribe(&audio_cmd_tx);
                    }
                    ReleaseAction::ArmHandsFree => {
                        state.hands_free_active.store(true, Ordering::Relaxed);
                        start_recording(&app, max_handsfree_secs(&app));
                    }
                    ReleaseAction::Noop => {}
                }
            }
            CoordinatorMsg::Hotkey(HotkeyEvent::OtherKeyDuringHold) => {
                // Accidental-press guard (Feature A). A keyDown of some other
                // key arrived while the hold key was down — with the default
                // Right Option binding that is the macOS special-character
                // modifier at work (Option+e = ´ …), so this is the user
                // typing, not push-to-talking. Only act while a hold is
                // genuinely in flight; a late/stale event after release must
                // not disturb a subsequent press.
                if hold.key_down.load(Ordering::Relaxed) {
                    hold.other_key.store(true, Ordering::Relaxed);
                    // Mark consumed so the delayed hold-check won't start a
                    // recording and the release won't arm hands-free.
                    hold.consumed.store(true, Ordering::Relaxed);
                    if *state.dictation_state.lock().unwrap() == DictationState::Recording {
                        // A false start already began (hold outlived the
                        // threshold before the special char was typed):
                        // discard it — the user is typing, not dictating.
                        state.hands_free_active.store(false, Ordering::Relaxed);
                        let _ = audio_cmd_tx.send(AudioCommand::Cancel);
                    }
                }
            }
            CoordinatorMsg::Hotkey(HotkeyEvent::CancelRequested) => {
                if *state.dictation_state.lock().unwrap() == DictationState::Recording {
                    // The recording is being thrown away; mark the in-flight
                    // press consumed so its release doesn't arm hands-free (F4).
                    hold.consumed.store(true, Ordering::Relaxed);
                    state.hands_free_active.store(false, Ordering::Relaxed);
                    let _ = audio_cmd_tx.send(AudioCommand::Cancel);
                }
            }
            CoordinatorMsg::TrayToggleDictation => {
                let current = *state.dictation_state.lock().unwrap();
                if current == DictationState::Idle {
                    // Manual start behaves like a hands-free session; consume
                    // any dangling press so a stray release can't double-toggle.
                    hold.consumed.store(true, Ordering::Relaxed);
                    state.hands_free_active.store(true, Ordering::Relaxed);
                    start_recording(&app, max_handsfree_secs(&app));
                } else if current == DictationState::Recording {
                    state.hands_free_active.store(false, Ordering::Relaxed);
                    stop_and_transcribe(&audio_cmd_tx);
                }
            }
            CoordinatorMsg::Audio(AudioReply::Started) => {
                // Spin up this recording's rolling-transcription worker
                // (Feature B) if enabled. Created here on the coordinator
                // thread — not in `start_recording`, which may run on a timer
                // thread. A new epoch abandons any previous worker's output.
                rolling_in = None;
                rolling_epoch = rolling_epoch.wrapping_add(1);
                rolling_preview.clear();
                pending_rolling = None;
                let rolling_enabled = state.config.lock().unwrap().rolling_transcription;
                if rolling_enabled {
                    let epoch = rolling_epoch;
                    let (out_tx, out_rx) = mpsc::channel::<RollingOutput>();
                    if let Some(coord_tx) = state.coordinator_tx.lock().unwrap().clone() {
                        // Forward the worker's output onto the coordinator
                        // channel, epoch-tagged. Exits when the worker drops
                        // `out_tx` (recording finalized or abandoned).
                        std::thread::spawn(move || {
                            while let Ok(output) = out_rx.recv() {
                                if coord_tx
                                    .send(CoordinatorMsg::Rolling { epoch, output })
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        });
                        rolling_in =
                            Some(rolling::spawn_rolling_worker(model_cmd_tx.clone(), out_tx));
                    }
                }
            }
            CoordinatorMsg::Audio(AudioReply::RollingSamples { samples }) => {
                // Feed settled audio to the rolling worker (if one is running
                // for this recording); it cuts and dispatches chunks.
                if let Some(rin) = &rolling_in {
                    let _ = rin.send(RollingInput::Samples(samples));
                }
            }
            CoordinatorMsg::Audio(AudioReply::Level(level)) => {
                let elapsed = state
                    .recording_started
                    .lock()
                    .unwrap()
                    .map(|s| s.elapsed())
                    .unwrap_or_default();
                let max_secs = state.recording_max_secs.lock().unwrap().unwrap_or(0);
                overlay::emit_overlay(&app, overlay::recording_event(level, elapsed, max_secs));
            }
            CoordinatorMsg::Audio(AudioReply::Stopped { samples, duration, capped, auto_stopped_silence }) => {
                if capped {
                    // The worker auto-stopped at the max-duration cap while the
                    // key may still be physically held. Reset the mode flag and
                    // consume the in-flight press so its eventual release is a
                    // no-op rather than a surprise hands-free toggle (F3/F4).
                    state.hands_free_active.store(false, Ordering::Relaxed);
                    hold.consumed.store(true, Ordering::Relaxed);
                    eprintln!("[vzt-flow] recording hit max-duration cap; transcribing what was captured");
                } else if auto_stopped_silence {
                    // Hands-free VAD auto-stop: same reset as the cap path, but
                    // it's not a max-duration hit, just the end of speech.
                    state.hands_free_active.store(false, Ordering::Relaxed);
                    hold.consumed.store(true, Ordering::Relaxed);
                }
                state.set_dictation_state(DictationState::Transcribing);
                tray::refresh_menu(&app);

                // A daemon `listen` command's reply channel (+ optional mode
                // override), if this recording was triggered that way rather
                // than via the hotkey/tray toggle.
                let listen_pending = state.pending_listen.lock().unwrap().take();

                // Frontmost app + resolved profile, captured now (right as
                // recording ends) rather than after ASR completes — that's
                // both a more accurate "at paste time" reading and lets the
                // overlay show the mode badge immediately.
                let app_bundle_id = permissions::frontmost_bundle_id();
                let mut profile = state.profiles.lock().unwrap().resolve(app_bundle_id.as_deref());
                if let Some((_, Some(mode_override))) = &listen_pending {
                    profile.mode = mode_override.clone();
                }
                let listen_reply = listen_pending.map(|(tx, _)| tx);
                overlay::emit_overlay(&app, OverlayEvent::Transcribing { mode: profile.mode.clone() });

                // Rolling path (Feature B): the worker already holds the audio
                // and has transcribed everything but the tail. Tell it to
                // finalize; the assembled transcript comes back as
                // `RollingOutput::Final`. A watchdog guards against a wedged
                // engine stranding us in Transcribing (F2), mirroring the batch
                // timeout below. `samples` is empty in rolling mode.
                if let Some(rin) = rolling_in.take() {
                    let _ = rin.send(RollingInput::Finish);
                    pending_rolling = Some(PendingRollingFinal {
                        app_bundle_id,
                        profile,
                        listen_reply,
                        epoch: rolling_epoch,
                    });
                    if let Some(tx) = state.coordinator_tx.lock().unwrap().clone() {
                        let epoch = rolling_epoch;
                        let timeout = duration + Duration::from_secs(60);
                        std::thread::spawn(move || {
                            std::thread::sleep(timeout);
                            let _ = tx.send(CoordinatorMsg::RollingTimeout { epoch });
                        });
                    }
                    continue;
                }

                let (reply_tx, reply_rx) = mpsc::channel();
                let sent = model_cmd_tx.send(ModelCommand::Transcribe {
                    samples,
                    audio_duration: duration,
                    reply: reply_tx,
                });
                if sent.is_err() {
                    if let Some(tx) = &listen_reply {
                        let _ = tx.send(Err("transcriber unavailable".to_string()));
                    }
                    state.set_dictation_state(DictationState::Idle);
                    overlay::hide_overlay(&app);
                    continue;
                }
                let forward_tx = state.coordinator_tx.lock().unwrap().clone();
                std::thread::spawn(move || {
                    // Never wait forever on the transcriber. A dropped reply
                    // channel (panicked worker) surfaces as RecvError; a wedged
                    // inference trips the timeout. Either way synthesize an
                    // error result so the state machine leaves Transcribing and
                    // the overlay is dismissed instead of hanging (F2).
                    //
                    // Scaled with audio duration (+60s margin) rather than a
                    // flat 60s: Parakeet's measured real-time factor is well
                    // under 1x, but a flat cap sized for short dictations
                    // would falsely time out a legitimate 10min long-form
                    // recording if RTF is ever not tiny.
                    let transcribe_timeout = duration + Duration::from_secs(60);
                    let result = match reply_rx.recv_timeout(transcribe_timeout) {
                        Ok(result) => result,
                        Err(_) => Err("transcription failed".to_string()),
                    };
                    if let Some(tx) = forward_tx {
                        let _ = tx.send(CoordinatorMsg::TranscribeResult {
                            result,
                            audio_duration: duration,
                            app_bundle_id,
                            profile,
                            listen_reply,
                        });
                    }
                });
            }
            CoordinatorMsg::Audio(AudioReply::Disconnected { samples, duration }) => {
                // Input device faulted mid-recording (F8). Reset mode flags and
                // consume the in-flight press, then either salvage the take
                // (worker already discarded anything under ~1s, handing back an
                // empty buffer) or show a brief "mic disconnected" note.
                state.hands_free_active.store(false, Ordering::Relaxed);
                hold.consumed.store(true, Ordering::Relaxed);

                // Rolling (Feature B): the streamed audio is already with the
                // worker (`samples` is always empty here in rolling mode), so
                // finalize via a synthetic Stopped — routed through the rolling
                // path above — when there's enough to bother with, else abandon.
                if rolling_in.is_some() {
                    if duration.as_secs_f64() >= 1.0 {
                        eprintln!(
                            "[vzt-flow] microphone disconnected mid-recording; finalizing the {:.1}s captured (rolling)",
                            duration.as_secs_f64()
                        );
                        if let Some(tx) = state.coordinator_tx.lock().unwrap().clone() {
                            let _ = tx.send(CoordinatorMsg::Audio(AudioReply::Stopped {
                                samples: Vec::new(),
                                duration,
                                capped: false,
                                auto_stopped_silence: false,
                            }));
                        }
                    } else {
                        eprintln!("[vzt-flow] microphone disconnected mid-recording; nothing to salvage (rolling)");
                        rolling_in = None;
                        rolling_epoch = rolling_epoch.wrapping_add(1);
                        rolling_preview.clear();
                        if let Some((tx, _)) = state.pending_listen.lock().unwrap().take() {
                            let _ = tx.send(Err("microphone disconnected".to_string()));
                        }
                        state.set_dictation_state(DictationState::Idle);
                        overlay::emit_overlay(
                            &app,
                            OverlayEvent::Message { text: "Microphone disconnected".to_string() },
                        );
                        let app2 = app.clone();
                        std::thread::spawn(move || {
                            std::thread::sleep(Duration::from_millis(1500));
                            overlay::hide_overlay(&app2);
                        });
                    }
                    continue;
                }

                if samples.is_empty() {
                    eprintln!("[vzt-flow] microphone disconnected mid-recording; nothing to salvage");
                    if let Some((tx, _)) = state.pending_listen.lock().unwrap().take() {
                        let _ = tx.send(Err("microphone disconnected".to_string()));
                    }
                    state.set_dictation_state(DictationState::Idle);
                    overlay::emit_overlay(
                        &app,
                        OverlayEvent::Message { text: "Microphone disconnected".to_string() },
                    );
                    let app2 = app.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_millis(1500));
                        overlay::hide_overlay(&app2);
                    });
                } else {
                    eprintln!("[vzt-flow] microphone disconnected mid-recording; transcribing the {:.1}s captured", duration.as_secs_f64());
                    if let Some(tx) = state.coordinator_tx.lock().unwrap().clone() {
                        let _ = tx.send(CoordinatorMsg::Audio(AudioReply::Stopped {
                            samples,
                            duration,
                            capped: false,
                            auto_stopped_silence: false,
                        }));
                    }
                }
            }
            CoordinatorMsg::Audio(AudioReply::Cancelled) => {
                // Abandon any rolling worker (Feature B): dropping its sender
                // exits it, and the epoch bump discards any output already
                // queued from it.
                rolling_in = None;
                rolling_epoch = rolling_epoch.wrapping_add(1);
                rolling_preview.clear();
                pending_rolling = None;
                if let Some((tx, _)) = state.pending_listen.lock().unwrap().take() {
                    let _ = tx.send(Err("recording cancelled".to_string()));
                }
                state.set_dictation_state(DictationState::Idle);
                overlay::hide_overlay(&app);
            }
            CoordinatorMsg::Audio(AudioReply::Error(e)) => {
                eprintln!("[vzt-flow] audio error: {e}");
                rolling_in = None;
                rolling_epoch = rolling_epoch.wrapping_add(1);
                rolling_preview.clear();
                pending_rolling = None;
                if let Some((tx, _)) = state.pending_listen.lock().unwrap().take() {
                    let _ = tx.send(Err(e.clone()));
                }
                state.set_dictation_state(DictationState::Idle);
                overlay::emit_overlay(&app, OverlayEvent::Message { text: e });
                let app2 = app.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(1500));
                    overlay::hide_overlay(&app2);
                });
            }
            CoordinatorMsg::Rolling { epoch, output } => {
                // Discard output from an abandoned or older recording.
                if epoch != rolling_epoch {
                    continue;
                }
                match output {
                    RollingOutput::Preview { chunk_text } => {
                        // Live preview (Feature B): only while still recording,
                        // dictionary-corrected (no LLM) for display. Appended to
                        // the running raw tail; the overlay shows its last chars.
                        if *state.dictation_state.lock().unwrap() == DictationState::Recording {
                            let corrected = {
                                let dict = state.dictionary.lock().unwrap().clone();
                                dictionary::correct(&chunk_text, &dict)
                            };
                            let corrected = corrected.trim();
                            if !corrected.is_empty() {
                                if !rolling_preview.is_empty() {
                                    rolling_preview.push(' ');
                                }
                                rolling_preview.push_str(corrected);
                                overlay::emit_overlay(
                                    &app,
                                    OverlayEvent::Preview { text: rolling_preview.clone() },
                                );
                            }
                        }
                    }
                    RollingOutput::Final { raw_text, audio_duration } => {
                        rolling_preview.clear();
                        if let Some(p) = pending_rolling.take() {
                            if p.epoch == epoch {
                                run_pipeline(
                                    &app,
                                    raw_text,
                                    audio_duration,
                                    p.app_bundle_id,
                                    p.profile,
                                    p.listen_reply,
                                );
                                tray::refresh_menu(&app);
                            }
                        }
                    }
                }
            }
            CoordinatorMsg::RollingTimeout { epoch } => {
                // A rolling finalize wedged (F2). Only act if it's still the
                // current recording and hasn't finalized in the meantime.
                if epoch == rolling_epoch {
                    if let Some(p) = pending_rolling.take() {
                        eprintln!("[vzt-flow] rolling transcription timed out; leaving Transcribing (F2)");
                        rolling_in = None;
                        rolling_preview.clear();
                        if let Some(tx) = p.listen_reply {
                            let _ = tx.send(Err("transcription timed out".to_string()));
                        }
                        state.set_dictation_state(DictationState::Idle);
                        overlay::emit_overlay(
                            &app,
                            OverlayEvent::Message { text: "Transcription failed".to_string() },
                        );
                        let app2 = app.clone();
                        std::thread::spawn(move || {
                            std::thread::sleep(Duration::from_millis(1500));
                            overlay::hide_overlay(&app2);
                        });
                    }
                }
            }
            CoordinatorMsg::TranscribeResult { result, audio_duration, app_bundle_id, profile, listen_reply } => {
                match result {
                    Ok(transcript) => {
                        run_pipeline(&app, transcript.text, audio_duration, app_bundle_id, profile, listen_reply);
                    }
                    Err(e) => {
                        eprintln!("[vzt-flow] transcription error: {e}");
                        if let Some(tx) = listen_reply {
                            let _ = tx.send(Err(e));
                        }
                        state.set_dictation_state(DictationState::Idle);
                        // Surface the failure briefly instead of silently
                        // vanishing, then dismiss (F2).
                        overlay::emit_overlay(
                            &app,
                            OverlayEvent::Message { text: "Transcription failed".to_string() },
                        );
                        let app2 = app.clone();
                        std::thread::spawn(move || {
                            std::thread::sleep(Duration::from_millis(1500));
                            overlay::hide_overlay(&app2);
                        });
                    }
                }
                tray::refresh_menu(&app);
            }
            CoordinatorMsg::CleanupDone { raw_text, result, mode_label, audio_duration, app_bundle_id, listen_reply } => {
                finalize_dictation(
                    &app,
                    &raw_text,
                    &result.text,
                    &mode_label,
                    audio_duration,
                    app_bundle_id,
                    listen_reply,
                );
            }
            CoordinatorMsg::Model(ModelStatusEvent::Loading) => {
                *state.model_lifecycle.lock().unwrap() = ModelLifecycle::Loading;
                tray::refresh_menu(&app);
            }
            CoordinatorMsg::Model(ModelStatusEvent::Loaded { load_time }) => {
                *state.model_lifecycle.lock().unwrap() = ModelLifecycle::Loaded;
                eprintln!("[vzt-flow] model loaded in {:.2}s", load_time.as_secs_f64());
                tray::refresh_menu(&app);
            }
            CoordinatorMsg::Model(ModelStatusEvent::LoadFailed(e)) => {
                eprintln!("[vzt-flow] model load failed: {e}");
                *state.model_lifecycle.lock().unwrap() = ModelLifecycle::Unloaded;
                tray::refresh_menu(&app);
            }
            CoordinatorMsg::Model(ModelStatusEvent::Unloaded) => {
                *state.model_lifecycle.lock().unwrap() = ModelLifecycle::Unloaded;
                tray::refresh_menu(&app);
            }
            // cleanup_manager already logs its own lifecycle to stderr;
            // mirrored into `cleanup_lifecycle` so the daemon socket's
            // `status` command can report `cleanup_loaded`.
            CoordinatorMsg::Cleanup(status) => {
                let lifecycle = match status {
                    CleanupStatusEvent::Loading => ModelLifecycle::Loading,
                    CleanupStatusEvent::Loaded { .. } => ModelLifecycle::Loaded,
                    CleanupStatusEvent::LoadFailed(_) => ModelLifecycle::Unloaded,
                    CleanupStatusEvent::Unloaded => ModelLifecycle::Unloaded,
                };
                *state.cleanup_lifecycle.lock().unwrap() = lifecycle;
            }
            CoordinatorMsg::TestOverlay => {
                run_overlay_self_test(&app);
            }
            CoordinatorMsg::DaemonListen { mode, max_secs, reply } => {
                if *state.dictation_state.lock().unwrap() != DictationState::Idle {
                    let _ = reply.send(Err("already recording or transcribing".to_string()));
                    continue;
                }
                let cap = max_secs.unwrap_or_else(|| max_handsfree_secs(&app));
                *state.pending_listen.lock().unwrap() = Some((reply, mode));
                // Behaves like a hands-free (tap-to-toggle) recording: RMS
                // auto-stop is enabled by `start_recording` whenever
                // `hands_free_active` is set, and there's no hotkey press
                // in flight to consume, so no `hold` bookkeeping is needed.
                state.hands_free_active.store(true, Ordering::Relaxed);
                start_recording(&app, cap);
            }
        }
    }
}

/// Reads the current hands-free max-recording cap from config.
fn max_handsfree_secs(app: &AppHandle) -> u64 {
    app.state::<AppState>()
        .config
        .lock()
        .unwrap()
        .max_handsfree_secs
}

fn start_recording(app: &AppHandle, max_secs: u64) {
    let state = app.state::<AppState>();
    state.set_dictation_state(DictationState::Recording);
    *state.recording_started.lock().unwrap() = Some(std::time::Instant::now());
    *state.recording_max_secs.lock().unwrap() = Some(max_secs);
    tray::refresh_menu(app);
    overlay::show_overlay(app);
    overlay::emit_overlay(app, overlay::recording_event(0.0, Duration::ZERO, max_secs));
    // Energy-based auto-stop only applies to hands-free recordings — a
    // hold-to-talk recording's only stop signal is releasing the key.
    let (handsfree_silence_secs, rolling) = {
        let cfg = state.config.lock().unwrap();
        let hf = state
            .hands_free_active
            .load(Ordering::Relaxed)
            .then_some(cfg.handsfree_silence_secs);
        (hf, cfg.rolling_transcription)
    };
    let tx = state.audio_cmd_tx.lock().unwrap().clone();
    if let Some(tx) = tx {
        // `rolling` must match what the `Started` handler reads from config to
        // decide whether to create a rolling worker — both read the same flag.
        let _ = tx.send(AudioCommand::Start { max_secs, handsfree_silence_secs, rolling });
    }

    // Kick off cleanup-model load + Metal warm-up now, in parallel with the
    // user speaking, so it's already warm by the time transcription
    // finishes and the real (deadline-bound) cleanup call runs. The manager
    // no-ops if it's already loaded/warmed.
    let cleanup_tx = state.cleanup_cmd_tx.lock().unwrap().clone();
    if let Some(cleanup_tx) = cleanup_tx {
        let _ = cleanup_tx.send(CleanupCommand::Warmup);
    }
}

fn stop_and_transcribe(audio_cmd_tx: &Sender<AudioCommand>) {
    let _ = audio_cmd_tx.send(AudioCommand::Stop);
}

/// Runs the post-ASR pipeline: dictionary correction, then either code
/// mode (deterministic, no LLM, synchronous) or the cleanup LLM (async,
/// deadline-bound — see `cleanup_manager`). Either branch ends by calling
/// [`finalize_dictation`], directly for code mode or via
/// `CoordinatorMsg::CleanupDone` for the LLM path.
fn run_pipeline(
    app: &AppHandle,
    raw_text: String,
    audio_duration: Duration,
    app_bundle_id: Option<String>,
    profile: ProfileRule,
    listen_reply: Option<Sender<Result<ListenOutcome, String>>>,
) {
    let state = app.state::<AppState>();
    let dict = state.dictionary.lock().unwrap().clone();
    let corrected = dictionary::correct(&raw_text, &dict);

    if profile.mode == "code" {
        let final_text = codemode::transform(&corrected);
        finalize_dictation(app, &raw_text, &final_text, "code", audio_duration, app_bundle_id, listen_reply);
        return;
    }

    let mode = Mode::parse(&profile.mode);
    let mode_label = mode.label().to_string();

    if mode == Mode::Raw {
        // No LLM involved at all in raw mode; finish synchronously.
        finalize_dictation(app, &raw_text, &corrected, &mode_label, audio_duration, app_bundle_id, listen_reply);
        return;
    }

    let timeout_ms = {
        let cfg = state.config.lock().unwrap();
        flow_core::cleanup_manager::cleanup_deadline_ms(corrected.chars().count(), &cfg)
    };
    let dictionary_terms: Vec<String> = dict.iter().map(|d| d.term.clone()).collect();
    let ctx = CleanupContext { app_name: app_bundle_id.clone(), tone: profile.tone.clone(), dictionary_terms };
    let cleanup_tx = state.cleanup_cmd_tx.lock().unwrap().clone();

    let Some(cleanup_tx) = cleanup_tx else {
        finalize_dictation(app, &raw_text, &corrected, &mode_label, audio_duration, app_bundle_id, listen_reply);
        return;
    };

    let (reply_tx, reply_rx) = mpsc::channel();
    let sent = cleanup_tx.send(CleanupCommand::Clean {
        raw: corrected.clone(),
        mode,
        ctx,
        timeout_ms,
        reply: reply_tx,
    });
    if sent.is_err() {
        finalize_dictation(app, &raw_text, &corrected, &mode_label, audio_duration, app_bundle_id, listen_reply);
        return;
    }

    let forward_tx = state.coordinator_tx.lock().unwrap().clone();
    std::thread::spawn(move || {
        // The cleanup manager itself enforces the deadline internally and
        // always replies; this is just a backstop in case its reply
        // channel is ever dropped without a send (e.g. a manager panic).
        let result = reply_rx
            .recv_timeout(Duration::from_millis(timeout_ms + 2_000))
            .unwrap_or(CleanupResult { text: corrected, used_llm: false });
        if let Some(tx) = forward_tx {
            let _ = tx.send(CoordinatorMsg::CleanupDone {
                raw_text,
                result,
                mode_label,
                audio_duration,
                app_bundle_id,
                listen_reply,
            });
        }
    });
}

fn finalize_dictation(
    app: &AppHandle,
    raw_text: &str,
    cleaned_text: &str,
    mode_label: &str,
    audio_duration: Duration,
    app_bundle_id: Option<String>,
    listen_reply: Option<Sender<Result<ListenOutcome, String>>>,
) {
    let state = app.state::<AppState>();

    let snips = state.snippets.lock().unwrap().clone();
    let final_text = snippets::expand(cleaned_text, &snips).unwrap_or_else(|| cleaned_text.to_string());

    *state.last_transcript.lock().unwrap() = Some(final_text.clone());

    // A daemon `listen` command never pastes — it hands the text back over
    // the socket instead. Everything else (history logging, overlay
    // Done flash, state reset) is identical to a normal dictation.
    let message = if let Some(tx) = listen_reply {
        let _ = tx.send(Ok(ListenOutcome {
            raw: raw_text.to_string(),
            text: final_text.clone(),
            mode: mode_label.to_string(),
            duration_s: audio_duration.as_secs_f64(),
        }));
        None
    } else {
        let outcome = insert::paste_text(&final_text);
        match &outcome {
            Ok(insert::PasteOutcome::Pasted) => None,
            Ok(insert::PasteOutcome::SkippedSecureField) => {
                Some("Secure field — transcript on clipboard".to_string())
            }
            Ok(insert::PasteOutcome::SkippedNoAccessibility) => {
                Some("No Accessibility permission — transcript on clipboard".to_string())
            }
            Ok(insert::PasteOutcome::ClipboardOnly) => {
                // Linux/Wayland: no X server for the synthetic Ctrl+V. The
                // overlay pill is brief, so also fire a desktop notification
                // making the "paste manually" instruction discoverable.
                notify_clipboard_only(app);
                Some("Transcript on clipboard — press Ctrl+V".to_string())
            }
            Ok(insert::PasteOutcome::VerificationFailed) => {
                // Feature C: Cmd+V was sent but Accessibility verification found
                // the transcript wasn't in the focused field even after a
                // retry. The transcript is left on the clipboard (not
                // restored); surface that plus a notification since the overlay
                // pill is brief.
                notify_paste_maybe_failed(app);
                Some("Paste may have failed — transcript on clipboard".to_string())
            }
            Err(e) => Some(format!("Paste failed: {e}")),
        }
    };

    let entry = history::HistoryEntry {
        ts: history::now_unix(),
        app: app_bundle_id,
        raw_text: raw_text.to_string(),
        duration_s: audio_duration.as_secs_f64(),
        rtf: 0.0, // logged to stderr by the model manager; not recomputed here
        clean_text: final_text,
        mode: mode_label.to_string(),
    };
    if let Err(e) = history::append(&entry) {
        eprintln!("[vzt-flow] failed to append history: {e}");
    }

    state.set_dictation_state(DictationState::Done);
    if let Some(text) = message {
        overlay::emit_overlay(app, OverlayEvent::Message { text });
    } else {
        overlay::emit_overlay(app, OverlayEvent::Done);
    }

    let app2 = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(900));
        let state = app2.state::<AppState>();
        state.set_dictation_state(DictationState::Idle);
        overlay::hide_overlay(&app2);
    });
}

/// Best-effort desktop notification for the Linux/Wayland clipboard-only
/// paste path (see `insert::PasteOutcome::ClipboardOnly`). Failures (e.g. the
/// Notifications permission not granted) are swallowed — the transcript is
/// already on the clipboard and the overlay pill also shows the hint.
fn notify_clipboard_only(app: &AppHandle) {
    if let Err(e) = app
        .notification()
        .builder()
        .title("VZT Flow")
        .body("Transcript copied to clipboard — press Ctrl+V to paste (Wayland can't auto-paste).")
        .show()
    {
        eprintln!("[vzt-flow] clipboard-only notification failed: {e}");
    }
}

/// Best-effort desktop notification for the Feature C paste-verification
/// failure path (see `insert::PasteOutcome::VerificationFailed`). Failures are
/// swallowed — the transcript is already on the clipboard and the overlay pill
/// also shows the hint.
fn notify_paste_maybe_failed(app: &AppHandle) {
    if let Err(e) = app
        .notification()
        .builder()
        .title("VZT Flow")
        .body("Paste may have failed — transcript is on the clipboard, paste it manually.")
        .show()
    {
        eprintln!("[vzt-flow] paste-verification notification failed: {e}");
    }
}

/// Cycles the overlay through recording -> transcribing -> done -> hidden,
/// with fake level values, entirely for visual QA via the "Test overlay"
/// tray item — no microphone or transcriber involved.
fn run_overlay_self_test(app: &AppHandle) {
    overlay::show_overlay(app);
    let app2 = app.clone();
    std::thread::spawn(move || {
        // Fake a 40s cap so the last two steps land inside the 30s warning
        // window, exercising the elapsed-time readout and warning styling
        // (F-series "10min hold with no feedback" gap) without needing a
        // real multi-minute recording.
        let fake_max_secs = 40u64;
        let steps: [(f32, f64); 5] = [(0.1, 0.0), (0.4, 5.0), (0.8, 9.5), (0.5, 10.5), (0.2, 12.0)];
        for (level, elapsed_secs) in steps {
            overlay::emit_overlay(
                &app2,
                overlay::recording_event(level, Duration::from_secs_f64(elapsed_secs), fake_max_secs),
            );
            std::thread::sleep(Duration::from_millis(250));
        }
        overlay::emit_overlay(&app2, OverlayEvent::Transcribing { mode: "clean".to_string() });
        std::thread::sleep(Duration::from_millis(700));
        overlay::emit_overlay(&app2, OverlayEvent::Done);
        std::thread::sleep(Duration::from_millis(900));
        overlay::hide_overlay(&app2);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Feature A: accidental-press guard state transitions ----

    #[test]
    fn release_arms_hands_free_only_on_a_genuine_unconsumed_tap() {
        // Idle, not hands-free, not consumed, no other key: a real short tap.
        assert_eq!(
            decide_release(false, DictationState::Idle, false, false),
            ReleaseAction::ArmHandsFree
        );
    }

    #[test]
    fn release_stops_a_hold_to_talk_recording() {
        // Hold outlived the threshold; releasing ends the recording.
        assert_eq!(
            decide_release(false, DictationState::Recording, false, false),
            ReleaseAction::StopAndTranscribe
        );
    }

    #[test]
    fn release_toggles_hands_free_off() {
        // A tap while hands-free was live stops it, regardless of state.
        assert_eq!(
            decide_release(true, DictationState::Recording, false, false),
            ReleaseAction::StopAndTranscribe
        );
        assert_eq!(
            decide_release(true, DictationState::Idle, false, false),
            ReleaseAction::StopAndTranscribe
        );
    }

    #[test]
    fn consumed_release_is_a_noop_not_a_surprise_hands_free_start() {
        // F4: an Idle reached via cancel/cap is consumed — must not arm.
        assert_eq!(
            decide_release(false, DictationState::Idle, true, false),
            ReleaseAction::Noop
        );
    }

    #[test]
    fn accidental_press_guard_forces_noop_even_while_recording() {
        // Feature A: the special-character case. Other-key fired mid-hold, so
        // the release must be a no-op even if the discard hasn't flipped the
        // state back to Idle yet — otherwise we'd stop-and-transcribe (salvage)
        // audio the guard is deliberately throwing away.
        assert_eq!(
            decide_release(false, DictationState::Recording, false, true),
            ReleaseAction::Noop
        );
        // And of course when it has already returned to Idle.
        assert_eq!(
            decide_release(false, DictationState::Idle, true, true),
            ReleaseAction::Noop
        );
        // Even a hands-free flag can't override the guard.
        assert_eq!(
            decide_release(true, DictationState::Recording, false, true),
            ReleaseAction::Noop
        );
    }

    #[test]
    fn hold_start_requires_same_press_still_down_and_unconsumed() {
        assert!(should_start_after_hold(true, true, false));
        // A different press generation (key was re-pressed): this timer is stale.
        assert!(!should_start_after_hold(false, true, false));
        // Key already released before the threshold (a tap): don't start.
        assert!(!should_start_after_hold(true, false, false));
        // Consumed (Escape-cancel, cap, or the accidental-press guard): don't start.
        assert!(!should_start_after_hold(true, true, true));
    }
}
