//! Owns the (lazily loaded, idle-unloaded) cleanup LLM on a dedicated
//! thread, mirroring `model_manager`'s lifecycle for the transcriber.
//!
//! The hard cleanup deadline lives here: each `Clean` command races the
//! actual generation (run on its own worker thread) against a per-request
//! deadline computed by [`cleanup_deadline_ms`] (base cost + a
//! per-character allowance, capped — not a flat constant; see that
//! function's doc comment for the formula). If the
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
//!
//! # The scheduler
//!
//! This one worker owns the *only* resident LLM in the process (see
//! [`crate::llm`]), so dictation cleanup, the meeting summary and the
//! interview coach all queue here. They cannot simply take turns: a 20-second
//! background summary must not make the user's next dictation wait 20 seconds
//! for the model. So the worker does not consume commands one at a time —
//! it drains them into a [`Mailbox`] (an interactive queue plus one
//! latest-only slot each for coaching and background) and, while a generation
//! is in flight, comes up for air every [`SLICE`] to look at what has
//! arrived. Something strictly more urgent takes the model away from the
//! running job: cancel → [`PREEMPT_GRACE`] → **join** → requeue the
//! displaced request, up to [`MAX_PREEMPTIONS`] times before it is allowed
//! to finish regardless (a steady drip of dictations must not starve the
//! summary forever).
//!
//! Two invariants hold on every path through that machinery:
//!
//! - **No generation thread is ever dropped without `join()`** — deadline,
//!   preemption or normal completion alike (gotcha (e)).
//! - **Every reply channel is answered.** A request displaced from a
//!   latest-only slot, or preempted and then displaced, gets
//!   `Ok(String::new())` — the "no usable output" contract — because its
//!   caller is blocked on `recv` and would otherwise wait forever.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use crate::cleanup::{CleanupContext, CleanupProvider, Mode};
use crate::config::Config;
use crate::llm::{GenRequest, Priority};

/// Computes the wall-clock cleanup deadline (ms) for an input of
/// `input_char_len` characters: `cleanup_timeout_ms` (base cost — model
/// dispatch + short-input generation) plus `cleanup_timeout_per_char_ms` for
/// every character of input, capped at `cleanup_timeout_max_ms` so a single
/// dictation can never make the user wait longer than that for the LLM
/// before falling back to the raw transcript.
///
/// Derivation of the default `cleanup_timeout_per_char_ms` (6ms/char):
/// Qwen3-1.7B-Q4_K_M decodes at roughly 40-60 tok/s on M5 (measured); take
/// the 50 tok/s midpoint -> 20ms/token. Cleanup output is about as long as
/// input (grammar/filler fixes, not summarization) at ~4 characters/token,
/// with the same 1.3x headroom `cleanup::generate` reserves in its own
/// output-token budget for punctuation/expansion -> 20ms/token * 1.3 / 4
/// chars-per-token ~= 6.5ms/char, rounded down slightly to 6ms/char so the
/// common (short-dictation) case isn't over-budgeted.
pub fn cleanup_deadline_ms(input_char_len: usize, cfg: &Config) -> u64 {
    let scaled = cfg
        .cleanup_timeout_ms
        .saturating_add((input_char_len as u64).saturating_mul(cfg.cleanup_timeout_per_char_ms));
    scaled.min(cfg.cleanup_timeout_max_ms)
}

/// How long to wait, after setting `cancel`, for the generation thread to
/// notice and send its (now-irrelevant) result before we join it. Generous
/// relative to a single token's decode time on the target hardware, so the
/// common case is "thread exits almost immediately" — this is a ceiling,
/// not the expected wait.
const CANCEL_GRACE: Duration = Duration::from_millis(1500);

/// How long the worker waits on the in-flight generation before coming up
/// for air to check the mailbox. The *only* cost of a shorter slice is more
/// wakeups; the cost of a longer one is added latency on the preemption
/// path, which is the whole point of the scheduler.
const SLICE: Duration = Duration::from_millis(100);

/// The cancel grace on the *preemption* path. Much shorter than
/// [`CANCEL_GRACE`] because someone more urgent is already waiting — but it
/// is a grace period, not a deadline: the thread is joined unconditionally
/// afterwards either way (gotcha (e)).
const PREEMPT_GRACE: Duration = Duration::from_millis(250);

