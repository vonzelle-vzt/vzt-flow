//! Local interview coach: watches the `Them` transcript stream for
//! interview-style questions and, on a match, generates a short coaching tip
//! grounded in the candidate's own context (resume/talking points).
//!
//! # Shape
//!
//! [`Coach::observe`] is called from the transcription worker for *every*
//! recognized line and must never block it — recognizing a question is a
//! handful of string comparisons, and the generation itself happens on the
//! coach's own thread (`vzt-flow-interview-coach`).
//!
//! # Latest-only, and what "cancel" actually means here
//!
//! A newer question makes an older pending tip pointless: by the time it
//! arrives the interviewer has moved on and the candidate is already
//! answering something else. Superseding therefore happens at three layers,
//! and only the middle one is a real interrupt:
//!
//! 1. **The coach's 1-slot mailbox.** [`Coach::observe`] overwrites
//!    `pending` rather than queueing, so a question that never started
//!    simply disappears.
//! 2. **The scheduler's latest-only `Coaching` slot.**
//!    [`crate::llm::Priority::Coaching`] requests get one slot in
//!    `cleanup_manager`'s mailbox; filing a newer one *displaces* the
//!    pending older one, whose caller is answered `Ok("")` (see
//!    `cleanup_manager::Mailbox::file`/`requeue`). That is the actual
//!    cancellation of work that has been handed to the model but not yet
//!    started, or that was preempted mid-flight.
//! 3. **The epoch check on the way out.** [`crate::llm::GenRequest`] carries
//!    no cancel flag — the manager owns the `AtomicBool` it hands to
//!    `generate_raw` and only trips it on *its* deadline — so a generation
//!    that is already running to completion cannot be interrupted from here.
//!    The coach instead stamps every question with an epoch and drops any
//!    result whose epoch is behind the latest observed question (or behind a
//!    [`Coach::clear`]). The tip is computed and thrown away; nothing stale
//!    ever reaches `on_tip`.
//!
//! # Prompt ordering
//!
//! The task instruction is the **last** thing in the system prompt, for the
//! reason spelled out in CLAUDE.md gotcha (l): whatever trails the prompt is
//! what a greedy small model recites when it has nothing better to say. The
//! candidate context therefore leads, and [`crate::cleanup::is_context_echo`]
//! is the backstop for the cases ordering doesn't fix.

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use super::events::{InterviewTip, TranscriptLine};
use super::transcriber::Source;
use crate::cleanup::{is_context_echo, strip_think_block};
use crate::llm::{GenRequest, Priority, TextGenerator};

/// Filename of the candidate context inside the config directory.
const CONTEXT_FILENAME: &str = "interview.md";

/// Below this many characters a line is backchannel ("Right?", "Ok?",
/// "Mhm, yeah?") — never a question worth spending the model on.
const MIN_QUESTION_CHARS: usize = 15;

/// Words that open a question when they lead the line. Matched
/// case-insensitively against the first word only.
const INTERROGATIVE_LEADS: &[&str] = &[
    "what", "why", "how", "when", "where", "who", "which", "can", "could", "would", "will", "do",
    "does", "did", "are", "is", "have", "has", "tell", "walk", "describe", "explain", "give",
    "talk",
];

/// Phrases that make a line a prompt regardless of where they appear or how
/// it is punctuated — an ASR transcript often loses the question mark.
const PROMPT_PHRASES: &[&str] = &[
    "tell me about",
    "walk me through",
    "how would you",
    "what would you",
    "give me an example",
    "describe a time",
    "why do you",
    "talk me through",
];

/// Bullet markers accepted by [`parse_tip`]. The prompt asks for `- `; a 1.7B
/// model drifts to `*` and `•` often enough that rejecting those would throw
/// away otherwise perfect tips.
const BULLET_MARKERS: &[&str] = &["- ", "* ", "• ", "– "];

/// Exactly how many bullets a well-formed tip has.
const TIP_BULLETS: usize = 3;

/// Path to the candidate context file, `~/.config/vzt-flow/interview.md`.
pub fn context_path() -> anyhow::Result<PathBuf> {
    Ok(crate::config::config_dir()?.join(CONTEXT_FILENAME))
}

/// Reads the candidate context file, or `""` when absent or unreadable —
/// coaching without context is degraded, not broken, so this never errors.
pub fn load_context() -> String {
    context_path()
        .ok()
        .and_then(|p| fs::read_to_string(p).ok())
        .unwrap_or_default()
}

