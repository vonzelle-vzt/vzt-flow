//! Measures how long one interview coaching tip actually takes.
//!
//! The coach's whole premise is that a tip lands while the question is still
//! hanging in the air, so the number that matters is wall-clock latency from
//! "question recognized" to "four lines parsed" — and it has to be measured,
//! not assumed (CLAUDE.md, verification norms). The acceptance target is
//! **p95 ≤ 3.0s**; when it misses, the levers are `context_max_chars` first
//! and `max_new_tokens` second, both exposed as flags here so the experiment
//! doesn't need a recompile.
//!
//! Loads `LlamaCleanupProvider` **once** — the pattern `cleanup_replay`
//! establishes, because `flow clean-test` pays the model load plus several
//! seconds of one-time Metal pipeline JIT on every invocation and is
//! therefore useless for a series.
//!
//! Usage:
//!
//! ```text
//! cargo run --release --example interview_bench
//! cargo run --release --example interview_bench -- --context-chars 2400 --max-tokens 64
//! ```
//!
//! Reads the real `~/.config/vzt-flow/interview.md` when present so the
//! measurement reflects this machine's actual prefill cost; falls back to a
//! built-in sample resume of comparable size otherwise. The candidate context
//! is never printed — it is the user's own resume.

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn main() -> anyhow::Result<()> {
    use std::sync::atomic::AtomicBool;
    use std::time::Instant;

    use flow_core::cleanup::{is_context_echo, CleanupProvider};
    use flow_core::meeting::interview::{
        self, build_coach_system_prompt, build_coach_user_turn, looks_like_question, parse_tip,
        CoachConfig,
    };
    use flow_core::meeting::transcriber::Source;

    let defaults = CoachConfig::default();
    let mut context_chars = defaults.context_max_chars;
    let mut max_tokens = defaults.max_new_tokens;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--context-chars" => {
                context_chars = args.next().and_then(|v| v.parse().ok()).unwrap_or(context_chars)
            }
            "--max-tokens" => {
                max_tokens = args.next().and_then(|v| v.parse().ok()).unwrap_or(max_tokens)
            }
            other => anyhow::bail!(
                "unknown argument {other:?} (expected --context-chars N | --max-tokens N)"
            ),
        }
    }

    // The real context when the machine has one, so the prefill cost measured
    // here is the cost the user will actually pay.
    let (context_source, raw_context) = {
        let real = interview::load_context();
        if real.trim().is_empty() {
            ("built-in sample", SAMPLE_CONTEXT.to_string())
        } else {
            ("~/.config/vzt-flow/interview.md", real)
        }
    };
    let context = interview::truncate_context(&raw_context, context_chars);
    let system = build_coach_system_prompt(&context);

    // Two lines of meeting history, as `Coach::observe` would have collected.
    let recent = vec![
        transcript_line(1, Source::Them, "Thanks for making the time today."),
        transcript_line(2, Source::Me, "Happy to be here."),
    ];

    eprintln!("context     : {context_source}, {} chars raw", raw_context.chars().count());
    eprintln!("context_max_chars = {context_chars}   max_new_tokens = {max_tokens}");

    let model_path = flow_core::models::cleanup_model_path()?;
    let provider = flow_core::cleanup::LlamaCleanupProvider::load(&model_path)?;
    eprintln!("model load  : {:.2}s", provider.load_time.as_secs_f64());

    // Untimed warm-up, same rationale as `cleanup_replay`: forces the
    // one-time Metal kernel-pipeline JIT outside the measured run.
    let cancel = AtomicBool::new(false);
    let warm_user = build_coach_user_turn(&recent, "How would you warm up a Metal pipeline?");
    let _ = provider.generate_raw(&system, &warm_user, max_tokens, &cancel);

    // Prompt size, reported once: the system prompt is identical for every
    // question and dominates prefill. ~4 chars/token is the usual rule of
    // thumb for English on a BPE tokenizer — approximate, and labelled so.
    let sample_user = build_coach_user_turn(&recent, QUESTIONS[0]);
    let prompt_chars = system.chars().count() + sample_user.chars().count();
    println!(
        "prompt      : {} chars (system {} + user {}), ~{} tokens (approx, chars/4)",
        prompt_chars,
        system.chars().count(),
        sample_user.chars().count(),
        prompt_chars / 4
    );
    println!();

    let mut timings_ms: Vec<u64> = Vec::with_capacity(QUESTIONS.len());
    let mut parsed = 0usize;
    let mut echoes = 0usize;
    for (i, question) in QUESTIONS.iter().enumerate() {
        assert!(
            looks_like_question(question),
            "sample {i} must be recognized as a question: {question:?}"
        );
        let user = build_coach_user_turn(&recent, question);
        let started = Instant::now();
        let raw = provider.generate_raw(&system, &user, max_tokens, &cancel)?;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        timings_ms.push(elapsed_ms);

        // The same two output-side gates the coach applies, so a fast run
        // that produces nothing usable can't read as a pass.
        let echoed = is_context_echo(&raw, &context);
        let tip = parse_tip(&raw);
        if echoed {
            echoes += 1;
        }
        if tip.is_some() && !echoed {
            parsed += 1;
        }
        let verdict = if echoed {
            "CONTEXT-ECHO"
        } else if tip.is_some() {
            "ok"
        } else {
            "unparsed"
        };
        println!("[{i}] {elapsed_ms:>5}ms  {verdict:<12} {}", truncate(question, 58));
        match &tip {
            Some((headline, bullets)) => {
                println!("      ▸ {headline}");
                for b in bullets {
                    println!("        - {b}");
                }
            }
            None => println!("      raw={:?}", truncate(raw.trim(), 110)),
        }
    }

    let mut sorted = timings_ms.clone();
    sorted.sort_unstable();
    let p50 = percentile(&sorted, 50);
    let p95 = percentile(&sorted, 95);
    let mean = sorted.iter().sum::<u64>() as f64 / sorted.len() as f64;

    println!();
    println!("tips        : {}/{} parsed, {} context echoes", parsed, QUESTIONS.len(), echoes);
    println!("min={}ms  mean={:.0}ms  max={}ms", sorted[0], mean, sorted[sorted.len() - 1]);
    // Nearest-rank percentiles. With 8 samples p95 is the slowest one, which
    // is the conservative reading and exactly what the 3.0s gate wants.
    println!("p50={}ms ({:.2}s)", p50, p50 as f64 / 1000.0);
    println!("p95={}ms ({:.2}s)", p95, p95 as f64 / 1000.0);
    let verdict = if p95 <= 3000 { "PASS" } else { "MISS" };
    println!("target p95 <= 3000ms: {verdict}");

    // Drop the provider (and with it the llama.cpp model, backend and Metal
    // residency sets) BEFORE exiting — `std::process::exit` skips destructors
    // and tearing down with a live Metal context trips ggml's
    // `GGML_ASSERT([rsets->data count] == 0)`, aborting with 134.
    drop(provider);
    Ok(())
}

