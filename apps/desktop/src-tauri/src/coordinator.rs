//! The dictation state machine. Owns no UI directly — it drives the tray
//! label, the overlay window, and the paste/history pipeline in response
//! to hotkey and audio/model events. All state transitions happen on one
//! thread so there's a single source of truth for "what state are we in".

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use flow_core::audio::{AudioCommand, AudioReply};
use flow_core::cleanup::{CleanupContext, Mode};
use flow_core::cleanup_manager::{CleanupCommand, CleanupResult, CleanupStatusEvent};
use flow_core::config::Config;
use flow_core::hotkey::HotkeyEvent;
use flow_core::model_manager::{ModelCommand, ModelStatusEvent};
use flow_core::profiles::ProfileRule;
use flow_core::recovery::AudioStats;
use flow_core::rolling::{self, RollingInput, RollingOutput};
use flow_core::{codemode, dictionary, history, insert, permissions, snippets};
use tauri::{AppHandle, Manager};
use tauri_plugin_notification::NotificationExt;

use crate::overlay::{self, OverlayEvent};
use crate::state::{AppState, DictationState, LockRecover, ModelLifecycle};
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
        /// The recording itself, kept so a failed or empty transcription can
        /// be saved for recovery instead of vanishing.
        samples: Vec<f32>,
        stats: AudioStats,
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
        meta: DictationMeta,
    },
    /// A message from a recording's rolling-transcription worker (Feature B),
    /// tagged with the recording `epoch`. Previews from an older recording
    /// are ignored; a `Final`/`Late` never is (see [`route_rolling_final`]).
    Rolling { epoch: u64, output: RollingOutput },
    /// The rolling worker for `epoch` exited without ever sending a `Final`
    /// (it panicked outside its own recovery, or was abandoned). Replaces the
    /// old `duration + 60s` timer, which discarded healthy-but-slow takes: the
    /// worker now bounds its own wait by *progress* and always answers, so
    /// the coordinator only has to notice the worker disappearing.
    RollingWorkerLost { epoch: u64 },
    /// Tray "Recover last recording": re-transcribe the saved recording.
    RecoverLastRecording,
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
        *app.state::<AppState>().hotkey_keycode_handle.lock_or_recover() = Some(keycode_handle.clone());
    } else {
        eprintln!(
            "[vzt-flow] hotkey monitor failed to install a CGEventTap — this almost always means \
             Input Monitoring permission hasn't been granted (System Settings > Privacy & Security \
             > Input Monitoring). The tray's manual Start/Stop item still works; the re-arm driver \
             will arm the tap automatically the moment the grant lands (no restart)."
        );
    }
    active
}