/// Writes the candidate context file atomically (exclusive temp + rename), so
/// a Settings window saving while a session reads never sees a half file.
pub fn save_context(text: &str) -> anyhow::Result<()> {
    use anyhow::Context as _;

    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = context_path()?;
    let dir = path
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;

    let (tmp, mut file) = loop {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let seq = NEXT.fetch_add(1, Ordering::Relaxed);
        let tmp = dir.join(format!(
            ".interview.{}.{nanos}.{seq}.tmp",
            std::process::id()
        ));
        match OpenOptions::new().write(true).create_new(true).open(&tmp) {
            Ok(file) => break (tmp, file),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("failed to create {}", tmp.display())),
        }
    };
    let result = (|| -> std::io::Result<()> {
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, &path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.with_context(|| format!("failed to atomically save {}", path.display()))
}

/// Truncates `text` to `max_chars`, keeping the **head**.
///
/// The head is where a resume puts the current role and the job description
/// puts the requirements; a tail-truncation would keep the hobbies. Counts
/// `char`s, not bytes, so a context with non-ASCII in it can't be cut mid
/// code point.
pub fn truncate_context(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    text.chars().take(max_chars).collect()
}

/// Whether `text` reads as an interview question worth coaching on.
///
/// Three rules, any of which is sufficient, all case-insensitive and all
/// gated behind a [`MIN_QUESTION_CHARS`] floor:
///
/// - ends with `?` and has ≥4 words — plain punctuated question;
/// - opens with an interrogative lead and has ≥5 words — covers the very
///   common case of ASR dropping the question mark;
/// - contains a prompt phrase ("tell me about", "walk me through", …) —
///   these are imperative, not interrogative, and match neither rule above.
pub fn looks_like_question(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.chars().count() < MIN_QUESTION_CHARS {
        return false;
    }
    let lower = trimmed.to_lowercase();
    let words = lower.split_whitespace().count();

    if trimmed.ends_with('?') && words >= 4 {
        return true;
    }

    let first = lower
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_matches(|c: char| !c.is_alphanumeric());
    if words >= 5 && INTERROGATIVE_LEADS.contains(&first) {
        return true;
    }

    PROMPT_PHRASES.iter().any(|p| lower.contains(p))
}

/// Builds the coach's system prompt from the (already truncated) candidate
/// context.
///
/// The task instruction is deliberately **last** — gotcha (l). An empty
/// context omits the whole block rather than emitting a dangling heading,
/// which would otherwise be the nearest thing to "recent text" for the model
/// to copy.
pub fn build_coach_system_prompt(context: &str) -> String {
    let mut prompt = String::from(
        "You are a live interview coach. The candidate is answering questions in a job interview.\n",
    );
    let ctx = context.trim();
    if !ctx.is_empty() {
        prompt.push_str("\nCANDIDATE CONTEXT (resume, job description, talking points):\n");
        prompt.push_str(ctx);
        prompt.push('\n');
    }
    prompt.push_str(
        "\nReply with exactly four lines:\n\
         line 1: a 6-10 word restatement of what the question is really asking.\n\
         lines 2-4: three bullets starting with \"- \", each at most 12 words, naming a specific\n\
         fact from the candidate context wherever one applies.\n\
         Under 60 words total. No preamble, no headings, no closing remark.\n\
         Output only those four lines.",
    );
    prompt
}

/// Builds the coach's user turn: the recent transcript lines for context, the
/// triggering question under a `QUESTION:` label so the model can't confuse
/// it with the history, then the same Qwen3 `/no_think` suppression
/// `cleanup::clean` uses (`cleanup.rs:364`).
pub fn build_coach_user_turn(recent: &[TranscriptLine], question: &str) -> String {
    let mut turn = String::new();
    for line in recent {
        let text = line.text.trim();
        if text.is_empty() {
            continue;
        }
        turn.push_str(line.source.label());
        turn.push_str(": ");
        turn.push_str(text);
        turn.push('\n');
    }
    if !turn.is_empty() {
        turn.push('\n');
    }
    turn.push_str("QUESTION: ");
    turn.push_str(question.trim());
    turn.push_str(" /no_think");
    turn
}

/// Parses a raw model reply into `(headline, bullets)`.
///
/// A well-formed tip is one non-bullet headline line followed by exactly
/// [`TIP_BULLETS`] bullets. Anything else — no bullets, two bullets, five
/// bullets, bullets with no headline — is `None`, and the caller drops the
/// tip rather than showing the user half of one.
pub fn parse_tip(raw: &str) -> Option<(String, Vec<String>)> {
    let text = strip_think_block(raw);
    let mut headline: Option<String> = None;
    let mut bullets: Vec<String> = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match strip_bullet(line) {
            Some(bullet) if !bullet.is_empty() => bullets.push(bullet.to_string()),
            Some(_) => {}
            None => {
                // Only the first non-bullet line is a candidate headline;
                // anything after the bullets started is the "closing remark"
                // the prompt asked for and didn't get, and is ignored.
                if headline.is_none() && bullets.is_empty() {
                    let cleaned = clean_headline(line);
                    if !cleaned.is_empty() {
                        headline = Some(cleaned);
                    }
                }
            }
        }
    }

    let headline = headline?;
    if bullets.len() != TIP_BULLETS {
        return None;
    }
    Some((headline, bullets))
}