/// Nearest-rank percentile over an already-sorted slice.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn percentile(sorted: &[u64], p: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (p * sorted.len()).div_ceil(100).max(1);
    sorted[rank - 1]
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn transcript_line(
    seq: u64,
    source: flow_core::meeting::transcriber::Source,
    text: &str,
) -> flow_core::meeting::events::TranscriptLine {
    flow_core::meeting::events::TranscriptLine {
        session_id: "interview-bench".to_string(),
        seq,
        source,
        offset_secs: seq as f32,
        text: text.to_string(),
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let head: String = s.chars().take(n).collect();
    format!("{head}…")
}

/// The eight questions timed on every run. Chosen to hit all three
/// [`flow_core::meeting::interview::looks_like_question`] rules and to look
/// like a real screen: two behavioural, two systems, two motivational, two
/// forward-looking.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const QUESTIONS: &[&str] = &[
    "Tell me about a time you had to make a difficult technical decision.",
    "Walk me through the architecture of the last system you shipped.",
    "How would you scale that to ten times the traffic?",
    "What was the hardest bug you have ever had to track down?",
    "Why did you choose Rust for the audio pipeline?",
    "Give me an example of a tradeoff you made under a deadline.",
    "Describe a time you disagreed with your manager about a design.",
    "Where do you see yourself taking this team in the first ninety days?",
];

/// Stand-in candidate context, used when the machine has no
/// `~/.config/vzt-flow/interview.md`.
///
/// Deliberately **larger than the default `context_max_chars`**, so a run on
/// a machine with no real context still measures the worst case the config
/// permits: if the sample were shorter than the cap, the cap would never
/// bind, `--context-chars` would be a no-op, and the bench could not answer
/// the question it exists to answer. A real resume plus a job description
/// plus talking points reaches this size easily.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const SAMPLE_CONTEXT: &str = "\
# Candidate

