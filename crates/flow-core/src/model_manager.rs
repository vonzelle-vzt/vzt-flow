//! Owns the (lazily loaded, idle-unloaded) transcriber on a dedicated
//! thread so the tray/overlay never blocks on model load/inference.

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::engine::{ParakeetTranscriber, Transcript, Transcriber};

pub enum ModelCommand {
    Transcribe {
        samples: Vec<f32>,
        audio_duration: Duration,
        reply: mpsc::Sender<Result<Transcript, String>>,
    },
}

#[derive(Debug, Clone)]
pub enum ModelStatusEvent {
    Loading,
    Loaded { load_time: Duration },
    LoadFailed(String),
    /// Emitted after unloading due to idle timeout.
    Unloaded,
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
                    Ok(ModelCommand::Transcribe {
                        samples,
                        audio_duration,
                        reply,
                    }) => {
                        last_used = Instant::now();
                        if model.is_none() {
                            let _ = status_tx.send(ModelStatusEvent::Loading);
                            match ParakeetTranscriber::load(&model_dir) {
                                Ok(m) => {
                                    let _ = status_tx.send(ModelStatusEvent::Loaded {
                                        load_time: m.load_time,
                                    });
                                    model = Some(m);
                                }
                                Err(e) => {
                                    let _ = status_tx.send(ModelStatusEvent::LoadFailed(e.to_string()));
                                    let _ = reply.send(Err(e.to_string()));
                                    continue;
                                }
                            }
                        }
                        let transcriber = model.as_mut().expect("model just loaded or already present");
                        let started = Instant::now();
                        // A panic inside the ONNX inference path (bad tensor
                        // shape, allocator abort, etc.) must not take down this
                        // thread — that would wedge every future dictation in
                        // Transcribing forever. Catch it, reply Err, and drop
                        // the transcriber so the next command reloads cleanly.
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            transcriber.transcribe(&samples)
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
                                    "[vzt-flow] transcribed {:.2}s audio in {:.2}s (RTF {:.3})",
                                    audio_duration.as_secs_f64(),
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