/// The text after a bullet marker, or `None` when the line isn't a bullet.
fn strip_bullet(line: &str) -> Option<&str> {
    BULLET_MARKERS
        .iter()
        .find_map(|m| line.strip_prefix(m))
        .map(str::trim)
}

/// Strips the markdown a small model sprinkles on a headline it was told not
/// to decorate.
fn clean_headline(line: &str) -> String {
    line.trim()
        .trim_start_matches('#')
        .trim()
        .trim_matches('*')
        .trim()
        .to_string()
}

/// Budgets for one coaching tip.
///
/// `max_new_tokens` is small and fixed because a tip is four short lines
/// however long the meeting behind it is, and `context_max_chars` is well
/// under what the model could accept because *prefill is on the critical
/// path* for a 3-second target — every extra 1000 characters of resume is
/// latency the candidate pays on every single question.
#[derive(Debug, Clone, Copy)]
pub struct CoachConfig {
    pub timeout_ms: u64,
    pub max_new_tokens: i32,
    pub context_max_chars: usize,
    pub history_them: usize,
    pub history_me: usize,
}

impl Default for CoachConfig {
    fn default() -> Self {
        Self {
            timeout_ms: 3500,
            max_new_tokens: 90,
            context_max_chars: 4800,
            history_them: 3,
            history_me: 2,
        }
    }
}

/// A question waiting for (or occupying) the worker.
struct PendingQuestion {
    line: TranscriptLine,
    /// When the audio chunk that produced this line was closed. Tip latency
    /// is measured from here, not from when generation started — what the
    /// candidate experiences is the gap between the interviewer finishing the
    /// question and the tip appearing.
    chunk_closed_at: Instant,
    /// Transcript lines that preceded this question, oldest first.
    recent: Vec<TranscriptLine>,
    epoch: u64,
}

/// The 1-slot mailbox plus the bounded history rings, all under one lock.
struct Mailbox {
    pending: Option<PendingQuestion>,
    /// Bumped by every accepted question and by [`Coach::clear`]. A result
    /// whose epoch is behind this is stale — see the module docs.
    epoch: u64,
    /// Seq of the most recent accepted question, so a cleared tip can be
    /// attributed to the question it is wiping.
    last_seq: u64,
    them: Vec<TranscriptLine>,
    me: Vec<TranscriptLine>,
    stop: bool,
}