Staff Software Engineer, 11 years. Systems and audio infrastructure.
Based in Seattle. Open to hybrid.

## Current role - Northwind (2022 to now)
Tech lead for the real-time ingestion platform. Took a Python service that
topped out at 900 events/second and rewrote the hot path in Rust; it now
sustains 12,000 events/second at p99 under 40ms on the same three machines.
Owned the migration end to end, including a six-week dual-write period so
nothing was cut over blind. Led a team of five; ran the on-call rotation and
cut paging volume by 60% in two quarters by fixing the three alerts that
produced most of the noise.

Also owned the schema-evolution story: introduced a compatibility checker in
CI that rejects a producer change no consumer can read yet. It has blocked
seventeen breaking changes since it shipped and has never had a false
positive, which is why nobody has tried to turn it off.

Mentored three engineers to senior. Two of them now run services of their own.
The mentoring I am proudest of was the one who wanted to rewrite everything;
we shipped the smallest useful piece first and the rewrite never became
necessary.

## Earlier - Acme Media (2018-2022)
Built the transcription pipeline behind the podcast product. Learned the hard
way that a model with quadratic memory in input length will OOM a box in
production and not in staging: fixed it with silence-aware chunking, which
also cut end-of-recording latency from 25s to under 1s. The chunker is still
the piece I point at when someone asks what good engineering looks like -
it solved a memory ceiling and a latency wall with one idea.

Ran the migration off a vendor speech API onto an on-device model. Cut the
per-minute cost to zero, cut p50 latency from 4.1s to 900ms, and removed the
privacy review from every new feature that touched audio. The tradeoff was
accuracy: word error rate went from 7% to 9%, which we accepted after showing
product that the errors were in proper nouns a dictionary pass could fix.

## Before that - Lightship (2015-2018)
Backend engineer on payments. Wrote the idempotency layer that made retries
safe; it is still in production. Also the person who found the double-charge
bug on the night of the biggest sale of the year, by reading the ledger
rather than the logs.

## Technologies
Rust, Go, Python, C. Postgres, Kafka, Redis. macOS and Linux internals,
CoreAudio, ScreenCaptureKit, llama.cpp, Metal, CUDA basics. Tokio, gRPC,
Terraform, Kubernetes. Prometheus and Grafana; OpenTelemetry tracing.

## How I work
- I prefer measuring over arguing: every number above was measured, not
  estimated, and I keep the harness in the repo so the next person can
  re-measure it rather than trust me.
- I write the failure mode into the comment. Code says what it does; the
  comment should say what it will do to you at 3am.
- I ship the smallest thing that proves the idea, then widen it.
- I would rather delete a feature than maintain one nobody uses.

## Talking points
- The failure I am proudest of catching is a race that only reproduced with
  eight concurrent callers; a single-call test passed every time, which is
  why the regression test spawns eight threads.
- The decision I would make differently: I let a dual-write period run six
  weeks when three would have done, because I was more worried about being
  wrong than about the cost of waiting.
- I want to work closer to the product and further from the incident channel.
- Question I want to ask them: what does the on-call week actually look like,
  and who decides what is worth paging for?

## Role applied for
Staff engineer, platform. The posting emphasises real-time systems, explicit
latency budgets, mentoring, and taking a service from prototype to
production. It calls out Rust and streaming data specifically, and says the
team owns its own on-call. Reports to the director of platform engineering.
Interview loop: system design, a code reading exercise, two behavioural
rounds, and a conversation with the director about scope.

## Selected projects
- vzt-flow: an on-device dictation tool. Hold a key, talk, the transcript
  lands at the cursor. No cloud, no subscription, no account. The interesting
  part is the scheduler that lets one resident 1.1GB model serve dictation
  cleanup, a meeting summary and live coaching without any of them starving
  the others.
- A lock-free SPSC ring buffer for CoreAudio render callbacks, benchmarked
  against a mutex version at 48kHz; the mutex version dropped frames under
  load and the ring buffer did not.
- A migration checker that diffs a Postgres schema against every query in the
  repo and fails CI when a column a query needs is about to disappear.

## Education
BSc Computer Science, University of Washington, 2015. Undergraduate thesis on
lock-free ring buffers for audio callbacks, which is more relevant now than
it was then.
";

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
fn main() {
    eprintln!("interview_bench needs the embedded llama.cpp provider (Apple Silicon macOS only)");
    std::process::exit(2);
}
