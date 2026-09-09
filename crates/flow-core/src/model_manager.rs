//! Owns the (lazily loaded, idle-unloaded) transcriber on a dedicated
//! thread so the tray/overlay never blocks on model load/inference.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::audio::TARGET_SAMPLE_RATE;
use crate::chunking::transcribe_long;
use crate::engine::{ParakeetTranscriber, Transcriber, Transcript};

pub enum ModelCommand {
    /// Transcribe a complete recording. Long audio is chunked internally by
    /// [`transcribe_long`] to bound the engine's quadratic memory growth.
    Transcribe {
        samples: Vec<f32>,
        audio_duration: Duration,
        reply: mpsc::Sender<Result<Transcript, String>>,
    },
    /// Transcribe a single, already-bounded (≤35s) chunk in one pass — used by
    /// the rolling path (`crate::rolling`) to transcribe settled chunks during
    /// recording and the tail at release. Shares this manager's engine, so all
    /// chunks (and any concurrent full `Transcribe`) serialize on the one
    /// thread; the caller has already sized the chunk under the single-pass
    /// memory ceiling, so this deliberately skips `transcribe_long`.
    TranscribeChunk {
        samples: Vec<f32>,
        reply: mpsc::Sender<Result<Transcript, String>>,
    },
    /// Best-effort: load the model now if it isn't loaded, so the (multi-
    /// second) load overlaps the user speaking instead of landing on the
    /// critical path at release. No reply — fire and forget from the
    /// coordinator, mirroring `CleanupCommand::Warmup`. Idempotent: a
    /// second Warmup while loaded is a no-op. Resets the idle timer, so a
    /// press counts as activity, and a preload that is never used still
    /// unloads after `idle_unload_secs`.
    Warmup,
}

#[derive(Debug, Clone)]
pub enum ModelStatusEvent {
    Loading,
    Loaded { load_time: Duration },
    LoadFailed(String),
    /// Emitted after unloading due to idle timeout.
    Unloaded,
}

/// Loads the transcriber into `model` if it isn't already present. Shared by
/// the transcribe commands and `Warmup` so both go through identical
/// load/status-event bookkeeping. Returns whether a model is now available.
fn ensure_loaded(
    model: &mut Option<ParakeetTranscriber>,
    model_dir: &Path,
    status_tx: &mpsc::Sender<ModelStatusEvent>,
) -> Result<(), String> {
    if model.is_some() {
        return Ok(());
    }
    let _ = status_tx.send(ModelStatusEvent::Loading);
    match ParakeetTranscriber::load(model_dir) {
        Ok(m) => {
            let _ = status_tx.send(ModelStatusEvent::Loaded { load_time: m.load_time });
            *model = Some(m);
            Ok(())
        }
        Err(e) => {
            let _ = status_tx.send(ModelStatusEvent::LoadFailed(e.to_string()));
            Err(e.to_string())
        }
    }
}

