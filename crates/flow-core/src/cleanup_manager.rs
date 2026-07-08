//! Owns the (lazily loaded, idle-unloaded) cleanup LLM on a dedicated
//! thread, mirroring `model_manager`'s lifecycle for the transcriber.
//!
//! The hard cleanup deadline lives here: each `Clean` command races the
//! actual generation (run on its own worker thread) against a timer. If the
//! generation wins, its text is used; if the timer wins, `cancel` is set so
//! the worker's token loop stops within one token (see `cleanup::generate`),
//! we give it a short grace period, then **join** the thread before
//! replying with the raw transcript — this manager never detaches a live
//! llama.cpp thread, which previously left an orphaned Metal-backed
//! generation running and crashed the process at exit
//! (`GGML_ASSERT([rsets->data count] == 0)`).
//!
//! `Warmup` is sent by the coordinator when a recording *starts* (not when
//! it ends) so model load and the first-ever-context Metal kernel-pipeline
//! JIT compilation — both of which are one-time-per-process costs of
//! several seconds — happen in parallel with the user speaking, instead of
//! eating into the first real cleanup's deadline.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use crate::cleanup::{CleanupContext, CleanupProvider, Mode};

/// How long to wait, after setting `cancel`, for the generation thread to
/// notice and send its (now-irrelevant) result before we join it. Generous
/// relative to a single token's decode time on the target hardware, so the
/// common case is "thread exits almost immediately" — this is a ceiling,
/// not the expected wait.
const CANCEL_GRACE: Duration = Duration::from_millis(1500);

pub enum CleanupCommand {
    Clean {
        raw: String,
        mode: Mode,
        ctx: CleanupContext,
        timeout_ms: u64,
        reply: mpsc::Sender<CleanupResult>,
    },
    /// Best-effort: load the model if needed and run one throwaway
    /// generation to force Metal pipeline JIT compilation now. No reply —
    /// fire and forget from the coordinator.
    Warmup,
}

#[derive(Debug, Clone)]
pub struct CleanupResult {
    pub text: String,
    /// True only when the LLM produced the text within the deadline; false
    /// for raw mode, a missing/unloadable model, an empty/errored
    /// generation, or a deadline timeout — all of which fall back to the
    /// original (dictionary-corrected) transcript.
    pub used_llm: bool,
}

#[derive(Debug, Clone)]
pub enum CleanupStatusEvent {
    Loading,
    Loaded { load_time: Duration },
    LoadFailed(String),
    Unloaded,
}

#[cfg(target_os = "macos")]
fn load_provider(model_path: &Path) -> anyhow::Result<Box<dyn CleanupProvider>> {
    Ok(Box::new(crate::cleanup::LlamaCleanupProvider::load(model_path)?))
}

#[cfg(not(target_os = "macos"))]
fn load_provider(_model_path: &Path) -> anyhow::Result<Box<dyn CleanupProvider>> {
    anyhow::bail!("embedded llama.cpp cleanup provider is only implemented for macOS")
}

/// Loads the provider into `provider` if it isn't already loaded (and
/// hasn't already failed once this load-cycle). Shared by the `Clean` and
/// `Warmup` command handlers so both go through identical load/status-event
/// bookkeeping.
fn ensure_loaded(
    provider: &mut Option<Arc<dyn CleanupProvider>>,
    load_failed_once: &mut bool,
    model_path: &Path,
    status_tx: &mpsc::Sender<CleanupStatusEvent>,
) {
    if provider.is_some() || *load_failed_once {
        return;
    }
    let _ = status_tx.send(CleanupStatusEvent::Loading);
    let started = Instant::now();
    match load_provider(model_path) {
        Ok(p) => {
            let load_time = started.elapsed();
            eprintln!("[vzt-flow] cleanup model loaded in {:.2}s", load_time.as_secs_f64());
            let _ = status_tx.send(CleanupStatusEvent::Loaded { load_time });
            *provider = Some(Arc::from(p));
        }
        Err(e) => {
            eprintln!(
                "[vzt-flow] cleanup model unavailable ({e}); dictation will continue with the raw \
                 (dictionary-corrected) transcript for the rest of this session"
            );
            let _ = status_tx.send(CleanupStatusEvent::LoadFailed(e.to_string()));
            *load_failed_once = true;
        }
    }
}