/// Starvation cap. After being preempted this many times a request runs to
/// completion regardless of what else arrives, so a steady drip of
/// dictations can never stop a meeting summary from ever finishing.
const MAX_PREEMPTIONS: u8 = 2;

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
    /// Free-form generation on the same resident model — the meeting summary
    /// and the interview coach (see `crate::llm`). `Ok(String::new())` on the
    /// reply channel means "no usable output" (no model, deadline, or
    /// displaced by a newer request of the same priority); `Err` means a
    /// generation actually ran and failed.
    Generate {
        req: GenRequest,
        reply: mpsc::Sender<Result<String, String>>,
    },
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

/// How a worker obtains its provider. Always `load_provider(&model_path)` in
/// production; the tests substitute a fake so the scheduler can be exercised
/// without a real 1.1GB model (and off macOS).
type ProviderLoader = Box<dyn FnMut() -> anyhow::Result<Box<dyn CleanupProvider>> + Send>;

/// Loads the provider into `provider` if it isn't already loaded (and
/// hasn't already failed once this load-cycle). Shared by the `Clean`,
/// `Generate` and `Warmup` command handlers so all three go through
/// identical load/status-event bookkeeping.
fn ensure_loaded(
    provider: &mut Option<Arc<dyn CleanupProvider>>,
    load_failed_once: &mut bool,
    load: &mut ProviderLoader,
    status_tx: &mpsc::Sender<CleanupStatusEvent>,
) {
    if provider.is_some() || *load_failed_once {
        return;
    }
    let _ = status_tx.send(CleanupStatusEvent::Loading);
    let started = Instant::now();
    match load() {
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

/// A free-form generation waiting for (or displaced from) the worker: the
/// request, the caller blocked on its answer, and how many times it has
/// already been preempted — the counter [`MAX_PREEMPTIONS`] caps.
struct PendingGen {
    req: GenRequest,
    reply: mpsc::Sender<Result<String, String>>,
    preempt_count: u8,
}

impl PendingGen {
    /// Answers the caller with the "no usable output" contract and drops the
    /// request. Used wherever a newer request supersedes this one: the caller
    /// is sitting on `reply_rx.recv()`, so it must be *answered*, not
    /// abandoned.
    fn answer_empty(self) {
        let _ = self.reply.send(Ok(String::new()));
    }
}

/// One unit of work the scheduler can run. `Clean` is dictation cleanup and
/// is always [`Priority::Interactive`]; `Gen` is everything else and carries
/// its own priority.
///
/// (The plan named this queue `VecDeque<CleanupCommand>`; it holds `Job`
/// instead so that `Warmup` — which is a flag, not queued work — is not
/// representable here, and so a preempted interactive generation can be
/// requeued with its `preempt_count` intact.)
enum Job {
    Clean {
        raw: String,
        mode: Mode,
        ctx: CleanupContext,
        timeout_ms: u64,
        reply: mpsc::Sender<CleanupResult>,
    },
    Gen(PendingGen),
}

/// What the scheduler chose to do next.
enum Pick {
    Warmup,
    Job(Job),
}

/// Work handed to the worker but not yet run.
///
/// Interactive work *queues*: a second dictation must not cancel the first,
/// and both were typed by a user who is waiting. Coaching and background get
/// one latest-only slot each, because a newer tip (or a newer summary) makes
/// an older pending one pointless — running both wastes the model on an
/// answer nobody is going to read.
#[derive(Default)]
struct Mailbox {
    interactive: VecDeque<Job>,
    coaching: Option<PendingGen>,
    background: Option<PendingGen>,
    warmup: bool,
}

impl Mailbox {
    fn push_command(&mut self, cmd: CleanupCommand) {
        match cmd {
            CleanupCommand::Warmup => self.warmup = true,
            CleanupCommand::Clean { raw, mode, ctx, timeout_ms, reply } => {
                self.interactive.push_back(Job::Clean { raw, mode, ctx, timeout_ms, reply });
            }
            CleanupCommand::Generate { req, reply } => {
                self.file(PendingGen { req, reply, preempt_count: 0 });
            }
        }
    }

    /// Files a generation in the slot for its priority, answering whatever it
    /// displaces.
    fn file(&mut self, gen: PendingGen) {
        match gen.req.priority {
            Priority::Interactive => self.interactive.push_back(Job::Gen(gen)),
            Priority::Coaching => {
                if let Some(displaced) = self.coaching.replace(gen) {
                    displaced.answer_empty();
                }
            }
            Priority::Background => {
                if let Some(displaced) = self.background.replace(gen) {
                    displaced.answer_empty();
                }
            }
        }
    }

    /// Puts a preempted generation back. Interactive work goes to the *front*
    /// of the queue (it was already running, so it precedes whatever queued
    /// behind it). A slotted request goes back into its slot — unless a newer
    /// one arrived while it ran, in which case latest-only wins and this
    /// older, already-interrupted request is answered empty rather than
    /// displacing the newer one.
    fn requeue(&mut self, gen: PendingGen) {
        match gen.req.priority {
            Priority::Interactive => self.interactive.push_front(Job::Gen(gen)),
            Priority::Coaching if self.coaching.is_some() => gen.answer_empty(),
            Priority::Background if self.background.is_some() => gen.answer_empty(),
            _ => self.file(gen),
        }
    }

    /// The most urgent thing waiting. `warmup` is deliberately excluded: it is
    /// an idle-only optimisation and never a reason to interrupt real work.
    fn best_priority(&self) -> Option<Priority> {
        if !self.interactive.is_empty() {
            Some(Priority::Interactive)
        } else if self.coaching.is_some() {
            Some(Priority::Coaching)
        } else if self.background.is_some() {
            Some(Priority::Background)
        } else {
            None
        }
    }

    /// Pick order: interactive → coaching → background → warmup. Warmup comes
    /// last, which is the same thing as the plan's "warmup, but only when
    /// idle": it is worth doing only when the worker would otherwise be
    /// sitting still, never at the cost of making a real request wait.
    fn pick(&mut self) -> Option<Pick> {
        if let Some(job) = self.interactive.pop_front() {
            return Some(Pick::Job(job));
        }
        if let Some(gen) = self.coaching.take() {
            return Some(Pick::Job(Job::Gen(gen)));
        }
        if let Some(gen) = self.background.take() {
            return Some(Pick::Job(Job::Gen(gen)));
        }
        if std::mem::take(&mut self.warmup) {
            return Some(Pick::Warmup);
        }
        None
    }
}

/// Moves everything currently queued on `cmd_rx` into `mailbox` without
/// blocking. Returns true once the last sender has hung up — the worker's
/// shutdown signal, which it acts on after draining what it already holds.
fn drain_into(mailbox: &mut Mailbox, cmd_rx: &mpsc::Receiver<CleanupCommand>) -> bool {
    loop {
        match cmd_rx.try_recv() {
            Ok(cmd) => mailbox.push_command(cmd),
            Err(mpsc::TryRecvError::Empty) => return false,
            Err(mpsc::TryRecvError::Disconnected) => return true,
        }
    }
}

/// How an in-flight generation ended. On every one of these the generation
/// thread has already been **joined** — see [`await_generation`].
enum Outcome {
    Finished(anyhow::Result<String>),
    Deadline,
    Preempted,
}

/// Waits for the in-flight generation in [`SLICE`]-sized steps, coming up for
/// air between them to enforce the wall-clock deadline and to drain the
/// mailbox so a more urgent arrival can take the model away.
///
/// Three exits, and **all three join `handle`**: the generation replied; the
/// deadline passed (cancel → [`CANCEL_GRACE`] → join, exactly the pre-
/// scheduler behaviour); or something strictly more urgent than `in_flight`
/// arrived and `preemptible` is set (cancel → [`PREEMPT_GRACE`] → join). The
/// grace periods are courtesies that let the thread hand back its result on
/// its own; the join is unconditional either way (gotcha (e)).
#[allow(clippy::too_many_arguments)]
fn await_generation(
    gen_rx: &mpsc::Receiver<anyhow::Result<String>>,
    handle: std::thread::JoinHandle<()>,
    cancel: &AtomicBool,
    external_cancel: Option<&AtomicBool>,
    deadline: Instant,
    in_flight: Priority,
    preemptible: bool,
    mailbox: &mut Mailbox,
    cmd_rx: &mpsc::Receiver<CleanupCommand>,
    disconnected: &mut bool,
) -> Outcome {
    let outcome = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || external_cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            // Deadline hit: ask the worker to stop, give it a short grace
            // period to notice and send its (now-irrelevant) result, which we
            // discard — then join below.
            cancel.store(true, Ordering::Relaxed);
            let _ = gen_rx.recv_timeout(CANCEL_GRACE);
            break Outcome::Deadline;
        }
        // Never overshoot the deadline by up to a slice: the last wait is
        // whatever is actually left of it.
        match gen_rx.recv_timeout(remaining.min(SLICE)) {
            Ok(result) => break Outcome::Finished(result),
            // The generation thread always sends before it exits, so a
            // disconnected channel means it unwound. Report it as a failed
            // generation rather than waiting for a message that is never
            // coming.
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                break Outcome::Finished(Err(anyhow::anyhow!(
                    "generation thread ended without a result"
                )))
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }

        if drain_into(mailbox, cmd_rx) {
            *disconnected = true;
        }
        // Strictly more urgent only (`Priority`'s Ord is most-urgent-first),
        // so same-priority work never interrupts, and the starvation cap is
        // checked by the caller via `preemptible`.
        if preemptible && matches!(mailbox.best_priority(), Some(p) if p < in_flight) {
            cancel.store(true, Ordering::Relaxed);
            let _ = gen_rx.recv_timeout(PREEMPT_GRACE);
            break Outcome::Preempted;
        }
    };
    // Always wait for the OS thread to actually finish — whether it already
    // sent its result (returns immediately) or was just cancelled (blocks
    // until the in-flight decode call returns and the loop's next
    // cancel-check fires).
    let _ = handle.join();
    outcome
}