struct Shared {
    mailbox: Mutex<Mailbox>,
    wake: Condvar,
    /// 1 while the worker thread is running. Test-only: it is how
    /// `shutdown_joins_the_worker` proves the join really happened rather
    /// than the worker being detached and outliving the `Coach`. The worker
    /// itself holds its own clone, so nothing in a release build reads this.
    #[cfg(test)]
    live: Arc<AtomicUsize>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Mailbox> {
        // A panicking `on_tip` observer must not permanently disable
        // coaching — same reasoning as gotcha (i) in the desktop crate.
        self.mailbox.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// Watches the transcript for interview questions and emits at most one tip
/// per question, latest-only.
pub struct Coach {
    shared: Arc<Shared>,
    worker: Option<std::thread::JoinHandle<()>>,
    /// Read-only after `spawn`; the worker holds its own clones of these.
    session_id: String,
    enabled: Arc<AtomicBool>,
    on_tip: Arc<dyn Fn(&InterviewTip) + Send + Sync>,
    history_them: usize,
    history_me: usize,
}

impl Coach {
    /// Spawns the coach worker. The candidate `context` is truncated and
    /// baked into the system prompt once, here, rather than per question.
    pub fn spawn(
        session_id: String,
        gen: Arc<dyn TextGenerator>,
        context: String,
        enabled: Arc<AtomicBool>,
        cfg: CoachConfig,
        on_tip: Arc<dyn Fn(&InterviewTip) + Send + Sync>,
    ) -> Coach {
        let context = truncate_context(&context, cfg.context_max_chars);
        let system = build_coach_system_prompt(&context);
        let live = Arc::new(AtomicUsize::new(0));
        let shared = Arc::new(Shared {
            mailbox: Mutex::new(Mailbox {
                pending: None,
                epoch: 0,
                last_seq: 0,
                them: Vec::new(),
                me: Vec::new(),
                stop: false,
            }),
            wake: Condvar::new(),
            #[cfg(test)]
            live: Arc::clone(&live),
        });

        let worker_shared = Arc::clone(&shared);
        let worker_session_id = session_id.clone();
        let worker_enabled = Arc::clone(&enabled);
        let worker_on_tip = Arc::clone(&on_tip);
        let worker = std::thread::Builder::new()
            .name("vzt-flow-interview-coach".to_string())
            .spawn(move || {
                let _live = LiveGuard::enter(&live);
                run_worker(
                    worker_shared,
                    gen,
                    worker_session_id,
                    system,
                    context,
                    worker_enabled,
                    cfg,
                    worker_on_tip,
                );
            })
            .expect("failed to spawn interview coach thread");

        Coach {
            shared,
            worker: Some(worker),
            session_id,
            enabled,
            on_tip,
            history_them: cfg.history_them,
            history_me: cfg.history_me,
        }
    }

    /// Feeds one recognized transcript line to the coach.
    ///
    /// Every line updates the history rings; only a `Them` line that
    /// [`looks_like_question`] (and only while `enabled`) files a question,
    /// displacing whatever was pending.
    ///
    /// `chunk_closed_at` is stamped by the caller when it dequeued the audio
    /// chunk, so the reported latency covers transcription too.
    pub fn observe(&self, line: &TranscriptLine, chunk_closed_at: Instant) {
        let mut mb = self.shared.lock();

        // Snapshot the history *before* adding this line, so a question is
        // never repeated back to the model as its own context.
        let recent = merged_history(&mb);

        let (ring, cap) = match line.source {
            Source::Them => (&mut mb.them, self.history_them),
            Source::Me => (&mut mb.me, self.history_me),
        };
        if cap > 0 {
            ring.push(line.clone());
            let overflow = ring.len().saturating_sub(cap);
            if overflow > 0 {
                ring.drain(..overflow);
            }
        }

        if line.source != Source::Them
            || !self.enabled.load(Ordering::Relaxed)
            || !looks_like_question(&line.text)
        {
            return;
        }

        mb.epoch += 1;
        mb.last_seq = line.seq;
        let epoch = mb.epoch;
        mb.pending = Some(PendingQuestion {
            line: line.clone(),
            chunk_closed_at,
            recent,
            epoch,
        });
        drop(mb);
        self.shared.wake.notify_all();
    }

    /// Drops any pending question, invalidates whatever is in flight, and
    /// emits one tip with **empty bullets** — the frontend's signal to clear
    /// its panel. Used when interview mode is switched off mid-session.
    pub fn clear(&self) {
        let seq = {
            let mut mb = self.shared.lock();
            mb.epoch += 1;
            mb.pending = None;
            mb.last_seq
        };
        let tip = InterviewTip {
            session_id: self.session_id.clone(),
            seq,
            question: String::new(),
            question_offset_secs: 0.0,
            headline: String::new(),
            bullets: Vec::new(),
            latency_ms: 0,
        };
        emit(&self.on_tip, &tip);
    }

    /// Stops the worker and **joins** it. Never detached: the worker can be
    /// blocked inside `generate`, and a detached thread that outlives the
    /// session would keep a llama.cpp/Metal generation alive with nobody left
    /// to read it (gotcha (e)).
    pub fn shutdown(mut self) {
        self.stop_and_join();
    }

    fn stop_and_join(&mut self) {
        {
            let mut mb = self.shared.lock();
            mb.stop = true;
            mb.pending = None;
        }
        self.shared.wake.notify_all();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for Coach {
    /// Belt and braces for the same never-detach rule: dropping a `Coach`
    /// without calling [`Coach::shutdown`] still stops and joins the worker.
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

/// Increments a liveness counter for exactly as long as the worker runs,
/// decrementing on every exit path including an unwind.
struct LiveGuard(Arc<AtomicUsize>);

impl LiveGuard {
    fn enter(counter: &Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        LiveGuard(Arc::clone(counter))
    }
}

impl Drop for LiveGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The history rings merged into one oldest-first slice. Both rings are in
/// arrival order and `seq` is monotonic per session, so a merge by `seq`
/// restores the order the two speakers actually spoke in.
fn merged_history(mb: &Mailbox) -> Vec<TranscriptLine> {
    let mut lines: Vec<TranscriptLine> = mb.them.iter().chain(mb.me.iter()).cloned().collect();
    lines.sort_by_key(|l| l.seq);
    lines
}

/// Calls an observer without letting its panic take the coach down.
fn emit(on_tip: &Arc<dyn Fn(&InterviewTip) + Send + Sync>, tip: &InterviewTip) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| on_tip(tip)));
    if result.is_err() {
        eprintln!("[vzt-flow] interview: tip observer panicked; coaching continues");
    }
}

#[allow(clippy::too_many_arguments)]
fn run_worker(
    shared: Arc<Shared>,
    gen: Arc<dyn TextGenerator>,
    session_id: String,
    system: String,
    context: String,
    enabled: Arc<AtomicBool>,
    cfg: CoachConfig,
    on_tip: Arc<dyn Fn(&InterviewTip) + Send + Sync>,
) {
    loop {
        // Wait for a question (or for shutdown).
        let pending = {
            let mut mb = shared.lock();
            loop {
                if mb.stop {
                    return;
                }
                if let Some(pending) = mb.pending.take() {
                    break pending;
                }
                mb = shared
                    .wake
                    .wait(mb)
                    .unwrap_or_else(|p| p.into_inner());
            }
        };

        // Cheap pre-check: a question superseded before we ever reached it.
        if !is_current(&shared, pending.epoch) || !enabled.load(Ordering::Relaxed) {
            continue;
        }

        let user = build_coach_user_turn(&pending.recent, &pending.line.text);
        let generated = gen.generate(GenRequest {
            system: system.clone(),
            user,
            max_new_tokens: cfg.max_new_tokens,
            timeout_ms: cfg.timeout_ms,
            priority: Priority::Coaching,
        });

        // The only cancellation available for a generation that actually ran
        // to completion: throw the answer away. A newer question (or a
        // `clear`) has already moved the epoch on.
        if !is_current(&shared, pending.epoch) {
            continue;
        }
        if !enabled.load(Ordering::Relaxed) {
            continue;
        }

        let raw = match generated {
            Ok(raw) => raw,
            Err(e) => {
                eprintln!("[vzt-flow] interview: tip generation failed ({e}); no tip");
                continue;
            }
        };

        // Empty is the manager's "no usable output" contract — a displaced
        // request, a deadline, or no model at all. All three mean no tip.
        let text = strip_think_block(&raw);
        if text.trim().is_empty() {
            continue;
        }

        // Gotcha (l)'s backstop: a model with nothing useful to add recites
        // the most recent thing in its context, which here is the candidate's
        // own resume.
        if is_context_echo(&text, &context) {
            eprintln!(
                "[vzt-flow] interview: tip recited the candidate context ({} chars); discarding",
                text.len()
            );
            continue;
        }

        let Some((headline, bullets)) = parse_tip(&text) else {
            eprintln!("[vzt-flow] interview: tip was not four well-formed lines; discarding");
            continue;
        };

        let tip = InterviewTip {
            session_id: session_id.clone(),
            seq: pending.line.seq,
            question: pending.line.text.clone(),
            question_offset_secs: pending.line.offset_secs,
            headline,
            bullets,
            latency_ms: pending.chunk_closed_at.elapsed().as_millis() as u64,
        };
        emit(&on_tip, &tip);
    }
}

/// Whether `epoch` is still the latest question the coach has seen.
fn is_current(shared: &Arc<Shared>, epoch: u64) -> bool {
    shared.lock().epoch == epoch
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    // ---- pure helpers -------------------------------------------------

    #[test]
    fn looks_like_question_accepts_real_interview_prompts() {
        let positives = [
            "Tell me about a time you had to make a difficult technical decision.",
            "Walk me through your architecture.",
            "How would you scale that?",
            "What was the hardest bug you ever shipped a fix for?",
            "Why did you choose Rust for the audio pipeline?",
            "Can you describe your testing strategy in detail?",
            "Describe a time you disagreed with your manager.",
            "Give me an example of a tradeoff you made.",
            "What would you do differently next time?",
            "Talk me through the incident from last quarter.",
            "Where do you see the product going in a year?",
            "Who owned the release process on that team?",
        ];
        assert_eq!(positives.len(), 12);
        for p in positives {
            assert!(looks_like_question(p), "should be a question: {p:?}");
        }
        // The lead rule must be case-insensitive.
        assert!(looks_like_question("HOW DO YOU HANDLE BACKPRESSURE THERE"));
    }

    #[test]
    fn looks_like_question_rejects_backchannel_and_statements() {
        for n in ["Right?", "Yeah.", "So we shipped it last quarter.", "Ok?", ""] {
            assert!(!looks_like_question(n), "should not be a question: {n:?}");
        }
        // Long enough, ends with '?', but too few words to be a question.
        assert!(!looks_like_question("Interesting, right?"));
        // Long enough, opens with a lead, but under the 5-word floor.
        assert!(!looks_like_question("Is that right there"));
    }

    #[test]
    fn context_is_truncated_from_the_head_at_the_configured_limit() {
        let text = "HEAD marker. ".to_string() + &"x".repeat(10_000) + " TAIL marker.";
        let cut = truncate_context(&text, 4800);
        assert_eq!(cut.chars().count(), 4800);
        assert!(cut.starts_with("HEAD marker."), "kept the head");
        assert!(!cut.contains("TAIL marker"), "dropped the tail");
        // Shorter than the limit is returned untouched.
        assert_eq!(truncate_context("short", 4800), "short");
        // Multi-byte input is cut on a char boundary, not a byte one.
        let emoji = "é".repeat(50);
        assert_eq!(truncate_context(&emoji, 10).chars().count(), 10);
    }

    #[test]
    fn system_prompt_leads_with_the_context_and_trails_with_the_task() {
        let prompt = build_coach_system_prompt("Staff engineer at Acme.");
        let ctx_at = prompt.find("Staff engineer at Acme.").expect("context present");
        let task_at = prompt.find("Reply with exactly four lines:").expect("task present");
        assert!(ctx_at < task_at, "context must lead, task must trail (gotcha (l))");
        assert!(
            prompt.trim_end().ends_with("Output only those four lines."),
            "task instruction must be last: {prompt:?}"
        );
        // No dangling heading when there is no context.
        assert!(!build_coach_system_prompt("   ").contains("CANDIDATE CONTEXT"));
    }

    #[test]
    fn user_turn_carries_recent_lines_then_the_question_then_no_think() {
        let recent = vec![line(1, Source::Them, "So you joined in 2021."), line(2, Source::Me, "That is right.")];
        let turn = build_coach_user_turn(&recent, "  What did you own there?  ");
        assert_eq!(
            turn,
            "Them: So you joined in 2021.\nMe: That is right.\n\nQUESTION: What did you own there? /no_think"
        );
        assert_eq!(
            build_coach_user_turn(&[], "Why Rust?"),
            "QUESTION: Why Rust? /no_think"
        );
    }

    #[test]
    fn parse_tip_extracts_a_headline_and_three_bullets() {
        let raw = "<think>\n\n</think>\n\nThey want your decision-making under ambiguity\n\
                   - Name the Acme migration you led in 2023\n\
                   - Give the number: 12k events per second\n\
                   - Close with what you would change now\n";
        let (headline, bullets) = parse_tip(raw).expect("well-formed tip");
        assert_eq!(headline, "They want your decision-making under ambiguity");
        assert_eq!(bullets.len(), 3);
        assert_eq!(bullets[0], "Name the Acme migration you led in 2023");
        assert_eq!(bullets[2], "Close with what you would change now");

        // Markdown decoration on the headline and a drifting bullet marker.
        let messy = "**What they are really asking**\n* one\n• two\n- three\nHope that helps!";
        let (headline, bullets) = parse_tip(messy).expect("lenient about markers");
        assert_eq!(headline, "What they are really asking");
        assert_eq!(bullets, vec!["one", "two", "three"]);
    }

    #[test]
    fn parse_tip_returns_none_for_malformed_output() {
        // No bullets at all.
        assert!(parse_tip("Just a sentence with no structure.").is_none());
        // Too few bullets.
        assert!(parse_tip("Headline\n- one\n- two").is_none());
        // Too many.
        assert!(parse_tip("Headline\n- one\n- two\n- three\n- four").is_none());
        // Bullets with no headline.
        assert!(parse_tip("- one\n- two\n- three").is_none());
        // Nothing at all.
        assert!(parse_tip("").is_none());
        assert!(parse_tip("<think>thinking</think>").is_none());
    }

    // ---- the Coach ----------------------------------------------------

    /// A well-formed tip, so a dropped one is provably the coach's doing and
    /// not a parse failure.
    const GOOD_TIP: &str = "They want a concrete example\n- Lead with the Acme migration\n- Give one number\n- Say what you changed";

    fn line(seq: u64, source: Source, text: &str) -> TranscriptLine {
        TranscriptLine {
            session_id: "sess-1".to_string(),
            seq,
            source,
            offset_secs: seq as f32,
            text: text.to_string(),
        }
    }

    const QUESTION_1: &str = "Tell me about a time you shipped something hard.";
    const QUESTION_2: &str = "How would you scale the ingestion path?";

    /// Answers instantly with a fixed well-formed tip, recording every user
    /// turn it was asked to generate.
    struct FixedGen {
        seen: Mutex<Vec<String>>,
        reply: String,
    }

    impl FixedGen {
        fn new(reply: &str) -> Arc<Self> {
            Arc::new(Self { seen: Mutex::new(Vec::new()), reply: reply.to_string() })
        }
    }

    impl TextGenerator for FixedGen {
        fn generate(&self, req: GenRequest) -> anyhow::Result<String> {
            assert_eq!(req.priority, Priority::Coaching, "coach must use the Coaching slot");
            self.seen.lock().unwrap().push(req.user);
            Ok(self.reply.clone())
        }
    }

    /// Blocks the *first* generation on a barrier, announcing on `started`
    /// when each call begins. Stands in for a real 2-3s generation that a
    /// newer question arrives in the middle of.
    struct BlockingGen {
        started: Mutex<mpsc::Sender<String>>,
        release: (Mutex<bool>, Condvar),
        block_next: AtomicBool,
        seen: Mutex<Vec<String>>,
    }

    impl BlockingGen {
        fn new(started: mpsc::Sender<String>) -> Arc<Self> {
            Arc::new(Self {
                started: Mutex::new(started),
                release: (Mutex::new(false), Condvar::new()),
                block_next: AtomicBool::new(true),
                seen: Mutex::new(Vec::new()),
            })
        }

        fn release(&self) {
            let (m, cv) = &self.release;
            *m.lock().unwrap() = true;
            cv.notify_all();
        }
    }

    impl TextGenerator for BlockingGen {
        fn generate(&self, req: GenRequest) -> anyhow::Result<String> {
            self.seen.lock().unwrap().push(req.user.clone());
            let _ = self.started.lock().unwrap().send(req.user.clone());
            if self.block_next.swap(false, Ordering::SeqCst) {
                let (m, cv) = &self.release;
                let mut released = m.lock().unwrap();
                while !*released {
                    released = cv.wait(released).unwrap();
                }
            }
            Ok(GOOD_TIP.to_string())
        }
    }

    struct Harness {
        coach: Coach,
        tips: mpsc::Receiver<InterviewTip>,
        enabled: Arc<AtomicBool>,
    }

    fn harness(gen: Arc<dyn TextGenerator>, context: &str, on: bool) -> Harness {
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let enabled = Arc::new(AtomicBool::new(on));
        let coach = Coach::spawn(
            "sess-1".to_string(),
            gen,
            context.to_string(),
            Arc::clone(&enabled),
            CoachConfig::default(),
            Arc::new(move |tip: &InterviewTip| {
                let _ = tx.lock().unwrap().send(tip.clone());
            }),
        );
        Harness { coach, tips: rx, enabled }
    }

    fn no_tip(rx: &mpsc::Receiver<InterviewTip>) {
        match rx.recv_timeout(Duration::from_millis(400)) {
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            other => panic!("expected no further tip, got {other:?}"),
        }
    }

    #[test]
    fn only_them_lines_trigger_a_tip() {
        let gen = FixedGen::new(GOOD_TIP);
        let h = harness(gen.clone(), "Staff engineer at Acme.", true);

        // A question spoken by the candidate is not a question *for* them.
        h.coach.observe(&line(1, Source::Me, QUESTION_1), Instant::now());
        no_tip(&h.tips);

        h.coach.observe(&line(2, Source::Them, QUESTION_1), Instant::now());
        let tip = h.tips.recv_timeout(Duration::from_secs(5)).expect("tip for the Them question");
        assert_eq!(tip.question, QUESTION_1);
        assert_eq!(tip.seq, 2);
        assert_eq!(tip.bullets.len(), 3);
        assert_eq!(tip.headline, "They want a concrete example");

        // A non-question Them line changes nothing.
        h.coach.observe(&line(3, Source::Them, "So we shipped it last quarter."), Instant::now());
        no_tip(&h.tips);

        // The Me line was still kept as context for the question that followed.
        let seen = gen.seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "exactly one generation");
        assert!(seen[0].contains("Me: Tell me about a time you shipped something hard."));
        assert!(seen[0].ends_with(&format!("QUESTION: {QUESTION_1} /no_think")));
        drop(seen);
        h.coach.shutdown();
    }

    #[test]
    fn no_tip_when_disabled() {
        let gen = FixedGen::new(GOOD_TIP);
        let h = harness(gen.clone(), "Staff engineer at Acme.", false);

        h.coach.observe(&line(1, Source::Them, QUESTION_1), Instant::now());
        no_tip(&h.tips);
        assert!(gen.seen.lock().unwrap().is_empty(), "the model must not be touched");

        // Same line, same coach, flag on: proves the coach itself works and
        // the flag is what suppressed the tip.
        h.enabled.store(true, Ordering::Relaxed);
        h.coach.observe(&line(2, Source::Them, QUESTION_1), Instant::now());
        assert_eq!(
            h.tips.recv_timeout(Duration::from_secs(5)).expect("tip once enabled").seq,
            2
        );
        h.coach.shutdown();
    }

    #[test]
    fn a_newer_question_cancels_the_in_flight_one() {
        let (started_tx, started_rx) = mpsc::channel();
        let gen = BlockingGen::new(started_tx);
        let h = harness(gen.clone(), "Staff engineer at Acme.", true);

        h.coach.observe(&line(1, Source::Them, QUESTION_1), Instant::now());
        let first = started_rx.recv_timeout(Duration::from_secs(5)).expect("Q1 generation started");
        assert!(first.contains(QUESTION_1));

        // Q2 arrives while Q1 is still inside `generate`.
        h.coach.observe(&line(2, Source::Them, QUESTION_2), Instant::now());
        gen.release();

        let tip = h.tips.recv_timeout(Duration::from_secs(5)).expect("a tip");
        assert_eq!(tip.question, QUESTION_2, "the emitted tip must be for the newer question");
        assert_eq!(tip.seq, 2);
        no_tip(&h.tips);

        // Q1 really did run — its answer was computed and thrown away,
        // which is what "cancel" means for a generation already in flight
        // (GenRequest carries no cancel flag; see the module docs).
        let seen = gen.seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "both questions reached the generator: {seen:?}");
        assert!(seen[0].contains(QUESTION_1));
        assert!(seen[1].contains(QUESTION_2));
        drop(seen);
        h.coach.shutdown();
    }

