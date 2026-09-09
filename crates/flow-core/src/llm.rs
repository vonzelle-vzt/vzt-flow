//! The single resident-LLM seam.
//!
//! Dictation cleanup, the meeting summary and the interview coach all want
//! the same ~1.1GB Qwen3 model. Loading it more than once is not an option
//! (memory, and llama.cpp's Metal backend is not something you want two
//! copies of), so *every* generation in the process goes through one
//! [`crate::cleanup_manager`] worker, and this module is the caller-facing
//! side of that: a [`TextGenerator`] hands the manager a [`GenRequest`] with
//! a [`Priority`] and blocks for the answer.
//!
//! The manager's scheduler is what makes sharing safe — a 20-second
//! background summary cannot make the user's next dictation wait for it,
//! because a `Priority::Interactive` request preempts one that is already
//! running (see `cleanup_manager`'s mailbox docs).
//!
//! # The empty-string contract
//!
//! `Ok(String::new())` means "no usable output" — the same contract
//! [`crate::cleanup::CleanupProvider::clean`] already uses, so every caller's
//! existing empty-check is the fallback path. A *dead* manager (channel
//! closed, model never loaded, request displaced by a newer one) therefore
//! degrades to empty rather than to `Err`: callers of a best-effort feature
//! must degrade, not fail. `Err` is reserved for a generation that actually
//! ran and failed.

use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Result;

use crate::cleanup_manager::{self, CleanupCommand, CleanupStatusEvent};

/// Scheduling class of a generation request. `Ord` is the priority order —
/// `Interactive < Coaching < Background`, i.e. *lower is more urgent* — and
/// the manager's scheduler compares with `<` to decide whether an arriving
/// request may preempt the one in flight.
///
/// - `Interactive`: the user is waiting with a cursor blinking. Dictation
///   cleanup. Never preempted, never queued behind anything.
/// - `Coaching`: a live interview tip. Worth interrupting a summary for;
///   latest-only (a newer question makes an older pending tip pointless).
/// - `Background`: the end-of-meeting summary. Runs when nothing else wants
///   the model, and yields when something does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    Interactive,
    Coaching,
    Background,
}

/// One free-form generation, prompt and budget included.
///
/// `max_new_tokens` is explicit rather than derived from the input length
/// (which is what dictation cleanup does): a coaching tip is ~90 tokens
/// however long the transcript behind it is, and it must not be budgeted —
/// or timed — as if it were a full cleanup pass.
#[derive(Debug, Clone)]
pub struct GenRequest {
    pub system: String,
    pub user: String,
    pub max_new_tokens: i32,
    /// Wall-clock deadline for this request once it starts running. Time
    /// spent queued or preempted does not count against it.
    pub timeout_ms: u64,
    pub priority: Priority,
}

/// Anything that can turn a [`GenRequest`] into text. Implementations must be
/// shareable across threads: the meeting session thread, the coach and the
/// desktop coordinator all hold the same `Arc<dyn TextGenerator>`.
pub trait TextGenerator: Send + Sync {
    fn generate(&self, req: GenRequest) -> Result<String>;
}

/// Generates through an already-running [`cleanup_manager`] — the desktop
/// case, where the coordinator owns the manager and hands out its sender.
///
/// The `Sender` lives behind a `Mutex` because `mpsc::Sender` is `Send` but
/// not `Sync`, and `TextGenerator` requires `Sync`. The lock is released
/// before the reply is awaited, so a caller blocked on a slow generation
/// never stops another caller from *enqueuing* a more urgent one — which is
/// exactly what preemption depends on.
pub struct ManagerGenerator {
    tx: Mutex<mpsc::Sender<CleanupCommand>>,
}

impl ManagerGenerator {
    pub fn new(tx: mpsc::Sender<CleanupCommand>) -> Self {
        Self { tx: Mutex::new(tx) }
    }
}

impl TextGenerator for ManagerGenerator {
    fn generate(&self, req: GenRequest) -> Result<String> {
        let (reply_tx, reply_rx) = mpsc::channel();
        {
            // A poisoned lock still holds a perfectly usable Sender — recover
            // rather than panic, so one panicking caller can't take the
            // generator down for every other feature (cf. gotcha (i)).
            let tx = self.tx.lock().unwrap_or_else(|p| p.into_inner());
            if tx.send(CleanupCommand::Generate { req, reply: reply_tx }).is_err() {
                // Manager gone (never spawned, or shutting down).
                return Ok(String::new());
            }
        }
        recv_reply(reply_rx)
    }
}

/// Generates through a [`cleanup_manager`] this generator spawns itself, on
/// first use — the CLI case, where nothing else in the process owns a
/// manager yet.
///
/// The point is that the CLI and the desktop end up with *exactly one*
/// resident model each, differing only in who spawned the manager; a
/// `LocalGenerator` is never created alongside a `ManagerGenerator` in the
/// same process.
pub struct LocalGenerator {
    model_path: PathBuf,
    idle_timeout: Duration,
    inner: Mutex<Option<Manager>>,
}

struct Manager {
    tx: mpsc::Sender<CleanupCommand>,
    handle: std::thread::JoinHandle<()>,
}

impl LocalGenerator {
    pub fn new(model_path: PathBuf, idle_timeout: Duration) -> Self {
        Self { model_path, idle_timeout, inner: Mutex::new(None) }
    }