/// Spawns the cleanup-lifecycle thread. Runs until `cmd_rx` disconnects.
pub fn spawn(
    model_path: PathBuf,
    idle_timeout: Duration,
    cmd_rx: mpsc::Receiver<CleanupCommand>,
    status_tx: mpsc::Sender<CleanupStatusEvent>,
) -> std::thread::JoinHandle<()> {
    spawn_with_loader(
        Box::new(move || load_provider(&model_path)),
        idle_timeout,
        cmd_rx,
        status_tx,
    )
}

/// The worker itself, with the provider source injected. `spawn` is the only
/// production caller; the tests use this directly with a fake provider.
fn spawn_with_loader(
    mut load: ProviderLoader,
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
            let mut mailbox = Mailbox::default();
            // Sticky: once the last sender is gone the worker finishes what it
            // already holds (so nobody is left blocked on a reply we could
            // still have answered) and then exits.
            let mut disconnected = false;

            loop {
                if drain_into(&mut mailbox, &cmd_rx) {
                    disconnected = true;
                }

                let Some(pick) = mailbox.pick() else {
                    if disconnected {
                        break;
                    }
                    // Nothing to do: block, which is also where the
                    // idle-unload timer lives.
                    match cmd_rx.recv_timeout(idle_timeout) {
                        Ok(cmd) => mailbox.push_command(cmd),
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            if provider.is_some() && last_used.elapsed() >= idle_timeout {
                                provider = None;
                                warmed_up = false;
                                eprintln!("[vzt-flow] cleanup model unloaded after {idle_timeout:?} idle");
                                let _ = status_tx.send(CleanupStatusEvent::Unloaded);
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => disconnected = true,
                    }
                    continue;
                };

                match pick {
                    Pick::Warmup => {
                        // Nothing left to be warm *for* once the last sender
                        // has gone; don't spend seconds of Metal JIT on the
                        // way out.
                        if disconnected {
                            break;
                        }
                        ensure_loaded(&mut provider, &mut load_failed_once, &mut load, &status_tx);
                        if warmed_up {
                            continue;
                        }
                        let Some(p) = provider.clone() else { continue };
                        let started = Instant::now();
                        let cancel = AtomicBool::new(false);
                        let ctx = CleanupContext::default();
                        // Runs inline on this thread rather than through the
                        // preemptible path: warmup is only ever picked when
                        // the mailbox is otherwise empty, and it is the thing
                        // that makes the *next* request fast — interrupting it
                        // would just move the JIT cost onto that request.
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
                    Pick::Job(Job::Clean { raw, mode, ctx, timeout_ms, reply }) => {
                        last_used = Instant::now();

                        if mode == Mode::Raw {
                            let _ = reply.send(CleanupResult { text: raw, used_llm: false });
                            continue;
                        }

                        ensure_loaded(&mut provider, &mut load_failed_once, &mut load, &status_tx);
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

                        let outcome = await_generation(
                            &gen_rx,
                            handle,
                            &cancel,
                            None,
                            Instant::now() + Duration::from_millis(timeout_ms),
                            Priority::Interactive,
                            // A dictation is never preempted: the user is
                            // waiting on it, and nothing outranks Interactive.
                            // Stated explicitly rather than left to follow
                            // from the priority comparison.
                            false,
                            &mut mailbox,
                            &cmd_rx,
                            &mut disconnected,
                        );

                        let (final_text, used_llm, log_msg) = match outcome {
                            Outcome::Finished(Ok(text)) if !text.trim().is_empty() => {
                                (text, true, format!("llm path won ({} mode)", mode.label()))
                            }
                            Outcome::Finished(Ok(_empty)) => (
                                raw.clone(),
                                false,
                                "llm produced no usable output; falling back to raw".to_string(),
                            ),
                            Outcome::Finished(Err(e)) => {
                                (raw.clone(), false, format!("generation failed ({e}); falling back to raw"))
                            }
                            Outcome::Deadline => (
                                raw.clone(),
                                false,
                                format!(
                                    "{timeout_ms}ms deadline exceeded; cancelled generation and \
                                     pasting raw"
                                ),
                            ),
                            // Unreachable while `preemptible` is false above.
                            // Handled as a raw fallback rather than with an
                            // `unreachable!()`, because a panic here would
                            // take the whole manager thread down and the
                            // hotkey would silently stop cleaning up (the
                            // shape of gotcha (i)).
                            Outcome::Preempted => (
                                raw.clone(),
                                false,
                                "generation preempted; falling back to raw".to_string(),
                            ),
                        };
                        eprintln!("[vzt-flow] cleanup: {log_msg}");
                        let _ = reply.send(CleanupResult { text: final_text, used_llm });
                    }
                    Pick::Job(Job::Gen(pending)) => {
                        if pending.req.cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed)) {
                            pending.answer_empty();
                            continue;
                        }
                        last_used = Instant::now();
                        ensure_loaded(&mut provider, &mut load_failed_once, &mut load, &status_tx);
                        let Some(p) = provider.clone() else {
                            let _ = pending.reply.send(Ok(String::new()));
                            continue;
                        };
                        let PendingGen { req, reply, preempt_count } = pending;

                        let cancel = Arc::new(AtomicBool::new(false));
                        let (gen_tx, gen_rx) = mpsc::channel();
                        let system = req.system.clone();
                        let user = req.user.clone();
                        let max_new_tokens = req.max_new_tokens;
                        let cancel_for_gen = cancel.clone();
                        let handle = std::thread::spawn(move || {
                            let result = p.generate_raw(&system, &user, max_new_tokens, &cancel_for_gen);
                            let _ = gen_tx.send(result);
                        });

                        let outcome = await_generation(
                            &gen_rx,
                            handle,
                            &cancel,
                            req.cancel.as_deref(),
                            // The deadline starts now, not when the request
                            // was made: time spent queued or preempted does
                            // not count against it.
                            Instant::now() + Duration::from_millis(req.timeout_ms),
                            req.priority,
                            preempt_count < MAX_PREEMPTIONS,
                            &mut mailbox,
                            &cmd_rx,
                            &mut disconnected,
                        );

                        match outcome {
                            Outcome::Finished(Ok(text)) => {
                                let _ = reply.send(Ok(text));
                            }
                            Outcome::Finished(Err(e)) => {
                                let _ = reply.send(Err(e.to_string()));
                            }
                            Outcome::Deadline => {
                                let _ = reply.send(Ok(String::new()));
                            }
                            Outcome::Preempted => {
                                mailbox.requeue(PendingGen {
                                    req,
                                    reply,
                                    preempt_count: preempt_count + 1,
                                });
                            }
                        }
                    }
                }
            }
        })
        .expect("failed to spawn cleanup manager thread")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{GenRequest, Priority};
    use std::sync::atomic::AtomicUsize;

    /// Increments a counter for as long as a *slow* generation is actually
    /// executing, decrementing on every exit path. The manager must never
    /// move on to the next job while this is non-zero — that is the whole of
    /// gotcha (e): a detached llama.cpp thread keeps a Metal context alive
    /// and crashes the process at exit.
    struct LiveGuard(Arc<AtomicUsize>);

    impl LiveGuard {
        fn new(counter: &Arc<AtomicUsize>) -> Self {
            counter.fetch_add(1, Ordering::SeqCst);
            Self(counter.clone())
        }
    }

    impl Drop for LiveGuard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// Stands in for `LlamaCleanupProvider` in the scheduler tests: no model,
    /// no Metal, but the same cooperative-cancel contract — a generation
    /// sleeps in a loop polling `cancel` between "tokens", and once cancelled
    /// it still takes `cancel_drag` to unwind, the way a llama.cpp `decode`
    /// call already in flight cannot be interrupted mid-token. That drag is
    /// what makes "joined" distinguishable from "detached": set it longer
    /// than `PREEMPT_GRACE` and only a real `join()` keeps the live count at
    /// zero when the manager moves on.
    ///
    /// `live` is per-instance rather than a process-global static because the
    /// test harness runs these tests in parallel threads of one process.
    struct FakeProvider {
        live: Arc<AtomicUsize>,
        /// Inputs containing this marker run slowly; everything else returns
        /// at once, so a test decides exactly which request occupies the
        /// worker.
        slow_marker: &'static str,
        /// How many of the marked generations run slowly before they start
        /// completing immediately — lets a test preempt the same request N
        /// times and then watch it finish.
        slow_calls: usize,
        seen_slow: AtomicUsize,
        slow_for: Duration,
        cancel_drag: Duration,
    }

    impl FakeProvider {
        fn new(slow_marker: &'static str, slow_calls: usize, slow_for: Duration) -> Self {
            Self {
                live: Arc::new(AtomicUsize::new(0)),
                slow_marker,
                slow_calls,
                seen_slow: AtomicUsize::new(0),
                slow_for,
                cancel_drag: Duration::ZERO,
            }
        }

        fn with_cancel_drag(mut self, drag: Duration) -> Self {
            self.cancel_drag = drag;
            self
        }

        fn run(&self, input: &str, cancel: &AtomicBool, out: String) -> anyhow::Result<String> {
            let slow = input.contains(self.slow_marker)
                && self.seen_slow.fetch_add(1, Ordering::SeqCst) < self.slow_calls;
            if !slow {
                return Ok(out);
            }
            // Counted from here rather than from the top of the call, so
            // `live` tracks exactly the generations that can *be* detached —
            // the long, cancellable ones. An instant-return generation would
            // otherwise raise the count for the microseconds between spawn
            // and return, which is long enough to race a test that samples it
            // right after the previous job's reply lands.
            let _live = LiveGuard::new(&self.live);
            let deadline = Instant::now() + self.slow_for;
            while Instant::now() < deadline {
                if cancel.load(Ordering::Relaxed) {
                    // Still "live" for the whole drag — an in-flight decode
                    // does not stop the instant you ask it to.
                    std::thread::sleep(self.cancel_drag);
                    return Ok(String::new());
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            Ok(out)
        }
    }

    impl CleanupProvider for FakeProvider {
        fn clean(
            &self,
            raw: &str,
            _mode: Mode,
            _ctx: &CleanupContext,
            cancel: &AtomicBool,
        ) -> anyhow::Result<String> {
            self.run(raw, cancel, format!("cleaned: {raw}"))
        }

        fn generate_raw(
            &self,
            _system: &str,
            user: &str,
            _max_new_tokens: i32,
            cancel: &AtomicBool,
        ) -> anyhow::Result<String> {
            self.run(user, cancel, format!("generated: {user}"))
        }
    }

    /// A running manager wired to a `FakeProvider`, torn down (sender
    /// dropped, thread joined) when the test ends — including on a panicking
    /// assertion.
    struct Harness {
        cmd_tx: Option<mpsc::Sender<CleanupCommand>>,
        live: Arc<AtomicUsize>,
        _status_rx: mpsc::Receiver<CleanupStatusEvent>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl Harness {
        fn new(fake: FakeProvider) -> Self {
            let live = fake.live.clone();
            let (cmd_tx, cmd_rx) = mpsc::channel();
            let (status_tx, status_rx) = mpsc::channel();
            let mut fake = Some(Box::new(fake) as Box<dyn CleanupProvider>);
            let handle = spawn_with_loader(
                Box::new(move || match fake.take() {
                    Some(p) => Ok(p),
                    None => anyhow::bail!("fake provider already consumed"),
                }),
                // Long enough that the idle-unload path never fires mid-test.
                Duration::from_secs(3600),
                cmd_rx,
                status_tx,
            );
            Self { cmd_tx: Some(cmd_tx), live, _status_rx: status_rx, handle: Some(handle) }
        }

        fn clean(&self, raw: &str, timeout_ms: u64) -> mpsc::Receiver<CleanupResult> {
            let (reply, rx) = mpsc::channel();
            self.send(CleanupCommand::Clean {
                raw: raw.to_string(),
                mode: Mode::Clean,
                ctx: CleanupContext::default(),
                timeout_ms,
                reply,
            });
            rx
        }

        fn generate(
            &self,
            user: &str,
            priority: Priority,
            timeout_ms: u64,
        ) -> mpsc::Receiver<Result<String, String>> {
            let (reply, rx) = mpsc::channel();
            self.send(CleanupCommand::Generate {
                req: GenRequest {
                    system: "sys".into(),
                    user: user.to_string(),
                    max_new_tokens: 90,
                    timeout_ms,
                    priority,
                    cancel: None,
                },
                reply,
            });
            rx
        }

        fn send(&self, cmd: CleanupCommand) {
            self.cmd_tx.as_ref().expect("harness alive").send(cmd).expect("manager thread alive");
        }

        /// Spins until exactly `want` generations are executing, so a test
        /// never races the worker picking a job up off its channel.
        fn wait_until_live(&self, want: usize) {
            let deadline = Instant::now() + Duration::from_secs(10);
            while self.live.load(Ordering::SeqCst) != want {
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for {want} live generation(s); saw {}",
                    self.live.load(Ordering::SeqCst)
                );
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            self.cmd_tx.take();
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    /// The scheduler's reason to exist: a background summary must not make
    /// the user's next dictation wait for it. Fails by timeout against a
    /// single-`recv_timeout` worker, which runs the summary to completion
    /// before it even looks at the `Clean`.
    #[test]
    fn a_dictation_clean_preempts_an_in_flight_background_summary() {
        let h = Harness::new(FakeProvider::new("SUMMARY", 1, Duration::from_secs(3)));

        let summary_rx = h.generate("SUMMARY transcript", Priority::Background, 30_000);
        h.wait_until_live(1);

        let started = Instant::now();
        let clean_rx = h.clean("hello there", 5_000);
        let result = clean_rx
            .recv_timeout(Duration::from_millis(1_500))
            .expect("a dictation cleanup must not wait behind a background summary");
        assert!(started.elapsed() < Duration::from_millis(1_500), "took {:?}", started.elapsed());
        assert_eq!(result.text, "cleaned: hello there");
        assert!(result.used_llm);

        // ...and the displaced summary is requeued, not dropped on the floor.
        let summary = summary_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the preempted summary must be requeued and answered");
        assert_eq!(summary.unwrap(), "generated: SUMMARY transcript");
    }

    /// Gotcha (e). The fake drags for 800ms after being cancelled — longer
    /// than `PREEMPT_GRACE` — so the only way the live count can be zero when
    /// the preempting `Clean` answers is if the manager actually `join()`ed
    /// the preempted thread instead of dropping its handle.
    #[test]
    fn a_preempted_request_is_joined_not_detached() {
        let h = Harness::new(
            FakeProvider::new("SUMMARY", 1, Duration::from_secs(3))
                .with_cancel_drag(Duration::from_millis(800)),
        );
        assert!(Duration::from_millis(800) > PREEMPT_GRACE, "the drag must outlast the grace");

        let summary_rx = h.generate("SUMMARY transcript", Priority::Background, 30_000);
        h.wait_until_live(1);

        let clean_rx = h.clean("hi", 5_000);
        let result = clean_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the preempting dictation must be served");

        assert_eq!(
            h.live.load(Ordering::SeqCst),
            0,
            "a preempted generation was still executing when the manager moved on — its thread \
             was detached, not joined"
        );
        assert_eq!(result.text, "cleaned: hi");
        assert_eq!(
            summary_rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap(),
            "generated: SUMMARY transcript"
        );
    }

    /// The coaching slot is latest-only: a newer tip request replaces a
    /// pending one. The displaced caller is blocked on its reply channel, so
    /// it must be answered with the empty "no usable output" contract rather
    /// than left to hang forever.
    #[test]
    fn a_newer_coaching_request_replaces_a_pending_one_and_the_old_caller_is_answered() {
        let h = Harness::new(FakeProvider::new("LONG", 1, Duration::from_secs(3)));

        // Occupy the worker with an Interactive job: nothing outranks it, so
        // both coaching requests sit in the mailbox rather than running.
        let clean_rx = h.clean("LONG dictation", 30_000);
        h.wait_until_live(1);

        let first_rx = h.generate("tip one", Priority::Coaching, 5_000);
        std::thread::sleep(Duration::from_millis(250)); // drained into the coaching slot

        let sent = Instant::now();
        let second_rx = h.generate("tip two", Priority::Coaching, 5_000);
        let displaced = first_rx
            .recv_timeout(Duration::from_millis(500))
            .expect("a displaced coaching request must be answered, not left to hang");
        assert_eq!(displaced.unwrap(), "");
        // Answered while the 3s dictation is still running — i.e. promptly on
        // displacement, not merely once the worker got around to it.
        assert!(sent.elapsed() < Duration::from_millis(500), "took {:?}", sent.elapsed());

        assert_eq!(
            clean_rx.recv_timeout(Duration::from_secs(10)).unwrap().text,
            "cleaned: LONG dictation"
        );
        assert_eq!(
            second_rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap(),
            "generated: tip two"
        );
    }

    /// Preemption is strictly one-way: only something *more* urgent than the
    /// job in flight may interrupt it. A coaching tip arriving mid-dictation
    /// must leave the dictation's deadline path exactly as it was.
    #[test]
    fn coaching_never_preempts_a_dictation_clean() {
        let h = Harness::new(FakeProvider::new("LONG", 1, Duration::from_millis(1_200)));

        let clean_rx = h.clean("LONG dictation", 30_000);
        h.wait_until_live(1);

        let started = Instant::now();
        let tip_rx = h.generate("tip", Priority::Coaching, 5_000);

        let result = clean_rx.recv_timeout(Duration::from_secs(10)).expect("dictation answered");
        // A cancelled FakeProvider returns "", which the manager reports as
        // the raw fallback with used_llm=false — so full text plus used_llm
        // is proof the generation was never cancelled.
        assert_eq!(result.text, "cleaned: LONG dictation");
        assert!(result.used_llm, "the dictation was cancelled by a lower-priority request");
        assert!(
            started.elapsed() >= Duration::from_millis(900),
            "the dictation returned too fast to have run to completion: {:?}",
            started.elapsed()
        );

        assert_eq!(
            tip_rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap(),
            "generated: tip"
        );
    }

    /// Starvation cap. Two preemptions are allowed; at `MAX_PREEMPTIONS` the
    /// request runs to completion even though a higher-priority job is
    /// waiting, so a steady drip of dictations can never stop a summary from
    /// ever finishing.
    #[test]
    fn a_request_preempted_twice_runs_to_completion_the_third_time() {
        assert_eq!(MAX_PREEMPTIONS, 2, "this test encodes the cap it is testing");
        let h = Harness::new(FakeProvider::new("SUMMARY", 3, Duration::from_millis(1_500)));

        let summary_rx = h.generate("SUMMARY transcript", Priority::Background, 60_000);

        for (n, raw) in ["one", "two"].iter().enumerate() {
            h.wait_until_live(1);
            let clean_rx = h.clean(raw, 5_000);
            clean_rx
                .recv_timeout(Duration::from_secs(5))
                .unwrap_or_else(|e| panic!("preemption {} did not happen: {e}", n + 1));
        }

        // Third attempt: preempt_count is at the cap, so this dictation must
        // wait rather than displace the summary again.
        h.wait_until_live(1);
        let third_rx = h.clean("three", 5_000);

        let summary = summary_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the summary must finish rather than be preempted forever");
        assert_eq!(
            summary.unwrap(),
            "generated: SUMMARY transcript",
            "at MAX_PREEMPTIONS the summary must run to completion, not be cancelled again"
        );
        assert_eq!(
            third_rx.recv_timeout(Duration::from_secs(10)).unwrap().text,
            "cleaned: three"
        );
    }

    #[test]
    fn deadline_for_empty_input_is_the_base_timeout() {
        let cfg = Config::default();
        assert_eq!(cleanup_deadline_ms(0, &cfg), cfg.cleanup_timeout_ms);
    }

    #[test]
    fn deadline_scales_linearly_with_input_length() {
        let cfg = Config::default();
        // A ~40-word dictation (~200 chars): base + 200 * per-char.
        let expected = cfg.cleanup_timeout_ms + 200 * cfg.cleanup_timeout_per_char_ms;
        assert_eq!(cleanup_deadline_ms(200, &cfg), expected);
        assert!(expected < cfg.cleanup_timeout_max_ms, "sanity: short input shouldn't hit the cap");
    }

    #[test]
    fn deadline_is_capped_for_very_long_input() {
        let cfg = Config::default();
        // A ~1500-word ramble (~8000 chars) would blow way past the base
        // formula; the cap must win instead of an unbounded wait.
        assert_eq!(cleanup_deadline_ms(8_000, &cfg), cfg.cleanup_timeout_max_ms);
    }

    #[test]
    fn deadline_never_panics_on_pathological_input() {
        let cfg = Config::default();
        assert_eq!(cleanup_deadline_ms(usize::MAX, &cfg), cfg.cleanup_timeout_max_ms);
    }

    #[test]
    fn default_max_ms_is_a_sane_ceiling() {
        // Never blocks a dictation more than 20s waiting on the LLM.
        let cfg = Config::default();
        assert_eq!(cfg.cleanup_timeout_max_ms, 20_000);
    }
    #[test]
    fn session_cancellation_joins_an_active_generation() {
        let h = Harness::new(FakeProvider::new("COACH", 1, Duration::from_secs(30)));
        let cancel = Arc::new(AtomicBool::new(false));
        let (reply, rx) = mpsc::channel();
        h.send(CleanupCommand::Generate { req: GenRequest {
            system: "sys".into(), user: "COACH question".into(), max_new_tokens: 90,
            timeout_ms: 30_000, priority: Priority::Coaching, cancel: Some(cancel.clone()),
        }, reply });
        h.wait_until_live(1);
        cancel.store(true, Ordering::Relaxed);
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap().unwrap(), "");
        assert_eq!(h.live.load(Ordering::SeqCst), 0, "reply must follow join");
    }

}
