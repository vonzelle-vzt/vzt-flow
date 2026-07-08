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
use flow_core::{codemode, dictionary, history, insert, permissions, snippets};
use tauri::{AppHandle, Manager};

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
    let hotkey_result =
        flow_core::hotkey::spawn_monitor(config.hotkey_keycode, is_recording.clone(), hotkey_tx);
    let hotkey_active = hotkey_result.is_ok();
    if let Ok(keycode_handle) = &hotkey_result {
        *app.state::<AppState>().hotkey_keycode_handle.lock().unwrap() = Some(keycode_handle.clone());
    } else {
        eprintln!(
            "[vzt-flow] hotkey monitor failed to install a CGEventTap — this almost always means \
             Input Monitoring permission hasn't been granted (System Settings > Privacy & Security \
             > Input Monitoring). The tray's manual Start/Stop item still works."
        );
    }
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
    };
    while let Ok(msg) = rx.recv() {
        let state = app.state::<AppState>();
        match msg {
            CoordinatorMsg::Hotkey(HotkeyEvent::HoldKeyPressed) => {
                hold.key_down.store(true, Ordering::Relaxed);
                let gen = hold.generation.fetch_add(1, Ordering::Relaxed) + 1;
                // Fresh press: nothing resolved yet.
                hold.consumed.store(false, Ordering::Relaxed);

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
                    let still_same_press = generation.load(Ordering::Relaxed) == gen;
                    let still_down = key_down.load(Ordering::Relaxed);
                    let not_consumed = !consumed.load(Ordering::Relaxed);
                    if still_same_press && still_down && not_consumed {
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
                let hands_free = state.hands_free_active.load(Ordering::Relaxed);
                let current = *state.dictation_state.lock().unwrap();

                if hands_free {
                    // Tap while hands-free was live: toggle it off.
                    state.hands_free_active.store(false, Ordering::Relaxed);
                    stop_and_transcribe(&audio_cmd_tx);
                } else if current == DictationState::Recording {
                    // Hold threshold had already fired and recording is
                    // under way: this release ends it.
                    stop_and_transcribe(&audio_cmd_tx);
                } else if current == DictationState::Idle && !was_consumed {
                    // Genuine short tap (Idle, released before the hold
                    // threshold, and not already resolved by an Escape-cancel
                    // or an early cap): toggle hands-free recording on.
                    //
                    // The `!was_consumed` guard is the fix for F4 — without it
                    // an Idle reached via Escape-cancel or a cap-that-outlived
                    // its transcription would be misread as a tap and silently
                    // start a surprise hands-free recording.
                    state.hands_free_active.store(true, Ordering::Relaxed);
                    start_recording(&app, max_handsfree_secs(&app));
                }
                // else: consumed, or mid-transcription/paste — no-op.
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
            CoordinatorMsg::Audio(AudioReply::Started) => {}
            CoordinatorMsg::Audio(AudioReply::Level(level)) => {
                overlay::emit_overlay(&app, OverlayEvent::Recording { level });
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
                    // inference trips the 60s timeout. Either way synthesize an
                    // error result so the state machine leaves Transcribing and
                    // the overlay is dismissed instead of hanging (F2).
                    let result = match reply_rx.recv_timeout(Duration::from_secs(60)) {
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
                if let Some((tx, _)) = state.pending_listen.lock().unwrap().take() {
                    let _ = tx.send(Err("recording cancelled".to_string()));
                }
                state.set_dictation_state(DictationState::Idle);
                overlay::hide_overlay(&app);
            }
            CoordinatorMsg::Audio(AudioReply::Error(e)) => {
                eprintln!("[vzt-flow] audio error: {e}");
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
    tray::refresh_menu(app);
    overlay::show_overlay(app);
    overlay::emit_overlay(app, OverlayEvent::Recording { level: 0.0 });
    // Energy-based auto-stop only applies to hands-free recordings — a
    // hold-to-talk recording's only stop signal is releasing the key.
    let handsfree_silence_secs = if state.hands_free_active.load(Ordering::Relaxed) {
        Some(state.config.lock().unwrap().handsfree_silence_secs)
    } else {
        None
    };
    let tx = state.audio_cmd_tx.lock().unwrap().clone();
    if let Some(tx) = tx {
        let _ = tx.send(AudioCommand::Start { max_secs, handsfree_silence_secs });
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

    let timeout_ms = state.config.lock().unwrap().cleanup_timeout_ms;
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

/// Cycles the overlay through recording -> transcribing -> done -> hidden,
/// with fake level values, entirely for visual QA via the "Test overlay"
/// tray item — no microphone or transcriber involved.
fn run_overlay_self_test(app: &AppHandle) {
    overlay::show_overlay(app);
    let app2 = app.clone();
    std::thread::spawn(move || {
        for level in [0.1, 0.4, 0.8, 0.5, 0.2] {
            overlay::emit_overlay(&app2, OverlayEvent::Recording { level });
            std::thread::sleep(Duration::from_millis(250));
        }
        overlay::emit_overlay(&app2, OverlayEvent::Transcribing { mode: "clean".to_string() });
        std::thread::sleep(Duration::from_millis(700));
        overlay::emit_overlay(&app2, OverlayEvent::Done);
        std::thread::sleep(Duration::from_millis(900));
        overlay::hide_overlay(&app2);
    });
}