    #[test]
    fn clear_emits_an_empty_tip_and_cancels() {
        let (started_tx, started_rx) = mpsc::channel();
        let gen = BlockingGen::new(started_tx);
        let h = harness(gen.clone(), "Staff engineer at Acme.", true);

        h.coach.observe(&line(7, Source::Them, QUESTION_1), Instant::now());
        started_rx.recv_timeout(Duration::from_secs(5)).expect("Q1 generation started");

        h.coach.clear();
        let cleared = h.tips.recv_timeout(Duration::from_secs(5)).expect("the clearing tip");
        assert!(cleared.bullets.is_empty(), "clear emits an empty-bullets tip");
        assert!(cleared.headline.is_empty());
        assert_eq!(cleared.seq, 7, "attributed to the question being wiped");

        // The in-flight answer arrives after the clear and must be dropped.
        gen.release();
        no_tip(&h.tips);
        h.coach.shutdown();
    }

    #[test]
    fn a_tip_that_recites_the_context_is_discarded() {
        const CONTEXT: &str = "Led the payments platform team at Acme for four years, shipping a \
                               Rust ingestion service that handled twelve thousand events per second.";
        // Structurally a perfect tip — four lines, three bullets — but the
        // content is the candidate's own resume read back.
        const ECHO: &str = "Led the payments platform team at Acme for four years\n\
                            - shipping a Rust ingestion service that handled twelve thousand\n\
                            - events per second\n\
                            - Led the payments platform team";
        assert!(parse_tip(ECHO).is_some(), "the echo is well-formed, so parse_tip cannot be what drops it");
        assert!(is_context_echo(ECHO, CONTEXT), "precondition: this is an echo");

        let h = harness(FixedGen::new(ECHO), CONTEXT, true);
        h.coach.observe(&line(1, Source::Them, QUESTION_1), Instant::now());
        no_tip(&h.tips);
        h.coach.shutdown();
    }