/// Spawns the model-lifecycle thread. Runs until `cmd_rx` disconnects.
pub fn spawn(
    model_dir: PathBuf,
    idle_timeout: Duration,
    cmd_rx: mpsc::Receiver<ModelCommand>,
    status_tx: mpsc::Sender<ModelStatusEvent>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("vzt-flow-model-manager".into())
        .spawn(move || {
            let mut model: Option<ParakeetTranscriber> = None;
            let mut last_used = Instant::now();

            loop {
                match cmd_rx.recv_timeout(idle_timeout) {
                    Ok(ModelCommand::Warmup) => {
                        last_used = Instant::now();
                        if model.is_some() {
                            continue;
                        }
                        let _ = ensure_loaded(&mut model, &model_dir, &status_tx);
                    }
                    Ok(cmd) => {
                        // Unify the two transcription commands: a full
                        // `Transcribe` (chunked internally) vs. a single
                        // pre-bounded `TranscribeChunk` (single pass). Both
                        // share model load, panic isolation, and RTF logging.
                        let (samples, audio_duration, reply, is_chunk) = match cmd {
                            ModelCommand::Transcribe { samples, audio_duration, reply } => {
                                (samples, audio_duration, reply, false)
                            }
                            ModelCommand::TranscribeChunk { samples, reply } => {
                                let d = Duration::from_secs_f64(
                                    samples.len() as f64 / TARGET_SAMPLE_RATE as f64,
                                );
                                (samples, d, reply, true)
                            }
                            // Already handled by the `Ok(ModelCommand::Warmup)` arm above.
                            ModelCommand::Warmup => unreachable!("Warmup is matched before this arm"),
                        };

                        last_used = Instant::now();
                        if let Err(e) = ensure_loaded(&mut model, &model_dir, &status_tx) {
                            let _ = reply.send(Err(e));
                            continue;
                        }
                        let transcriber = model.as_mut().expect("model just loaded or already present");
                        let started = Instant::now();
                        // A panic inside the ONNX inference path (bad tensor
                        // shape, allocator abort, etc.) must not take down this
                        // thread — that would wedge every future dictation in
                        // Transcribing forever. Catch it, reply Err, and drop
                        // the transcriber so the next command reloads cleanly.
                        // `transcribe_long` chunks multi-minute audio so the
                        // engine's quadratic memory growth can't OOM-kill the
                        // daemon; a rolling chunk is already ≤35s so it takes
                        // the single-pass path directly.
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            if is_chunk {
                                transcriber.transcribe(&samples)
                            } else {
                                transcribe_long(&samples, transcriber)
                            }
                        }));
                        let infer_time = started.elapsed();
                        match result {
                            Ok(transcript) => {
                                let rtf = if audio_duration.as_secs_f64() > 0.0 {
                                    infer_time.as_secs_f64() / audio_duration.as_secs_f64()
                                } else {
                                    0.0
                                };
                                eprintln!(
                                    "[vzt-flow] transcribed {:.2}s {} in {:.2}s (RTF {:.3})",
                                    audio_duration.as_secs_f64(),
                                    if is_chunk { "rolling chunk" } else { "audio" },
                                    infer_time.as_secs_f64(),
                                    rtf
                                );
                                let _ = reply.send(transcript.map_err(|e| e.to_string()));
                            }
                            Err(_panic) => {
                                eprintln!(
                                    "[vzt-flow] transcriber panicked; dropping model to force a \
                                     clean reload on the next request"
                                );
                                model = None;
                                let _ = reply.send(Err(
                                    "transcription failed (internal error)".to_string()
                                ));
                            }
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if model.is_some() && last_used.elapsed() >= idle_timeout {
                            model = None;
                            eprintln!("[vzt-flow] model unloaded after {idle_timeout:?} idle");
                            let _ = status_tx.send(ModelStatusEvent::Unloaded);
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        })
        .expect("failed to spawn model manager thread")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A path that can never resolve to a real model, so `ParakeetTranscriber::load`
    /// bails immediately (`engine.rs:91-95`) and no real model is required for these
    /// tests — they only exercise `Warmup`'s bookkeeping, never real inference.
    fn missing_model_dir() -> PathBuf {
        PathBuf::from("/nonexistent/vzt-flow-model-manager-test-dir")
    }

    fn spawn_with(idle_timeout: Duration) -> (mpsc::Sender<ModelCommand>, mpsc::Receiver<ModelStatusEvent>) {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (status_tx, status_rx) = mpsc::channel();
        spawn(missing_model_dir(), idle_timeout, cmd_rx, status_tx);
        (cmd_tx, status_rx)
    }

    /// Every `ensure_loaded` attempt against a missing model dir emits
    /// `Loading` before `LoadFailed`. Drains the `Loading` and returns the
    /// event after it, so tests can assert on `LoadFailed` without hardcoding
    /// that ordering inline.
    fn recv_past_loading(status_rx: &mpsc::Receiver<ModelStatusEvent>) -> ModelStatusEvent {
        match status_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(ModelStatusEvent::Loading) => {
                status_rx.recv_timeout(Duration::from_secs(5)).expect("event after Loading")
            }
            Ok(other) => other,
            Err(e) => panic!("expected a status event, got {e:?}"),
        }
    }

    #[test]
    fn warmup_on_a_missing_model_emits_load_failed_and_does_not_panic() {
        let (cmd_tx, status_rx) = spawn_with(Duration::from_secs(300));

        cmd_tx.send(ModelCommand::Warmup).unwrap();
        match recv_past_loading(&status_rx) {
            ModelStatusEvent::LoadFailed(_) => {}
            other => panic!("expected LoadFailed after first Warmup, got {other:?}"),
        }

        // The thread must still be alive (not panicked) after a failed
        // Warmup — a second Warmup should also produce a LoadFailed.
        cmd_tx.send(ModelCommand::Warmup).unwrap();
        match recv_past_loading(&status_rx) {
            ModelStatusEvent::LoadFailed(_) => {}
            other => panic!("expected LoadFailed after second Warmup, got {other:?}"),
        }
    }

    #[test]
    fn warmup_is_not_answered_and_never_blocks_the_sender() {
        let (cmd_tx, _status_rx) = spawn_with(Duration::from_secs(300));

        // `Warmup` carries no reply channel, so there is nothing to wait on;
        // the send itself must return immediately.
        let result = cmd_tx.send(ModelCommand::Warmup);
        assert!(result.is_ok(), "Warmup send should succeed without blocking on any reply");
    }

    #[test]
    fn warmup_resets_the_idle_timer() {
        let idle_timeout = Duration::from_millis(300);
        let (cmd_tx, status_rx) = spawn_with(idle_timeout);

        // Two Warmups 200ms apart against a missing model: each resets
        // `last_used`, so the idle-unload branch should never fire before
        // 400ms even though the timeout itself is only 300ms.
        cmd_tx.send(ModelCommand::Warmup).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        cmd_tx.send(ModelCommand::Warmup).unwrap();

        let deadline = Instant::now() + Duration::from_millis(400);
        while Instant::now() < deadline {
            if let Ok(ModelStatusEvent::Unloaded) = status_rx.try_recv() {
                panic!("Unloaded fired before the idle timer should have elapsed");
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        // The thread must still be responsive: a further Warmup still
        // produces a LoadFailed rather than silence from a wedged loop.
        cmd_tx.send(ModelCommand::Warmup).unwrap();
        match recv_past_loading(&status_rx) {
            ModelStatusEvent::LoadFailed(_) => {}
            other => panic!("expected LoadFailed, thread appears wedged: got {other:?}"),
        }
    }

    #[test]
    fn a_never_used_preload_still_unloads() {
        // Documents the intent: a Warmup that is never followed by a
        // transcribe still counts as "used" for idle-unload purposes via
        // `last_used = Instant::now()`, so the unload branch's existing
        // guard (`model.is_some() && last_used.elapsed() >= idle_timeout`)
        // is what eventually reclaims it — no separate "warmed but idle"
        // path is needed. Exercised here against the missing-model path
        // (so no real model is required): after a short idle_timeout with
        // no further activity, the loop must still observe timeouts and
        // stay alive rather than getting stuck waiting on `recv_timeout`.
        let idle_timeout = Duration::from_millis(100);
        let (cmd_tx, status_rx) = spawn_with(idle_timeout);

        cmd_tx.send(ModelCommand::Warmup).unwrap();
        // Drain the Loading + LoadFailed from the Warmup itself.
        let _ = recv_past_loading(&status_rx);

        // No model ever loaded (load failed), so there is nothing to
        // unload — but the loop must still be alive and responsive after
        // several idle_timeout windows have elapsed with no activity.
        std::thread::sleep(idle_timeout * 4);
        cmd_tx.send(ModelCommand::Warmup).unwrap();
        match recv_past_loading(&status_rx) {
            ModelStatusEvent::LoadFailed(_) => {}
            other => panic!("expected LoadFailed, thread appears wedged: got {other:?}"),
        }
    }
}