/// Spawns the cleanup-lifecycle thread. Runs until `cmd_rx` disconnects.
pub fn spawn(
    model_path: PathBuf,
    idle_timeout: Duration,
    cmd_rx: mpsc::Receiver<CleanupCommand>,
    status_tx: mpsc::Sender<CleanupStatusEvent>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("vzt-flow-cleanup-manager".into())
        .spawn(move || {
            let mut provider: Option<Arc<dyn CleanupProvider>> = None;
            let mut load_failed_once = false;
            // Whether the current `provider` has already paid the one-time
            // Metal kernel-pipeline JIT cost. Reset whenever the model is
            // unloaded/reloaded.
            let mut warmed_up = false;
            let mut last_used = Instant::now();

            loop {
                match cmd_rx.recv_timeout(idle_timeout) {
                    Ok(CleanupCommand::Warmup) => {
                        ensure_loaded(&mut provider, &mut load_failed_once, &model_path, &status_tx);
                        if warmed_up {
                            continue;
                        }
                        let Some(p) = provider.clone() else { continue };
                        let started = Instant::now();
                        let cancel = AtomicBool::new(false);
                        let ctx = CleanupContext::default();
                        match p.clean("this is a warm up call", Mode::Clean, &ctx, &cancel) {
                            Ok(_) => {
                                warmed_up = true;
                                eprintln!(
                                    "[vzt-flow] cleanup model warmed up in {:.2}s",
                                    started.elapsed().as_secs_f64()
                                );
                            }
                            Err(e) => eprintln!("[vzt-flow] cleanup warmup generation failed (non-fatal): {e}"),
                        }
                    }
                    Ok(CleanupCommand::Clean { raw, mode, ctx, timeout_ms, reply }) => {
                        last_used = Instant::now();

                        if mode == Mode::Raw {
                            let _ = reply.send(CleanupResult { text: raw, used_llm: false });
                            continue;
                        }

                        ensure_loaded(&mut provider, &mut load_failed_once, &model_path, &status_tx);
                        let Some(p) = provider.clone() else {
                            let _ = reply.send(CleanupResult { text: raw, used_llm: false });
                            continue;
                        };

                        let cancel = Arc::new(AtomicBool::new(false));
                        let (gen_tx, gen_rx) = mpsc::channel();
                        let raw_for_gen = raw.clone();
                        let cancel_for_gen = cancel.clone();
                        let handle = std::thread::spawn(move || {
                            let result = p.clean(&raw_for_gen, mode, &ctx, &cancel_for_gen);
                            let _ = gen_tx.send(result);
                        });

                        let (final_text, used_llm, log_msg) =
                            match gen_rx.recv_timeout(Duration::from_millis(timeout_ms)) {
                                Ok(Ok(text)) if !text.trim().is_empty() => {
                                    (text, true, format!("llm path won ({} mode)", mode.label()))
                                }
                                Ok(Ok(_empty)) => (
                                    raw.clone(),
                                    false,
                                    "llm produced no usable output; falling back to raw".to_string(),
                                ),
                                Ok(Err(e)) => {
                                    (raw.clone(), false, format!("generation failed ({e}); falling back to raw"))
                                }
                                Err(_) => {
                                    // Deadline hit: ask the worker to stop, give it a
                                    // short grace period to notice and exit on its own
                                    // (it'll send its now-irrelevant result, which we
                                    // discard), then unconditionally join below —
                                    // never leave a live llama.cpp thread detached.
                                    cancel.store(true, Ordering::Relaxed);
                                    let _ = gen_rx.recv_timeout(CANCEL_GRACE);
                                    (
                                        raw.clone(),
                                        false,
                                        format!(
                                            "{timeout_ms}ms deadline exceeded; cancelled generation and \
                                             pasting raw"
                                        ),
                                    )
                                }
                            };
                        eprintln!("[vzt-flow] cleanup: {log_msg}");
                        // Always wait for the OS thread to actually finish —
                        // whether it already sent its result (fast path,
                        // returns immediately) or was just cancelled (grace
                        // path, blocks until the in-flight decode call
                        // returns and the loop's next cancel-check fires).
                        let _ = handle.join();
                        let _ = reply.send(CleanupResult { text: final_text, used_llm });
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if provider.is_some() && last_used.elapsed() >= idle_timeout {
                            provider = None;
                            warmed_up = false;
                            eprintln!("[vzt-flow] cleanup model unloaded after {idle_timeout:?} idle");
                            let _ = status_tx.send(CleanupStatusEvent::Unloaded);
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        })
        .expect("failed to spawn cleanup manager thread")
}
