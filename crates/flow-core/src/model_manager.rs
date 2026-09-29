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
    /// Load the model now if it isn't already, without transcribing anything.
    /// Sent when a recording *starts* so the (minutes-long, on a loaded
    /// machine) cold load overlaps with the user speaking instead of landing
    /// on the first rolling chunk or the release tail. Emits the same
    /// `Loading`/`Loaded`/`LoadFailed` events as a lazy load (the rolling
    /// watchdog keys off them), refreshes the idle clock, and replies nothing.
    /// A no-op when the model is already resident. Because the manager is a
    /// serial queue, a later `TranscribeChunk` simply waits behind the load.
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

/// Loads a transcriber from a model directory, returning it with the wall
/// time the load took. Injectable so the lifecycle can be tested without a
/// real ONNX model.
type Loader = Box<dyn FnMut(&Path) -> anyhow::Result<(Box<dyn Transcriber>, Duration)> + Send>;

fn parakeet_loader() -> Loader {
    Box::new(|dir| {
        let m = ParakeetTranscriber::load(dir)?;
        let t = m.load_time;
        Ok((Box::new(m) as Box<dyn Transcriber>, t))
    })
}

/// Spawns the model-lifecycle thread. Runs until `cmd_rx` disconnects.
pub fn spawn(
    model_dir: PathBuf,
    idle_timeout: Duration,
    cmd_rx: mpsc::Receiver<ModelCommand>,
    status_tx: mpsc::Sender<ModelStatusEvent>,
) -> std::thread::JoinHandle<()> {
    spawn_with(model_dir, idle_timeout, cmd_rx, status_tx, parakeet_loader())
}

/// Load into `model` if empty, emitting the lifecycle events. Shared by the
/// lazy load on a transcription command and by `Warmup`, so both take the
/// identical path.
fn ensure_loaded(
    model: &mut Option<Box<dyn Transcriber>>,
    loader: &mut Loader,
    model_dir: &Path,
    status_tx: &mpsc::Sender<ModelStatusEvent>,
) -> Result<(), String> {
    if model.is_some() {
        return Ok(());
    }
    let _ = status_tx.send(ModelStatusEvent::Loading);
    match loader(model_dir) {
        Ok((m, load_time)) => {
            let _ = status_tx.send(ModelStatusEvent::Loaded { load_time });
            *model = Some(m);
            Ok(())
        }
        Err(e) => {
            let _ = status_tx.send(ModelStatusEvent::LoadFailed(e.to_string()));
            Err(e.to_string())
        }
    }
}

fn spawn_with(
    model_dir: PathBuf,
    idle_timeout: Duration,
    cmd_rx: mpsc::Receiver<ModelCommand>,
    status_tx: mpsc::Sender<ModelStatusEvent>,
    mut loader: Loader,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("vzt-flow-model-manager".into())
        .spawn(move || {
            let mut model: Option<Box<dyn Transcriber>> = None;
            let mut last_used = Instant::now();

            loop {
                match cmd_rx.recv_timeout(idle_timeout) {
                    Ok(cmd) => {
                        // Unify the two transcription commands: a full
                        // `Transcribe` (chunked internally) vs. a single
                        // pre-bounded `TranscribeChunk` (single pass). Both
                        // share model load, panic isolation, and RTF logging.
                        let (samples, audio_duration, reply, is_chunk) = match cmd {
                            ModelCommand::Transcribe { samples, audio_duration, reply } => {
                                (samples, audio_duration, reply, false)
                            }
                            ModelCommand::Warmup => {
                                last_used = Instant::now();
                                // Same load path, events and error handling as
                                // the lazy load; nobody is waiting on a reply,
                                // so a failure is only logged (the next real
                                // transcription retries and surfaces it).
                                if let Err(e) =
                                    ensure_loaded(&mut model, &mut loader, &model_dir, &status_tx)
                                {
                                    eprintln!("[vzt-flow] model warm-up failed: {e}");
                                }
                                // A load can take minutes; don't let it count
                                // against the idle clock.
                                last_used = Instant::now();
                                continue;
                            }
                            ModelCommand::TranscribeChunk { samples, reply } => {
                                let d = Duration::from_secs_f64(
                                    samples.len() as f64 / TARGET_SAMPLE_RATE as f64,
                                );
                                (samples, d, reply, true)
                            }
                        };

                        last_used = Instant::now();
                        if let Err(e) = ensure_loaded(&mut model, &mut loader, &model_dir, &status_tx) {
                            let _ = reply.send(Err(e));
                            continue;
                        }
                        let transcriber = model.as_mut().expect("model just loaded or already present").as_mut();
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct Stub;
    impl Transcriber for Stub {
        fn transcribe(&mut self, _s: &[f32]) -> anyhow::Result<Transcript> {
            Ok(Transcript { text: "ok".into(), segments: None })
        }
    }

    fn harness() -> (
        mpsc::Sender<ModelCommand>,
        mpsc::Receiver<ModelStatusEvent>,
        Arc<AtomicUsize>,
        std::thread::JoinHandle<()>,
    ) {
        let loads = Arc::new(AtomicUsize::new(0));
        let l = loads.clone();
        let loader: Loader = Box::new(move |_| {
            l.fetch_add(1, Ordering::SeqCst);
            Ok((Box::new(Stub) as Box<dyn Transcriber>, Duration::from_millis(1)))
        });
        let (tx, rx) = mpsc::channel();
        let (stx, srx) = mpsc::channel();
        let h = spawn_with(PathBuf::from("/nonexistent"), Duration::from_secs(60), rx, stx, loader);
        (tx, srx, loads, h)
    }

    fn chunk(tx: &mpsc::Sender<ModelCommand>) {
        let (rtx, rrx) = mpsc::channel();
        tx.send(ModelCommand::TranscribeChunk { samples: vec![0.0; 1600], reply: rtx }).unwrap();
        assert_eq!(rrx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap().text, "ok");
    }

    #[test]
    fn warmup_loads_once_and_a_later_chunk_does_not_reload() {
        let (tx, srx, loads, h) = harness();
        tx.send(ModelCommand::Warmup).unwrap();
        assert!(matches!(srx.recv_timeout(Duration::from_secs(5)), Ok(ModelStatusEvent::Loading)));
        assert!(matches!(srx.recv_timeout(Duration::from_secs(5)), Ok(ModelStatusEvent::Loaded { .. })));
        chunk(&tx);
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        drop(tx);
        h.join().unwrap();
    }

    #[test]
    fn warmup_on_a_loaded_model_does_not_reload() {
        let (tx, _srx, loads, h) = harness();
        chunk(&tx); // lazy load
        tx.send(ModelCommand::Warmup).unwrap();
        tx.send(ModelCommand::Warmup).unwrap();
        chunk(&tx);
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        drop(tx);
        h.join().unwrap();
    }
}