/// How often the macOS late-grant re-arm driver polls Input Monitoring access
/// while the hotkey tap is unarmed. Matched to the Settings permission poll
/// (2s) so the dot flips green within a tick of the user granting. While
/// permission is denied each tick is a single non-prompting `IOHIDCheckAccess`
/// call (microseconds) on an otherwise-sleeping thread — `CGEventTapCreate` is
/// only ever attempted once the grant lands.
#[cfg(target_os = "macos")]
const REARM_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// macOS-only late-grant recovery. The `CGEventTap` is only spawned once, at
/// launch (see [`spawn`]). A brand-new user who launches, is told to grant
/// Input Monitoring, and grants it would otherwise be stuck with a dead hotkey
/// until they quit and relaunch — the exact "installed but not working" gap
/// this closes. Spawned only when the launch-time tap install failed.
///
/// It polls the grant on [`REARM_POLL_INTERVAL`] and, the instant it reads
/// `Granted`, arms the tap in place. Safety of re-arming rests entirely on
/// [`flow_core::hotkey::rearm_tick`]/`should_attempt_arm`: `spawn_monitor` is
/// **not idempotent** (a success parks an unkillable `CFRunLoop` thread holding
/// the tap), so we attempt an arm only while unarmed AND granted, and `return`
/// the instant one succeeds — `spawn_monitor` is thus called at most once from
/// here, on the grant transition. A *failed* `spawn_monitor` returns
/// immediately without leaking, so the rare granted-but-failed case retries
/// safely on the next tick.
#[cfg(target_os = "macos")]
fn spawn_hotkey_rearm_driver(
    app: AppHandle,
    is_recording: Arc<AtomicBool>,
    tx: Sender<HotkeyEvent>,
) {
    std::thread::Builder::new()
        .name("vzt-flow-hotkey-rearm".into())
        .spawn(move || loop {
            std::thread::sleep(REARM_POLL_INTERVAL);
            let state = app.state::<AppState>();
            let active = state.hotkey_monitor_active.load(Ordering::Relaxed);
            let armed = flow_core::hotkey::rearm_tick(
                active,
                permissions::input_monitoring_access,
                || {
                    // Arm with the *current* configured keycode: a Settings
                    // change made while unarmed only persisted to config (there
                    // was no live handle to update), so read it fresh rather
                    // than reuse the stale launch-time value.
                    let keycode = state.config.lock_or_recover().hotkey_keycode;
                    match flow_core::hotkey::spawn_monitor(
                        keycode,
                        is_recording.clone(),
                        tx.clone(),
                    ) {
                        Ok(handle) => {
                            // Publish the handle first so a concurrent
                            // `set_config` keycode change lands on it, then
                            // reconcile against config once more to close the
                            // read-keycode / publish-handle race (if it changed
                            // during the brief spawn, apply the latest).
                            *state.hotkey_keycode_handle.lock_or_recover() = Some(handle.clone());
                            handle.store(
                                state.config.lock_or_recover().hotkey_keycode,
                                Ordering::Relaxed,
                            );
                            state.hotkey_monitor_active.store(true, Ordering::Relaxed);
                            // Settings polls `get_permission_status`, so its dot
                            // flips green on its own; refresh the tray too.
                            crate::tray::refresh_menu(&app);
                            eprintln!(
                                "[vzt-flow] Input Monitoring granted — hotkey tap armed without a restart"
                            );
                            true
                        }
                        Err(()) => false,
                    }
                },
            );
            if armed {
                // Tap is up and self-sustaining (its own in-callback re-enable
                // + 5s watchdog). Nothing left to poll for; end the driver so a
                // second `spawn_monitor` can never happen.
                return;
            }
        })
        .expect("failed to spawn hotkey re-arm driver thread");
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
    // Mirrors "the transcriber is loading" for the rolling workers' watchdog
    // (a cold load is not a wedge — see `RollingConfig::model_loading`). Set
    // here, on the forwarder, so it is current even while the coordinator
    // loop is busy.
    let model_loading = Arc::new(AtomicBool::new(false));
    {
        let tx = unified_tx.clone();
        let loading = model_loading.clone();
        std::thread::spawn(move || {
            while let Ok(status) = model_status_rx.recv() {
                loading.store(matches!(status, ModelStatusEvent::Loading), Ordering::Relaxed);
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
    *app.state::<AppState>().cleanup_cmd_tx.lock_or_recover() = Some(cleanup_cmd_tx);

    // --- hotkey monitor ---
    // Keep a sender clone (`hotkey_tx`) past the install so the macOS re-arm
    // driver can feed a freshly-armed tap; on a first-try success (or non-
    // macOS) the clone is just dropped.
    let (hotkey_tx, hotkey_rx) = mpsc::channel::<HotkeyEvent>();
    let hotkey_active = spawn_hotkey_monitor(
        &app,
        config.hotkey_keycode,
        is_recording.clone(),
        hotkey_tx.clone(),
    );
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

    *app.state::<AppState>().audio_cmd_tx.lock_or_recover() = Some(audio_cmd_tx.clone());
    *app.state::<AppState>().model_cmd_tx.lock_or_recover() = Some(model_cmd_tx.clone());
    app.state::<AppState>()
        .hotkey_monitor_active
        .store(hotkey_active, Ordering::Relaxed);

    // macOS late-grant recovery: if the tap didn't come up (Input Monitoring
    // ungranted at launch, the fresh-install case), poll for the grant and arm
    // the tap the instant it lands — no restart. See `spawn_hotkey_rearm_driver`.
    #[cfg(target_os = "macos")]
    {
        if hotkey_active {
            drop(hotkey_tx);
        } else {
            spawn_hotkey_rearm_driver(app.clone(), is_recording.clone(), hotkey_tx);
        }
    }
    #[cfg(not(target_os = "macos"))]
    drop(hotkey_tx);

    // --- coordinator thread ---
    //
    // Supervised, because a panic in here is invisible and permanent. Rust's
    // default panic strategy unwinds and kills only the panicking THREAD, so
    // without this the process stays alive — tray icon, Settings window, all
    // of it — while the hotkey silently stops doing anything. No crash report,
    // no dialog, nothing to diagnose from. A user reports "the speak button
    // isn't working" and every visible sign says the app is fine.
    //
    // So: catch the unwind, put the state machine back to Idle
    // (`reset_after_panic` — a restart that left `dictation_state` at
    // Recording would still ignore every press), tell the user, and re-enter
    // the loop. The receiver is borrowed rather than moved so the restarted
    // loop keeps draining the SAME channel; queued messages survive.
    //
    // This cannot catch a process ABORT (e.g. the HIToolbox main-thread
    // assert that killed the app on 2026-07-29 — see `flow_core::insert`).
    // Nothing in-process can. That class has to be fixed by not making the
    // call; this handles the Rust-panic class only.
    {
        let app = app.clone();
        std::thread::spawn(move || loop {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_coordinator(
                    app.clone(),
                    &unified_rx,
                    audio_cmd_tx.clone(),
                    model_cmd_tx.clone(),
                    model_loading.clone(),
                );
            }));
            match outcome {
                // Clean return: the channel closed, i.e. the app is shutting
                // down. Restarting here would spin forever on a dead channel.
                Ok(()) => break,
                Err(_) => {
                    eprintln!(
                        "[vzt-flow] coordinator thread panicked; resetting the recorder and \
                         restarting it. The hotkey stays live — please retry your dictation. \
                         (Any transcript already produced is on your clipboard.)"
                    );
                    app.state::<AppState>().reset_after_panic();
                    overlay::emit_overlay(
                        &app,
                        OverlayEvent::Message {
                            text: "Dictation failed — recorder reset, try again".to_string(),
                        },
                    );
                    let app2 = app.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_secs(3));
                        overlay::hide_overlay(&app2);
                    });
                }
            }
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

/// How far into a hold a keyDown of some other key still reads as a shortcut
/// chord (Option+e = ´ on the default Right Option binding) rather than a
/// stray key during a long dictation.
const CHORD_WINDOW: Duration = Duration::from_millis(1500);

/// Whether an `OtherKeyDuringHold` `since_press` into the hold should cancel it
/// as an accidental chord.
fn other_key_is_chord(since_press: Option<Duration>) -> bool {
    since_press.is_some_and(|d| d < CHORD_WINDOW)
}

/// Where a rolling `Final` goes.
#[derive(Debug, PartialEq, Eq)]
enum FinalRoute {
    /// Normal path: dictionary → cleanup → paste.
    Pipeline,
    /// No dictation is waiting for it any more: clipboard + notification.
    ClipboardOnly,
    /// Nothing worth keeping.
    Ignore,
}

/// Routes a rolling `Final` tagged `msg_epoch`, given the epoch of the
/// dictation currently waiting for one (if any).
fn route_rolling_final(pending_epoch: Option<u64>, msg_epoch: u64, has_text: bool) -> FinalRoute {
    if pending_epoch == Some(msg_epoch) {
        FinalRoute::Pipeline
    } else if has_text {
        // Nobody is waiting for these words any more (a newer recording
        // started, or the dictation was already resolved). They used to be
        // dropped without a trace; they are the user's words, so keep them.
        FinalRoute::ClipboardOnly
    } else {
        FinalRoute::Ignore
    }
}

/// What the transcription stage learned about a dictation, carried through
/// cleanup to [`finalize_dictation`] for the overlay message and log line.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct DictationMeta {
    chunks: usize,
    failed_chunks: usize,
    partial: bool,
    stats: AudioStats,
    audio_saved: bool,
}

/// Whether a finished dictation's audio should be written to the recovery
/// slot: anything short of a complete transcript of audible speech. There is
/// one slot, so nothing that cannot hold words may overwrite it: never a
/// take the mic delivered as silence, and an empty result only with real
/// speech energy (so an accidental tap doesn't qualify either).
fn should_save_recovery(transcript_empty: bool, meta: &DictationMeta) -> bool {
    !meta.stats.mic_silent()
        && (meta.partial || meta.failed_chunks > 0 || (transcript_empty && meta.stats.has_speech()))
}

/// For an empty final transcript: the overlay line to show *instead of*
/// pasting. `None` means "not empty, paste normally".
fn empty_result_message(final_text: &str, meta: &DictationMeta) -> Option<String> {
    if !final_text.trim().is_empty() {
        return None;
    }
    Some(if meta.stats.mic_silent() {
        "Mic was silent — check your input".to_string()
    } else if meta.partial {
        // The watchdog gave up before any chunk finished (e.g. a cold model
        // load on a loaded machine): the words may still arrive as `Late`.
        if meta.audio_saved {
            "Transcription slow — audio saved".to_string()
        } else {
            "Transcription slow".to_string()
        }
    } else if meta.audio_saved {
        "No speech recognized — audio saved".to_string()
    } else {
        "No speech recognized".to_string()
    })
}

/// The overlay line after a paste: paste problems first (they say where the
/// text is), then incompleteness, else `None` for the plain Done check.
fn overlay_message(meta: &DictationMeta, paste_message: Option<String>) -> Option<String> {
    if paste_message.is_some() {
        paste_message
    } else if meta.partial {
        Some(if meta.audio_saved {
            "Partial transcript — audio saved".to_string()
        } else {
            "Partial transcript".to_string()
        })
    } else if meta.failed_chunks > 0 {
        Some("Some audio couldn't be transcribed".to_string())
    } else {
        None
    }
}

/// Drains the coordinator channel until it closes.
///
/// `rx` is borrowed, not owned, so the supervisor in [`spawn`] can re-enter
/// this function after a panic and keep serving the same channel. Returning
/// normally means the channel closed (shutdown); panicking means the
/// supervisor restarts us with the local state below rebuilt from scratch,
/// which is the correct recovery — none of it is worth preserving across a
/// panic.
fn run_coordinator(
    app: AppHandle,
    rx: &mpsc::Receiver<CoordinatorMsg>,
    audio_cmd_tx: Sender<AudioCommand>,
    model_cmd_tx: Sender<ModelCommand>,
    model_loading: Arc<AtomicBool>,
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
    // When the current hold began (for the stray-key chord window, B5), and
    // whether a stray key during it was already logged.
    let mut press_at: Option<Instant> = None;
    let mut stray_logged = false;
    // Why the in-flight cancel was requested, for its outcome log line.
    let mut cancel_reason: Option<&'static str> = None;
    while let Ok(msg) = rx.recv() {
        let state = app.state::<AppState>();
        match msg {
            CoordinatorMsg::Hotkey(HotkeyEvent::HoldKeyPressed) => {
                press_at = Some(Instant::now());
                stray_logged = false;
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
                if *state.dictation_state.lock_or_recover() != DictationState::Idle {
                    continue; // mid-transcription/paste; ignore new presses
                }

                let app2 = app.clone();
                let key_down = hold.key_down.clone();
                let generation = hold.generation.clone();
                let consumed = hold.consumed.clone();
                let (threshold, max_hold_secs) = {
                    let st = app2.state::<AppState>();
                    let cfg = st.config.lock_or_recover();
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
                        if *state.dictation_state.lock_or_recover() == DictationState::Idle {
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
                let current = *state.dictation_state.lock_or_recover();

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
                //
                // B5: only inside the chord window. Past it the user is
                // plainly dictating, and a brushed key used to throw away
                // the whole take (minutes of speech) without a word.
                let since_press = press_at.map(|t| t.elapsed());
                if hold.key_down.load(Ordering::Relaxed) && other_key_is_chord(since_press) {
                    hold.other_key.store(true, Ordering::Relaxed);
                    // Mark consumed so the delayed hold-check won't start a
                    // recording and the release won't arm hands-free.
                    hold.consumed.store(true, Ordering::Relaxed);
                    if *state.dictation_state.lock_or_recover() == DictationState::Recording {
                        // A false start already began (hold outlived the
                        // threshold before the special char was typed):
                        // discard it — the user is typing, not dictating.
                        state.hands_free_active.store(false, Ordering::Relaxed);
                        cancel_reason = Some("chord");
                        let _ = audio_cmd_tx.send(AudioCommand::Cancel);
                    }
                } else if hold.key_down.load(Ordering::Relaxed) && !stray_logged {
                    stray_logged = true;
                    eprintln!(
                        "[vzt-flow] a key was pressed {:.1}s into the hold — past the {:.1}s \
                         chord window, so the dictation continues",
                        since_press.unwrap_or_default().as_secs_f64(),
                        CHORD_WINDOW.as_secs_f64()
                    );
                }
            }
            CoordinatorMsg::Hotkey(HotkeyEvent::CancelRequested) => {
                if *state.dictation_state.lock_or_recover() == DictationState::Recording {
                    // The recording is being thrown away; mark the in-flight
                    // press consumed so its release doesn't arm hands-free (F4).
                    hold.consumed.store(true, Ordering::Relaxed);
                    state.hands_free_active.store(false, Ordering::Relaxed);
                    cancel_reason = Some("escape-or-flow-cancel");
                    let _ = audio_cmd_tx.send(AudioCommand::Cancel);
                }
            }
            CoordinatorMsg::TrayToggleDictation => {
                let current = *state.dictation_state.lock_or_recover();
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
                let rolling_enabled = state.config.lock_or_recover().rolling_transcription;
                if rolling_enabled {
                    let epoch = rolling_epoch;
                    let (out_tx, out_rx) = mpsc::channel::<RollingOutput>();
                    if let Some(coord_tx) = state.coordinator_tx.lock_or_recover().clone() {
                        // Forward the worker's output onto the coordinator
                        // channel, epoch-tagged. Exits when the worker drops
                        // `out_tx` (recording finalized or abandoned); if that
                        // happens before any `Final`, say so, so a dictation
                        // waiting in Transcribing is never stranded.
                        std::thread::spawn(move || {
                            let mut saw_final = false;
                            while let Ok(output) = out_rx.recv() {
                                saw_final |= matches!(output, RollingOutput::Final { .. });
                                if coord_tx
                                    .send(CoordinatorMsg::Rolling { epoch, output })
                                    .is_err()
                                {
                                    return;
                                }
                            }
                            if !saw_final {
                                let _ = coord_tx.send(CoordinatorMsg::RollingWorkerLost { epoch });
                            }
                        });
                        let cfg = rolling::RollingConfig {
                            model_loading: Some(model_loading.clone()),
                            ..rolling::RollingConfig::from_env()
                        };
                        rolling_in = Some(rolling::spawn_rolling_worker_with(
                            cfg,
                            model_cmd_tx.clone(),
                            out_tx,
                        ));
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
                    .lock_or_recover()
                    .map(|s| s.elapsed())
                    .unwrap_or_default();
                let max_secs = state.recording_max_secs.lock_or_recover().unwrap_or(0);
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
                let listen_pending = state.pending_listen.lock_or_recover().take();

                // Frontmost app + resolved profile, captured now (right as
                // recording ends) rather than after ASR completes — that's
                // both a more accurate "at paste time" reading and lets the
                // overlay show the mode badge immediately.
                let app_bundle_id = permissions::frontmost_bundle_id();
                let mut profile = state.profiles.lock_or_recover().resolve(app_bundle_id.as_deref());
                if let Some((_, Some(mode_override))) = &listen_pending {
                    profile.mode = mode_override.clone();
                }
                let listen_reply = listen_pending.map(|(tx, _)| tx);
                overlay::emit_overlay(&app, OverlayEvent::Transcribing { mode: profile.mode.clone() });

                // Rolling path (Feature B): the worker already holds the audio
                // and has transcribed everything but the tail. Tell it to
                // finalize; the assembled transcript comes back as
                // `RollingOutput::Final`. There is deliberately no timer here
                // any more: the old `duration + 60s` one fired while a healthy
                // but loaded engine was still working (measured: RTF 2.3 under
                // parallel builds → a 69s take needed 163s after release) and
                // threw the take away. The worker bounds its own wait by
                // progress and always answers; `RollingWorkerLost` covers the
                // worker vanishing. `samples` is empty in rolling mode.
                if let Some(rin) = rolling_in.take() {
                    let _ = rin.send(RollingInput::Finish);
                    pending_rolling = Some(PendingRollingFinal {
                        app_bundle_id,
                        profile,
                        listen_reply,
                        epoch: rolling_epoch,
                    });
                    continue;
                }

                let stats = AudioStats::from_samples(&samples, flow_core::audio::TARGET_SAMPLE_RATE);
                let kept = samples.clone();
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
                let forward_tx = state.coordinator_tx.lock_or_recover().clone();
                std::thread::spawn(move || {
                    // Never wait forever on the transcriber. A dropped reply
                    // channel (panicked worker) surfaces as RecvError; a wedged
                    // inference trips the timeout. Either way synthesize an
                    // error result so the state machine leaves Transcribing and
                    // the overlay is dismissed instead of hanging (F2) — and
                    // the audio travels with it, to be saved for recovery.
                    let transcribe_timeout = batch_transcribe_timeout(duration);
                    let result = match reply_rx.recv_timeout(transcribe_timeout) {
                        Ok(result) => result,
                        Err(_) => Err(format!(
                            "transcription timed out after {:.0}s",
                            transcribe_timeout.as_secs_f64()
                        )),
                    };
                    if let Some(tx) = forward_tx {
                        let _ = tx.send(CoordinatorMsg::TranscribeResult {
                            result,
                            audio_duration: duration,
                            app_bundle_id,
                            profile,
                            listen_reply,
                            samples: kept,
                            stats,
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
                        if let Some(tx) = state.coordinator_tx.lock_or_recover().clone() {
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
                        if let Some((tx, _)) = state.pending_listen.lock_or_recover().take() {
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
                    if let Some((tx, _)) = state.pending_listen.lock_or_recover().take() {
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
                    if let Some(tx) = state.coordinator_tx.lock_or_recover().clone() {
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
                if let Some((tx, _)) = state.pending_listen.lock_or_recover().take() {
                    let _ = tx.send(Err("recording cancelled".to_string()));
                }
                log_outcome(
                    "cancelled",
                    Some(cancel_reason.take().unwrap_or("cancel")),
                    recording_elapsed(&state),
                    &DictationMeta::default(),
                    0,
                );
                state.set_dictation_state(DictationState::Idle);
                overlay::hide_overlay(&app);
            }
            CoordinatorMsg::Audio(AudioReply::NotRecording) => {
                // The worker was idle when a stop/cancel reached it, so this
                // side and the worker disagree about whether a recording is
                // live. Reconcile toward the worker — it is the one that
                // actually owns the microphone.
                //
                // Guarded on `Recording` deliberately. A stop/cancel can be
                // acknowledged *after* a recording has already ended normally,
                // by which point we are Transcribing and a real transcript is
                // in flight; clearing state there would discard the user's
                // words to fix a problem that no longer exists. Idle needs no
                // action either. So this only ever rescues the stuck case.
                if *state.dictation_state.lock_or_recover() == DictationState::Recording {
                    eprintln!(
                        "[vzt-flow] stop/cancel arrived with no recording in progress — \
                         state was latched on Recording, resetting to Idle"
                    );
                    rolling_in = None;
                    rolling_epoch = rolling_epoch.wrapping_add(1);
                    rolling_preview.clear();
                    pending_rolling = None;
                    state.hands_free_active.store(false, Ordering::Relaxed);
                    if let Some((tx, _)) = state.pending_listen.lock_or_recover().take() {
                        let _ = tx.send(Err("recording cancelled".to_string()));
                    }
                    state.set_dictation_state(DictationState::Idle);
                    tray::refresh_menu(&app);
                    overlay::hide_overlay(&app);
                }
            }
            CoordinatorMsg::Audio(AudioReply::Error(e)) => {
                eprintln!("[vzt-flow] audio error: {e}");
                log_outcome("failed", Some("audio-error"), recording_elapsed(&state), &DictationMeta::default(), 0);
                rolling_in = None;
                rolling_epoch = rolling_epoch.wrapping_add(1);
                rolling_preview.clear();
                pending_rolling = None;
                if let Some((tx, _)) = state.pending_listen.lock_or_recover().take() {
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
                match output {
                    RollingOutput::Preview { chunk_text } => {
                        // Live preview (Feature B): only while still recording,
                        // dictionary-corrected (no LLM) for display. Appended to
                        // the running raw tail; the overlay shows its last chars.
                        // Previews from an abandoned or older recording are
                        // dropped — they carry nothing that isn't in its Final.
                        if epoch == rolling_epoch
                            && *state.dictation_state.lock_or_recover() == DictationState::Recording
                        {
                            let corrected = {
                                let dict = state.dictionary.lock_or_recover().clone();
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
                    RollingOutput::Final { raw_text, audio_duration, chunks, failed_chunks, partial, stats, audio } => {
                        let pending_epoch = pending_rolling.as_ref().map(|p| p.epoch);
                        let mut meta = DictationMeta { chunks, failed_chunks, partial, stats, audio_saved: false };
                        if should_save_recovery(raw_text.trim().is_empty(), &meta) {
                            meta.audio_saved = save_recovery_audio(&audio);
                        }
                        drop(audio);
                        match route_rolling_final(pending_epoch, epoch, !raw_text.trim().is_empty()) {
                            FinalRoute::Pipeline => {
                                rolling_preview.clear();
                                if let Some(p) = pending_rolling.take() {
                                    run_pipeline(
                                        &app,
                                        raw_text,
                                        audio_duration,
                                        p.app_bundle_id,
                                        p.profile,
                                        p.listen_reply,
                                        meta,
                                    );
                                }
                                tray::refresh_menu(&app);
                            }
                            FinalRoute::ClipboardOnly => {
                                eprintln!(
                                    "[vzt-flow] a transcript arrived for a dictation that was no longer \
                                     waiting (epoch {epoch}); putting it on the clipboard instead of \
                                     dropping it"
                                );
                                deliver_to_clipboard(
                                    &app,
                                    &raw_text,
                                    "An earlier dictation finished late — its transcript is on your clipboard.",
                                    "late-final",
                                    audio_duration,
                                    &meta,
                                );
                            }
                            FinalRoute::Ignore => {}
                        }
                    }
                    RollingOutput::Late { raw_text, failed_chunks } => {
                        // The chunks the watchdog abandoned finished after all.
                        // The partial text was already pasted and the user has
                        // moved on, so the complete transcript goes to the
                        // clipboard rather than being pasted a second time.
                        if !raw_text.trim().is_empty() {
                            let meta = DictationMeta { failed_chunks, ..Default::default() };
                            deliver_to_clipboard(
                                &app,
                                &raw_text,
                                "Your last dictation finished transcribing — the complete transcript is on your clipboard.",
                                "late-complete",
                                Duration::ZERO,
                                &meta,
                            );
                        }
                    }
                }
            }
            CoordinatorMsg::RollingWorkerLost { epoch } => {
                // The worker died without answering. Only matters if the
                // dictation is still waiting on it.
                if pending_rolling.as_ref().map(|p| p.epoch) == Some(epoch) {
                    let p = pending_rolling.take().expect("checked above");
                    eprintln!(
                        "[vzt-flow] the rolling transcription worker exited without a result; \
                         leaving Transcribing"
                    );
                    log_outcome("failed", Some("worker-lost"), recording_elapsed(&state), &DictationMeta::default(), 0);
                    rolling_preview.clear();
                    if let Some(tx) = p.listen_reply {
                        let _ = tx.send(Err("transcription failed".to_string()));
                    }
                    state.set_dictation_state(DictationState::Idle);
                    tray::refresh_menu(&app);
                    show_message_then_hide(&app, "Transcription failed".to_string(), 2500);
                }
            }
            CoordinatorMsg::RecoverLastRecording => {
                spawn_recovery(&app, model_cmd_tx.clone());
            }
            CoordinatorMsg::TranscribeResult { result, audio_duration, app_bundle_id, profile, listen_reply, samples, stats } => {
                match result {
                    Ok(transcript) => {
                        let mut meta = DictationMeta { chunks: 1, stats, ..Default::default() };
                        if should_save_recovery(transcript.text.trim().is_empty(), &meta) {
                            meta.audio_saved = save_recovery_audio(&samples);
                        }
                        drop(samples);
                        run_pipeline(&app, transcript.text, audio_duration, app_bundle_id, profile, listen_reply, meta);
                    }
                    Err(e) => {
                        eprintln!("[vzt-flow] transcription error: {e}");
                        // The whole take failed: keep it unless the mic heard
                        // nothing at all.
                        let saved = !stats.mic_silent() && save_recovery_audio(&samples);
                        let meta = DictationMeta { chunks: 1, failed_chunks: 1, stats, audio_saved: saved, ..Default::default() };
                        log_outcome("failed", Some("transcription-error"), audio_duration, &meta, 0);
                        // Map the real engine error to user-facing text before
                        // moving `e` into the listen reply — a missing-model
                        // failure becomes an actionable "download it" prompt
                        // instead of the old hard-coded "Transcription failed".
                        let mut text = transcription_error_message(&e);
                        if saved {
                            text = "Transcription failed — audio saved".to_string();
                            notify_audio_saved(&app);
                        }
                        if let Some(tx) = listen_reply {
                            let _ = tx.send(Err(e));
                        }
                        state.set_dictation_state(DictationState::Idle);
                        // Surface the failure instead of silently vanishing,
                        // then dismiss (F2).
                        show_message_then_hide(&app, text, 2500);
                    }
                }
                tray::refresh_menu(&app);
            }
            CoordinatorMsg::CleanupDone { raw_text, result, mode_label, audio_duration, app_bundle_id, listen_reply, meta } => {
                finalize_dictation(
                    &app,
                    &raw_text,
                    &result.text,
                    &mode_label,
                    audio_duration,
                    app_bundle_id,
                    listen_reply,
                    meta,
                );
            }
            CoordinatorMsg::Model(ModelStatusEvent::Loading) => {
                *state.model_lifecycle.lock_or_recover() = ModelLifecycle::Loading;
                tray::refresh_menu(&app);
            }
            CoordinatorMsg::Model(ModelStatusEvent::Loaded { load_time }) => {
                *state.model_lifecycle.lock_or_recover() = ModelLifecycle::Loaded;
                eprintln!("[vzt-flow] model loaded in {:.2}s", load_time.as_secs_f64());
                tray::refresh_menu(&app);
            }
            CoordinatorMsg::Model(ModelStatusEvent::LoadFailed(e)) => {
                eprintln!("[vzt-flow] model load failed: {e}");
                *state.model_lifecycle.lock_or_recover() = ModelLifecycle::Unloaded;
                // Give the failure a user-visible surface too — previously it
                // was only `eprintln!`'d, so a load failure was invisible. Skip
                // it while Recording so we don't cover the live level bars; the
                // transcribe reply path (`TranscribeResult::Err`) handles the
                // Transcribing case with the same mapped message.
                if *state.dictation_state.lock_or_recover() != DictationState::Recording {
                    let text = transcription_error_message(&e);
                    overlay::show_overlay(&app);
                    overlay::emit_overlay(&app, OverlayEvent::Message { text });
                    let app2 = app.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_millis(2500));
                        overlay::hide_overlay(&app2);
                    });
                }
                tray::refresh_menu(&app);
            }
            CoordinatorMsg::Model(ModelStatusEvent::Unloaded) => {
                *state.model_lifecycle.lock_or_recover() = ModelLifecycle::Unloaded;
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
                *state.cleanup_lifecycle.lock_or_recover() = lifecycle;
            }
            CoordinatorMsg::TestOverlay => {
                run_overlay_self_test(&app);
            }
            CoordinatorMsg::DaemonListen { mode, max_secs, reply } => {
                if *state.dictation_state.lock_or_recover() != DictationState::Idle {
                    let _ = reply.send(Err("already recording or transcribing".to_string()));
                    continue;
                }
                let cap = max_secs.unwrap_or_else(|| max_handsfree_secs(&app));
                *state.pending_listen.lock_or_recover() = Some((reply, mode));
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
        .lock_or_recover()
        .max_handsfree_secs
}

/// Maps a transcription / model-load error string to the short line shown on
/// the overlay. Pure (no I/O) so it's unit-testable. Recognizes the engine's
/// "model directory … does not exist" marker (`engine.rs`'s `ParakeetModel::
/// load` bail) and turns it into an actionable "download the model" prompt;
/// everything else collapses to a short generic so we never surface a raw
/// internal error string to the user.
fn transcription_error_message(err: &str) -> String {
    let lower = err.to_ascii_lowercase();
    if lower.contains("model directory") && lower.contains("does not exist") {
        "Speech model not installed — open VZT Flow Settings to download it".to_string()
    } else {
        "Transcription failed".to_string()
    }
}

/// Whether the Parakeet model is installed, using the hot-path cache in
/// `AppState.model_download` and only falling back to a filesystem check when
/// the cache says "absent". The cache only ever flips absent→present (a model
/// directory doesn't vanish under a running app), so a cached `true` is always
/// safe; a cached `false` triggers one cheap `check_parakeet_model()` (a
/// directory stat — sub-millisecond on APFS, negligible against the 300ms hold
/// threshold) which also refreshes the cache once a model appears.
fn parakeet_installed(app: &AppHandle) -> bool {
    let state = app.state::<AppState>();
    if state.model_download.parakeet_present.load(Ordering::Relaxed) {
        return true;
    }
    let present = flow_core::models::check_parakeet_model()
        .map(|s| s.present)
        .unwrap_or(false);
    if present {
        state.model_download.parakeet_present.store(true, Ordering::Relaxed);
    }
    present
}

/// Builds the overlay line shown when the hotkey is pressed but no speech model
/// is installed: a live download percentage if one is running (the moment the
/// user is most likely to be confused about why nothing happens), otherwise a
/// prompt to open Settings and download it.
fn model_missing_overlay_text(app: &AppHandle) -> String {
    let dl = &app.state::<AppState>().model_download;
    if dl.active_kind.lock_or_recover().is_some() {
        let done = dl.downloaded.load(Ordering::Relaxed);
        let total = dl.total.load(Ordering::Relaxed);
        if total > 0 {
            let pct = ((done as f64 / total as f64) * 100.0).round() as u64;
            format!("Downloading speech model… {pct}%")
        } else {
            "Downloading speech model…".to_string()
        }
    } else {
        "Speech model not installed — open VZT Flow Settings to download it".to_string()
    }
}

fn start_recording(app: &AppHandle, max_secs: u64) {
    let state = app.state::<AppState>();

    // Gate: never record without a speech model. Before this, `start_recording`
    // captured audio unconditionally and the missing-model failure only
    // surfaced ~30s later when the lazy model load failed at release — the user
    // talked into a void. Now we refuse up front, tell them why, and reset any
    // mode/listen bookkeeping the caller set so we don't get stuck believing a
    // hands-free session or daemon `listen` is live.
    if !parakeet_installed(app) {
        state.hands_free_active.store(false, Ordering::Relaxed);
        if let Some((tx, _)) = state.pending_listen.lock_or_recover().take() {
            let _ = tx.send(Err("speech model not installed".to_string()));
        }
        let text = model_missing_overlay_text(app);
        overlay::show_overlay(app);
        overlay::emit_overlay(app, OverlayEvent::Message { text });
        // Linger a little longer than the usual transient (900ms) so there's
        // time to read the download hint.
        let app2 = app.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(2500));
            overlay::hide_overlay(&app2);
        });
        return;
    }

    state.set_dictation_state(DictationState::Recording);
    *state.recording_started.lock_or_recover() = Some(std::time::Instant::now());
    *state.recording_max_secs.lock_or_recover() = Some(max_secs);
    tray::refresh_menu(app);
    overlay::show_overlay(app);
    overlay::emit_overlay(app, overlay::recording_event(0.0, Duration::ZERO, max_secs));
    // Energy-based auto-stop only applies to hands-free recordings — a
    // hold-to-talk recording's only stop signal is releasing the key.
    let (handsfree_silence_secs, rolling) = {
        let cfg = state.config.lock_or_recover();
        let hf = state
            .hands_free_active
            .load(Ordering::Relaxed)
            .then_some(cfg.handsfree_silence_secs);
        (hf, cfg.rolling_transcription)
    };
    let tx = state.audio_cmd_tx.lock_or_recover().clone();
    if let Some(tx) = tx {
        // `rolling` must match what the `Started` handler reads from config to
        // decide whether to create a rolling worker — both read the same flag.
        let _ = tx.send(AudioCommand::Start { max_secs, handsfree_silence_secs, rolling });
    }

    // Kick off cleanup-model load + Metal warm-up now, in parallel with the
    // user speaking, so it's already warm by the time transcription
    // finishes and the real (deadline-bound) cleanup call runs. The manager
    // no-ops if it's already loaded/warmed.
    let cleanup_tx = state.cleanup_cmd_tx.lock_or_recover().clone();
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
    meta: DictationMeta,
) {
    let state = app.state::<AppState>();
    let dict = state.dictionary.lock_or_recover().clone();
    let corrected = dictionary::correct(&raw_text, &dict);

    // Nothing was recognized: skip code mode / the LLM entirely and let
    // `finalize_dictation` say so instead of pasting an empty string (B4).
    if corrected.trim().is_empty() {
        finalize_dictation(app, &raw_text, "", &profile.mode, audio_duration, app_bundle_id, listen_reply, meta);
        return;
    }

    if profile.mode == "code" {
        let final_text = codemode::transform(&corrected);
        finalize_dictation(app, &raw_text, &final_text, "code", audio_duration, app_bundle_id, listen_reply, meta);
        return;
    }

    let mode = Mode::parse(&profile.mode);
    let mode_label = mode.label().to_string();

    if mode == Mode::Raw {
        // No LLM involved at all in raw mode; finish synchronously.
        finalize_dictation(app, &raw_text, &corrected, &mode_label, audio_duration, app_bundle_id, listen_reply, meta);
        return;
    }

    let timeout_ms = {
        let cfg = state.config.lock_or_recover();
        flow_core::cleanup_manager::cleanup_deadline_ms(corrected.chars().count(), &cfg)
    };
    let dictionary_terms: Vec<String> = dict.iter().map(|d| d.term.clone()).collect();
    let ctx = CleanupContext { app_name: app_bundle_id.clone(), tone: profile.tone.clone(), dictionary_terms };
    let cleanup_tx = state.cleanup_cmd_tx.lock_or_recover().clone();

    let Some(cleanup_tx) = cleanup_tx else {
        finalize_dictation(app, &raw_text, &corrected, &mode_label, audio_duration, app_bundle_id, listen_reply, meta);
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
        finalize_dictation(app, &raw_text, &corrected, &mode_label, audio_duration, app_bundle_id, listen_reply, meta);
        return;
    }

    let forward_tx = state.coordinator_tx.lock_or_recover().clone();
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
                meta,
            });
        }
    });
}

/// The overlay line for a paste outcome that needs the user's attention
/// (the transcript is on the clipboard rather than in the field), firing the
/// matching notification. `None` for a clean paste.
fn paste_message(app: &AppHandle, outcome: &anyhow::Result<insert::PasteOutcome>) -> Option<String> {
    match outcome {
        Ok(insert::PasteOutcome::Pasted) => None,
        Ok(insert::PasteOutcome::SkippedSecureField) => Some("Secure field — transcript on clipboard".to_string()),
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
}

#[allow(clippy::too_many_arguments)]
fn finalize_dictation(
    app: &AppHandle,
    raw_text: &str,
    cleaned_text: &str,
    mode_label: &str,
    audio_duration: Duration,
    app_bundle_id: Option<String>,
    listen_reply: Option<Sender<Result<ListenOutcome, String>>>,
    meta: DictationMeta,
) {
    let state = app.state::<AppState>();

    let snips = state.snippets.lock_or_recover().clone();
    let final_text = snippets::expand(cleaned_text, &snips).unwrap_or_else(|| cleaned_text.to_string());

    // B4: an empty result is never a silent "Done". Nothing is pasted (an
    // empty paste used to flash the success check), nothing goes into
    // history, and the overlay says what happened. A daemon `listen` still
    // gets its (empty) reply so the caller isn't left waiting.
    if let Some(text) = empty_result_message(&final_text, &meta) {
        if let Some(tx) = listen_reply {
            let _ = tx.send(Ok(ListenOutcome {
                raw: raw_text.to_string(),
                text: String::new(),
                mode: mode_label.to_string(),
                duration_s: audio_duration.as_secs_f64(),
            }));
        }
        if meta.audio_saved {
            notify_audio_saved(app);
        }
        log_outcome("empty", None, audio_duration, &meta, 0);
        state.set_dictation_state(DictationState::Done);
        tray::refresh_menu(app);
        finish_with(app, Some(text));
        return;
    }

    *state.last_transcript.lock_or_recover() = Some(final_text.clone());

    // A daemon `listen` command never pastes — it hands the text back over
    // the socket instead. Everything else (history logging, overlay
    // Done flash, state reset) is identical to a normal dictation.
    let (outcome, pasted_msg) = if let Some(tx) = listen_reply {
        let _ = tx.send(Ok(ListenOutcome {
            raw: raw_text.to_string(),
            text: final_text.clone(),
            mode: mode_label.to_string(),
            duration_s: audio_duration.as_secs_f64(),
        }));
        ("returned", None)
    } else {
        let outcome = insert::paste_text(&final_text);
        let msg = paste_message(app, &outcome);
        (if msg.is_none() { "pasted" } else { "clipboard" }, msg)
    };
    let outcome = if meta.partial { "partial" } else { outcome };

    let entry = history::HistoryEntry {
        ts: history::now_unix(),
        app: app_bundle_id,
        raw_text: raw_text.to_string(),
        duration_s: audio_duration.as_secs_f64(),
        rtf: 0.0, // logged to stderr by the model manager; not recomputed here
        clean_text: final_text.clone(),
        mode: mode_label.to_string(),
    };
    if let Err(e) = history::append(&entry) {
        eprintln!("[vzt-flow] failed to append history: {e}");
    }

    if meta.partial || meta.failed_chunks > 0 {
        notify_incomplete(app, &meta);
    }
    log_outcome(outcome, None, audio_duration, &meta, final_text.chars().count());

    state.set_dictation_state(DictationState::Done);
    finish_with(app, overlay_message(&meta, pasted_msg));
}

/// Shows `message` (or the Done check), returns to Idle after 900ms as
/// before, and keeps a message on screen long enough to read (2.5s) — unless
/// a new recording has taken the overlay over in the meantime.
fn finish_with(app: &AppHandle, message: Option<String>) {
    let linger = if message.is_some() { Duration::from_millis(2500) } else { Duration::from_millis(900) };
    match message {
        Some(text) => overlay::emit_overlay(app, OverlayEvent::Message { text }),
        None => overlay::emit_overlay(app, OverlayEvent::Done),
    }
    let app2 = app.clone();
    std::thread::spawn(move || {
        let idle_after = Duration::from_millis(900);
        std::thread::sleep(idle_after);
        let state = app2.state::<AppState>();
        {
            let mut ds = state.dictation_state.lock_or_recover();
            if *ds == DictationState::Done {
                *ds = DictationState::Idle;
            }
        }
        state.is_recording.store(false, Ordering::Relaxed);
        // The menu was last built while Done ("Status: Done", "Stop
        // dictation"); rebuild it for Idle, which also enables "Recover last
        // recording" once a recording was saved.
        tray::refresh_menu(&app2);
        std::thread::sleep(linger.saturating_sub(idle_after));
        if *state.dictation_state.lock_or_recover() == DictationState::Idle {
            overlay::hide_overlay(&app2);
        }
    });
}

/// Shows a transient overlay message and hides it after `ms`, unless a new
/// recording has started meanwhile.
fn show_message_then_hide(app: &AppHandle, text: String, ms: u64) {
    overlay::show_overlay(app);
    overlay::emit_overlay(app, OverlayEvent::Message { text });
    let app2 = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(ms));
        if *app2.state::<AppState>().dictation_state.lock_or_recover() == DictationState::Idle {
            overlay::hide_overlay(&app2);
        }
    });
}

/// How long the batch (non-rolling) path waits for a whole-recording
/// transcription. Was `duration + 60s`, which a loaded machine exceeds for
/// any take over ~45s (RTF > 2 measured under parallel builds); now it
/// allows the same 6× real time the rolling watchdog does, plus the one-time
/// model-load allowance. A timeout still keeps the audio (it is saved for
/// recovery), so this bounds waiting, not the user's words.
fn batch_transcribe_timeout(duration: Duration) -> Duration {
    duration.mul_f32(6.0) + Duration::from_secs(120)
}

/// How long the current (or just-ended) recording has been running.
fn recording_elapsed(state: &AppState) -> Duration {
    state
        .recording_started
        .lock_or_recover()
        .map(|s| s.elapsed())
        .unwrap_or_default()
}

/// Writes the recovery recording, logging (not failing) on error. Returns
/// whether it is actually on disk, so no message claims "audio saved" falsely.
fn save_recovery_audio(samples: &[f32]) -> bool {
    if samples.is_empty() {
        return false;
    }
    match flow_core::recovery::save_last_recording(samples) {
        Ok(path) => {
            eprintln!(
                "[vzt-flow] saved the recording ({:.1}s) to {} — tray > Recover last recording",
                samples.len() as f64 / flow_core::audio::TARGET_SAMPLE_RATE as f64,
                path.display()
            );
            true
        }
        Err(e) => {
            eprintln!("[vzt-flow] failed to save the recording for recovery: {e}");
            false
        }
    }
}

/// One line per dictation in the log, whatever happened to it, so a take
/// that produced nothing still leaves a record (history is written on
/// success only).
fn format_outcome_line(
    outcome: &str,
    reason: Option<&str>,
    duration: Duration,
    meta: &DictationMeta,
    chars: usize,
) -> String {
    format!(
        "[vzt-flow] {} dictation outcome={outcome}{} duration={:.1}s chunks={} failed_chunks={} \
         peak={:.3} speech={:.1}s chars={chars} audio_saved={}",
        flow_core::logfile::timestamp(),
        reason.map(|r| format!(" reason={r}")).unwrap_or_default(),
        duration.as_secs_f64(),
        meta.chunks,
        meta.failed_chunks,
        meta.stats.peak,
        meta.stats.speech_secs,
        meta.audio_saved,
    )
}

fn log_outcome(outcome: &str, reason: Option<&str>, duration: Duration, meta: &DictationMeta, chars: usize) {
    eprintln!("{}", format_outcome_line(outcome, reason, duration, meta, chars));
}

/// Puts a transcript that no dictation is waiting for any more on the
/// clipboard (dictionary-corrected, no LLM) and says so with a notification.
fn deliver_to_clipboard(
    app: &AppHandle,
    raw_text: &str,
    body: &str,
    reason: &str,
    duration: Duration,
    meta: &DictationMeta,
) {
    let dict = app.state::<AppState>().dictionary.lock_or_recover().clone();
    let text = dictionary::correct(raw_text, &dict);
    match arboard::Clipboard::new().and_then(|mut c| c.set_text(text.clone())) {
        Ok(()) => {
            log_outcome("clipboard", Some(reason), duration, meta, text.chars().count());
            notify(app, body);
        }
        Err(e) => eprintln!("[vzt-flow] could not put the {reason} transcript on the clipboard: {e}"),
    }
}

/// Best-effort desktop notification (failures are logged, never fatal).
fn notify(app: &AppHandle, body: &str) {
    if let Err(e) = app.notification().builder().title("VZT Flow").body(body).show() {
        eprintln!("[vzt-flow] notification failed: {e}");
    }
}

fn notify_audio_saved(app: &AppHandle) {
    notify(
        app,
        "The recording was saved. Use \"Recover last recording\" in the menu bar to transcribe it again.",
    );
}

fn notify_incomplete(app: &AppHandle, meta: &DictationMeta) {
    let lead = if meta.partial {
        "Transcription was taking too long, so the part that finished was pasted; the rest will be put on your clipboard if it completes."
    } else {
        "Part of the dictation couldn't be transcribed."
    };
    let tail = if meta.audio_saved {
        " The recording is saved — use \"Recover last recording\" in the menu bar."
    } else {
        ""
    };
    notify(app, &format!("{lead}{tail}"));
}

/// Guards against two recoveries running at once.
static RECOVERY_RUNNING: AtomicBool = AtomicBool::new(false);

/// Tray "Recover last recording": re-transcribes the saved recording off the
/// main thread, through the model manager in ≤35s chunks (never one >35s
/// engine call — gotcha b), and puts the text on the clipboard.
fn spawn_recovery(app: &AppHandle, model_cmd_tx: Sender<ModelCommand>) {
    if RECOVERY_RUNNING.swap(true, Ordering::SeqCst) {
        notify(app, "Already recovering the last recording.");
        return;
    }
    let app = app.clone();
    std::thread::Builder::new()
        .name("vzt-flow-recovery".into())
        .spawn(move || {
            let result = (|| -> anyhow::Result<flow_core::recovery::Recovered> {
                let path = flow_core::recovery::last_recording_path()?;
                anyhow::ensure!(path.is_file(), "no saved recording at {}", path.display());
                let (samples, _) = flow_core::audio::load_audio_file_as_f32(&path)?;
                eprintln!(
                    "[vzt-flow] recovering {} ({:.1}s)",
                    path.display(),
                    samples.len() as f64 / flow_core::audio::TARGET_SAMPLE_RATE as f64
                );
                flow_core::recovery::transcribe_paced(&model_cmd_tx, &samples)
            })();
            let idle = *app.state::<AppState>().dictation_state.lock_or_recover() == DictationState::Idle;
            match result {
                Ok(r) if !r.text.trim().is_empty() => {
                    let state = app.state::<AppState>();
                    let dict = state.dictionary.lock_or_recover().clone();
                    let text = dictionary::correct(&r.text, &dict);
                    let copied = arboard::Clipboard::new().and_then(|mut c| c.set_text(text.clone())).is_ok();
                    *state.last_transcript.lock_or_recover() = Some(text.clone());
                    let meta = DictationMeta { chunks: r.chunks, failed_chunks: r.failed_chunks, ..Default::default() };
                    log_outcome("recovered", None, r.duration, &meta, text.chars().count());
                    let words = text.split_whitespace().count();
                    let body = if copied {
                        format!("Recovered {words} words from the last recording — they're on your clipboard.")
                    } else {
                        format!("Recovered {words} words — use \"Copy last transcript\" to copy them.")
                    };
                    notify(&app, &body);
                    if idle {
                        show_message_then_hide(&app, "Recovered — transcript on clipboard".to_string(), 2500);
                    }
                }
                Ok(r) => {
                    let meta = DictationMeta { chunks: r.chunks, failed_chunks: r.failed_chunks, ..Default::default() };
                    log_outcome("recovered", Some("no-speech"), r.duration, &meta, 0);
                    notify(&app, "No speech could be recognized in the last recording.");
                }
                Err(e) => {
                    eprintln!("[vzt-flow] recovery failed: {e}");
                    notify(&app, &format!("Couldn't recover the last recording: {e}"));
                }
            }
            RECOVERY_RUNNING.store(false, Ordering::SeqCst);
        })
        .expect("failed to spawn recovery thread");
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

    // ---- coordinator supervision (panic recovery) ----

    /// Pins the structural decision that makes the supervisor in [`spawn`]
    /// work: `run_coordinator` BORROWS the receiver instead of owning it.
    ///
    /// If it took `rx` by value, the receiver would be moved into the first
    /// `catch_unwind` closure and dropped when that call unwound — the restart
    /// would have no channel to serve, the hotkey would be dead anyway, and
    /// every message already queued behind the panicking one would be lost.
    /// This models exactly that loop and asserts the survivors are delivered.
    #[test]
    fn a_panicking_pass_does_not_consume_the_channel() {
        let (tx, rx) = mpsc::channel::<u8>();
        for msg in [1u8, 2, 3] {
            tx.send(msg).unwrap();
        }
        drop(tx);

        let seen = std::sync::Mutex::new(Vec::new());
        loop {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // Stand-in for run_coordinator: drains the BORROWED receiver,
                // and panics part-way through the way a bad dictation would.
                while let Ok(v) = rx.recv() {
                    if v == 2 {
                        panic!("simulated mid-dictation panic");
                    }
                    seen.lock().unwrap().push(v);
                }
            }));
            if outcome.is_ok() {
                break; // channel closed — clean shutdown, as in spawn()
            }
        }

        // 2 is lost (it was in flight when the panic hit) but 3 still arrives:
        // the restarted loop kept serving the same channel.
        assert_eq!(*seen.lock().unwrap(), vec![1, 3]);
    }

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

    // ---- error-string → user-facing overlay text (item 2) ----

    #[test]
    fn model_missing_engine_error_maps_to_actionable_message() {
        // The exact string engine.rs's ParakeetModel::load bails with.
        let engine_err = "model directory /Users/x/.config/vzt-flow/models/parakeet-v3 \
             does not exist. Run `flow models download parakeet-v3` first.";
        let msg = transcription_error_message(engine_err);
        assert_ne!(msg, "Transcription failed", "must not fall through to the generic");
        assert!(msg.to_lowercase().contains("model"));
        assert!(msg.to_lowercase().contains("settings") || msg.to_lowercase().contains("download"));
    }

    #[test]
    fn unrelated_error_maps_to_generic() {
        assert_eq!(
            transcription_error_message("transcription timed out"),
            "Transcription failed"
        );
        assert_eq!(
            transcription_error_message("some totally unrelated failure"),
            "Transcription failed"
        );
    }

    // ---- B5: a stray key cancels only a chord, not a long dictation ----

    #[test]
    fn a_stray_key_early_in_a_hold_is_still_treated_as_a_chord() {
        // Option+e typed right after pressing Right Option: cancel, as before.
        assert!(other_key_is_chord(Some(Duration::from_millis(400))));
        assert!(other_key_is_chord(Some(Duration::from_millis(1499))));
    }

    #[test]
    fn a_stray_key_late_in_a_long_hold_does_not_discard_the_dictation() {
        // Brushing a key 30s into a dictation used to cancel it silently.
        assert!(!other_key_is_chord(Some(Duration::from_secs(30))));
        assert!(!other_key_is_chord(Some(CHORD_WINDOW)));
        assert!(!other_key_is_chord(None), "no known press: nothing to cancel");
    }

    // ---- B2: a finished take is never dropped on the floor ----

    #[test]
    fn a_final_for_the_waiting_dictation_runs_the_pipeline() {
        assert_eq!(route_rolling_final(Some(7), 7, true), FinalRoute::Pipeline);
        assert_eq!(route_rolling_final(Some(7), 7, false), FinalRoute::Pipeline);
    }

    #[test]
    fn a_final_nobody_is_waiting_for_goes_to_the_clipboard_not_the_void() {
        // The old coordinator discarded a Final that arrived after its
        // watchdog had given up, or after a newer recording had started.
        assert_eq!(route_rolling_final(None, 7, true), FinalRoute::ClipboardOnly);
        assert_eq!(route_rolling_final(Some(8), 7, true), FinalRoute::ClipboardOnly);
        assert_eq!(route_rolling_final(None, 7, false), FinalRoute::Ignore);
    }

    fn speech_stats() -> AudioStats {
        AudioStats { peak: 0.4, speech_secs: 60.0, duration_secs: 70.0 }
    }

    #[test]
    fn an_incomplete_transcript_saves_the_audio_for_recovery() {
        let partial = DictationMeta { partial: true, stats: speech_stats(), ..Default::default() };
        assert!(should_save_recovery(false, &partial));
        let failed = DictationMeta { failed_chunks: 1, stats: speech_stats(), ..Default::default() };
        assert!(should_save_recovery(false, &failed));
        let empty_speech = DictationMeta { stats: speech_stats(), ..Default::default() };
        assert!(should_save_recovery(true, &empty_speech), "speech in, nothing out");
    }

    #[test]
    fn a_silent_mic_never_overwrites_the_recovery_slot() {
        // Found live: a watchdog trip on a take of digital silence counted as
        // partial and replaced the previous (speech) recording in the one
        // recovery slot with 20s of zeros.
        let silent = AudioStats { peak: 0.0, speech_secs: 0.0, duration_secs: 20.0 };
        for meta in [
            DictationMeta { partial: true, failed_chunks: 1, stats: silent, ..Default::default() },
            DictationMeta { failed_chunks: 1, stats: silent, ..Default::default() },
        ] {
            assert!(!should_save_recovery(true, &meta), "{meta:?}");
        }
        // A quiet-but-live mic still keeps a failed take.
        let quiet = AudioStats { peak: 0.02, speech_secs: 0.5, duration_secs: 40.0 };
        assert!(should_save_recovery(false, &DictationMeta { failed_chunks: 1, stats: quiet, ..Default::default() }));
    }

    #[test]
    fn a_complete_dictation_or_an_empty_silent_one_saves_nothing() {
        let ok = DictationMeta { chunks: 3, stats: speech_stats(), ..Default::default() };
        assert!(!should_save_recovery(false, &ok));
        let quiet = DictationMeta { stats: AudioStats { peak: 0.02, speech_secs: 0.3, duration_secs: 5.0 }, ..Default::default() };
        assert!(!should_save_recovery(true, &quiet), "an accidental tap must not overwrite a real recording");
    }

    // ---- B4: an empty result is never a silent "Done" ----

    #[test]
    fn an_empty_transcript_is_not_pasted_and_says_why() {
        let saved = DictationMeta { stats: speech_stats(), audio_saved: true, ..Default::default() };
        assert_eq!(
            empty_result_message("  ", &saved).as_deref(),
            Some("No speech recognized — audio saved")
        );
        let silent = DictationMeta { stats: AudioStats { peak: 0.0, speech_secs: 0.0, duration_secs: 30.0 }, ..Default::default() };
        assert_eq!(empty_result_message("", &silent).as_deref(), Some("Mic was silent — check your input"));
        let quiet = DictationMeta { stats: AudioStats { peak: 0.03, speech_secs: 0.0, duration_secs: 4.0 }, ..Default::default() };
        assert_eq!(empty_result_message("", &quiet).as_deref(), Some("No speech recognized"));
    }

    #[test]
    fn a_watchdog_trip_before_any_chunk_finished_is_not_called_no_speech() {
        // Measured live: a cold model took 544s to load under load, the
        // watchdog tripped with 0 of 3 chunks done, and the text arrived
        // later as Late. "No speech recognized" would be false there.
        let m = DictationMeta { partial: true, failed_chunks: 3, stats: speech_stats(), audio_saved: true, ..Default::default() };
        assert_eq!(empty_result_message("", &m).as_deref(), Some("Transcription slow — audio saved"));
    }

    #[test]
    fn a_real_transcript_is_pasted() {
        assert_eq!(empty_result_message("Hello there.", &DictationMeta::default()), None);
    }

    // ---- B2/B3: an incomplete paste says so ----

    #[test]
    fn a_partial_transcript_is_labelled_as_partial() {
        let m = DictationMeta { partial: true, failed_chunks: 1, audio_saved: true, ..Default::default() };
        assert_eq!(overlay_message(&m, None).as_deref(), Some("Partial transcript — audio saved"));
        let f = DictationMeta { failed_chunks: 2, audio_saved: true, ..Default::default() };
        assert_eq!(overlay_message(&f, None).as_deref(), Some("Some audio couldn't be transcribed"));
    }

    #[test]
    fn a_paste_problem_still_takes_precedence_and_a_clean_run_shows_done() {
        let m = DictationMeta { partial: true, ..Default::default() };
        let p = Some("Secure field — transcript on clipboard".to_string());
        assert_eq!(overlay_message(&m, p.clone()), p);
        assert_eq!(overlay_message(&DictationMeta::default(), None), None);
    }

    // ---- B1: one log line per dictation ----

    #[test]
    fn the_outcome_line_carries_everything_needed_to_diagnose_a_lost_take() {
        let meta = DictationMeta {
            chunks: 3,
            failed_chunks: 1,
            partial: true,
            stats: AudioStats { peak: 0.4123, speech_secs: 58.34, duration_secs: 70.0 },
            audio_saved: true,
        };
        let line = format_outcome_line("partial", Some("timeout"), Duration::from_secs_f64(70.06), &meta, 812);
        for field in [
            "dictation outcome=partial reason=timeout",
            "duration=70.1s",
            "chunks=3",
            "failed_chunks=1",
            "peak=0.412",
            "speech=58.3s",
            "chars=812",
            "audio_saved=true",
        ] {
            assert!(line.contains(field), "missing {field:?} in {line}");
        }
        assert!(line.starts_with("[vzt-flow] 20"), "must be timestamped: {line}");
    }

    #[test]
    fn the_batch_path_allows_a_loaded_machine_six_times_real_time() {
        // The old `duration + 60s` expired for a 69s take at RTF 1.9.
        assert_eq!(batch_transcribe_timeout(Duration::from_secs(69)), Duration::from_secs(69 * 6 + 120));
        assert!(batch_transcribe_timeout(Duration::from_secs(600)) >= Duration::from_secs(3600));
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