    /// Returns a sender for the manager, spawning it if this is the first
    /// call. Cloned out so the caller can drop the lock before blocking.
    fn sender(&self) -> mpsc::Sender<CleanupCommand> {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if guard.is_none() {
            let (cmd_tx, cmd_rx) = mpsc::channel();
            let (status_tx, status_rx) = mpsc::channel::<CleanupStatusEvent>();
            // Nothing in a CLI process consumes load/unload events; dropping
            // the receiver just makes the manager's `let _ = status_tx.send`
            // calls no-ops.
            drop(status_rx);
            let handle = cleanup_manager::spawn(
                self.model_path.clone(),
                self.idle_timeout,
                cmd_rx,
                status_tx,
            );
            *guard = Some(Manager { tx: cmd_tx, handle });
        }
        guard.as_ref().expect("just populated").tx.clone()
    }
}

impl TextGenerator for LocalGenerator {
    fn generate(&self, req: GenRequest) -> Result<String> {
        let tx = self.sender();
        let (reply_tx, reply_rx) = mpsc::channel();
        if tx.send(CleanupCommand::Generate { req, reply: reply_tx }).is_err() {
            return Ok(String::new());
        }
        recv_reply(reply_rx)
    }
}

impl Drop for LocalGenerator {
    /// Drops the command sender (which disconnects the manager's receiver and
    /// ends its loop) and then **joins** the manager thread, so the process
    /// never exits with a thread that still owns a live llama.cpp/Metal
    /// context — the same never-detach discipline the manager applies to its
    /// own generation threads (gotcha (e)).
    fn drop(&mut self) {
        let manager = self.inner.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(Manager { tx, handle }) = manager {
            drop(tx);
            let _ = handle.join();
        }
    }
}

/// Always "no usable output". For tests, and for a machine with no cleanup
/// model installed — the features that use a generator are all best-effort,
/// so they degrade to their no-LLM shape rather than erroring.
pub struct NullGenerator;

impl TextGenerator for NullGenerator {
    fn generate(&self, _req: GenRequest) -> Result<String> {
        Ok(String::new())
    }
}

/// Blocks for the manager's answer. A closed channel means the manager died
/// (or the request was dropped) without answering — degrade to empty, never
/// `Err`, and never block forever: the manager answers or drops every reply
/// channel it takes, and a dropped sender wakes this `recv`.
fn recv_reply(reply_rx: mpsc::Receiver<Result<String, String>>) -> Result<String> {
    match reply_rx.recv() {
        Ok(Ok(text)) => Ok(text),
        Ok(Err(e)) => Err(anyhow::anyhow!(e)),
        Err(_) => Ok(String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(priority: Priority) -> GenRequest {
        GenRequest {
            system: "sys".into(),
            user: "usr".into(),
            max_new_tokens: 90,
            timeout_ms: 1_000,
            priority,
        }
    }

    #[test]
    fn priority_orders_interactive_first_and_background_last() {
        assert!(Priority::Interactive < Priority::Coaching);
        assert!(Priority::Coaching < Priority::Background);
        let mut all = [Priority::Background, Priority::Interactive, Priority::Coaching];
        all.sort();
        assert_eq!(all, [Priority::Interactive, Priority::Coaching, Priority::Background]);
    }

    #[test]
    fn null_generator_returns_the_no_usable_output_contract() {
        assert_eq!(NullGenerator.generate(req(Priority::Coaching)).unwrap(), "");
    }

    #[test]
    fn manager_generator_round_trips_a_reply() {
        let (tx, rx) = mpsc::channel();
        let stub = std::thread::spawn(move || {
            while let Ok(cmd) = rx.recv() {
                if let CleanupCommand::Generate { req, reply } = cmd {
                    let _ = reply.send(Ok(format!("answered {}", req.user)));
                }
            }
        });
        let gen = ManagerGenerator::new(tx);
        assert_eq!(gen.generate(req(Priority::Background)).unwrap(), "answered usr");
        drop(gen);
        stub.join().unwrap();
    }

    #[test]
    fn manager_generator_propagates_a_real_generation_failure() {
        let (tx, rx) = mpsc::channel();
        let stub = std::thread::spawn(move || {
            while let Ok(cmd) = rx.recv() {
                if let CleanupCommand::Generate { reply, .. } = cmd {
                    let _ = reply.send(Err("decode step failed".into()));
                }
            }
        });
        let gen = ManagerGenerator::new(tx);
        let err = gen.generate(req(Priority::Coaching)).unwrap_err();
        assert!(err.to_string().contains("decode step failed"), "{err}");
        drop(gen);
        stub.join().unwrap();
    }

    #[test]
    fn a_dead_manager_degrades_to_empty_rather_than_erroring() {
        // Receiver dropped: the send itself fails.
        let (tx, rx) = mpsc::channel();
        drop(rx);
        assert_eq!(ManagerGenerator::new(tx).generate(req(Priority::Background)).unwrap(), "");

        // Manager takes the command and then dies without answering: the
        // reply channel closes and we must wake up, not hang.
        let (tx, rx) = mpsc::channel();
        let stub = std::thread::spawn(move || {
            let _swallowed = rx.recv();
        });
        assert_eq!(ManagerGenerator::new(tx).generate(req(Priority::Coaching)).unwrap(), "");
        stub.join().unwrap();
    }
}