    #[test]
    fn shutdown_joins_the_worker() {
        let h = harness(FixedGen::new(GOOD_TIP), "Staff engineer at Acme.", true);
        let live = Arc::clone(&h.coach.shared.live);

        h.coach.observe(&line(1, Source::Them, QUESTION_1), Instant::now());
        h.tips.recv_timeout(Duration::from_secs(5)).expect("tip");
        assert_eq!(live.load(Ordering::SeqCst), 1, "worker is running");

        h.coach.shutdown();
        // `shutdown` joined, so the guard has already run — no polling.
        assert_eq!(live.load(Ordering::SeqCst), 0, "worker joined, not detached");
    }

    #[test]
    fn dropping_a_coach_without_shutdown_still_joins_the_worker() {
        let h = harness(FixedGen::new(GOOD_TIP), "", true);
        let live = Arc::clone(&h.coach.shared.live);
        assert!(matches!(live.load(Ordering::SeqCst), 0 | 1));
        drop(h);
        assert_eq!(live.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn history_is_bounded_to_three_them_and_two_me_lines() {
        let gen = FixedGen::new(GOOD_TIP);
        let h = harness(gen.clone(), "", true);
        for seq in 1..=6 {
            h.coach.observe(&line(seq, Source::Them, &format!("them statement {seq} goes here")), Instant::now());
        }
        for seq in 7..=10 {
            h.coach.observe(&line(seq, Source::Me, &format!("me statement {seq} goes here")), Instant::now());
        }
        h.coach.observe(&line(11, Source::Them, QUESTION_2), Instant::now());
        h.tips.recv_timeout(Duration::from_secs(5)).expect("tip");

        let seen = gen.seen.lock().unwrap();
        let turn = seen.last().expect("one generation");
        assert!(!turn.contains("them statement 3"), "oldest Them line evicted: {turn}");
        for kept in ["them statement 4", "them statement 5", "them statement 6", "me statement 9", "me statement 10"] {
            assert!(turn.contains(kept), "expected {kept} in: {turn}");
        }
        assert!(!turn.contains("me statement 8"), "oldest Me line evicted: {turn}");
        drop(seen);
        h.coach.shutdown();
    }
}
