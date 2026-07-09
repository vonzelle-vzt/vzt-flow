//! LLM cleanup engine: fixes up a raw ASR transcript (filler words, false
//! starts, grammar/punctuation) or restructures it for the target app
//! ("polish"), via an embedded llama.cpp model.
//!
//! # Model choice
//!
//! Qwen3-1.7B-Instruct, GGUF, Q4_K_M
//! (`unsloth/Qwen3-1.7B-GGUF:Qwen3-1.7B-Q4_K_M.gguf`, ~1.1GB, sha256
//! `b139949c5bd74937ad8ed8c8cf3d9ffb1e99c866c823204dc42c0d91fa181897`).
//! `llama-cpp-2` reads the chat template baked into the GGUF via
//! `LlamaModel::chat_template`/`apply_chat_template` rather than hardcoding
//! one, so it handles Qwen3's template the same generic way it'd handle
//! Llama 3.2's — there was no fallback needed.
//!
//! # Timeout model
//!
//! The trait runs generation on the calling thread but checks a shared
//! `cancel` flag once per generated token (see `clean`'s `cancel`
//! parameter), plus a per-call output-token budget (scaled from the input's
//! length, floored at [`MIN_NEW_TOKENS`] and capped at
//! [`MAX_NEW_TOKENS_CEILING`] — see `generate`) as an absolute safety valve.
//! The hard wall-clock deadline itself lives one level up, in
//! `cleanup_manager::spawn`: it races
//! the call on a worker thread against a timer computed by
//! `cleanup_manager::cleanup_deadline_ms`, and if the timer wins it
//! sets `cancel`, waits a short grace period, and **joins** the worker
//! thread before returning the raw transcript — a live llama.cpp
//! context/thread is never detached. (An earlier version of this file did
//! detach on timeout; that orphaned a Metal-backed generation thread and
//! crashed at process exit with `GGML_ASSERT([rsets->data count] == 0)` —
//! cooperative cancellation + join is the fix.)

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use anyhow::{Context, Result};

/// How much (if any) LLM rewriting to apply. `Raw` never touches the LLM —
/// Parakeet already punctuates, so "raw" mode is a pure passthrough.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Raw,
    Clean,
    Polish,
}

impl Mode {
    pub fn parse(s: &str) -> Mode {
        match s.to_lowercase().as_str() {
            "raw" => Mode::Raw,
            "polish" => Mode::Polish,
            _ => Mode::Clean,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Mode::Raw => "raw",
            Mode::Clean => "clean",
            Mode::Polish => "polish",
        }
    }
}

/// Context passed to the cleanup provider so it can tailor its output to
/// where the transcript is headed.
#[derive(Debug, Clone, Default)]
pub struct CleanupContext {
    pub app_name: Option<String>,
    pub tone: String,
    pub dictionary_terms: Vec<String>,
}

/// A cleanup engine. Implementations must be safe to share across threads
/// (the cleanup manager owns one instance behind a lazy-load/idle-unload
/// lifecycle identical to the transcriber's).
pub trait CleanupProvider: Send + Sync {
    /// `cancel` is checked cooperatively (once per generated token) so a
    /// caller can abort a long-running generation from another thread by
    /// setting it — see the module docs on the timeout model.
    fn clean(&self, raw: &str, mode: Mode, ctx: &CleanupContext, cancel: &AtomicBool) -> Result<String>;
}

/// Always returns the input unchanged. Used when no model is loaded (or
/// couldn't be), and in tests.
pub struct PassthroughProvider;

impl CleanupProvider for PassthroughProvider {
    fn clean(&self, raw: &str, _mode: Mode, _ctx: &CleanupContext, _cancel: &AtomicBool) -> Result<String> {
        Ok(raw.to_string())
    }
}

/// Floor on the output-token budget for a single generation call — the
/// previous flat value, kept as the minimum so short dictations (the common
/// case) generate exactly as before.
const MIN_NEW_TOKENS: i32 = 300;

/// Ceiling on the output-token budget regardless of input length, so a
/// pathological input can't turn one cleanup call into a many-minute
/// generation even when the context has room for it. The *wall-clock*
/// deadline (see `cleanup_manager::cleanup_deadline_ms`) is what actually
/// bounds latency in practice; this just bounds compute per call.
const MAX_NEW_TOKENS_CEILING: i32 = 3000;

/// Rough characters-per-token estimate for English dictation text under
/// Qwen3's BPE tokenizer, used only to *size* the output-token budget below
/// — not for exact accounting (the real prompt token count from
/// `str_to_token` is what gates the hard skip-cleanup decision in
/// `generate`).
const CHARS_PER_TOKEN_ESTIMATE: f64 = 4.0;

/// Cleanup output is about as long as its input (grammar/filler-word fixes,
/// not summarization), so the output-token budget starts from a 1:1 token
/// estimate with 30% headroom for cases where the model expands slightly
/// (spelled-out terms, added punctuation). Mirrored by
/// `cleanup_manager::cleanup_deadline_ms`'s own per-char timing derivation.
const OUTPUT_TOKEN_HEADROOM: f64 = 1.3;

/// Raised from 4096 to fit a long-form dictation's prompt (a 10-minute
/// ramble is ~1500 words / ~2000+ tokens) plus a comparable output budget
/// within one context — Qwen3-1.7B supports up to 32K natively, so 8192
/// leaves plenty of headroom while still being cheap to allocate.
const CONTEXT_SIZE: u32 = 8192;

/// Whether a prompt of `prompt_tokens` leaves a safe budget for at least
/// [`MIN_NEW_TOKENS`] of output within `context_size` — if not, cleanup
/// should be skipped outright (raw fallback) rather than truncate the
/// speaker's words to fit. Factored out of `generate` (which is
/// macOS/llama.cpp-only) so it's unit-testable on every platform.
fn prompt_fits_context(prompt_tokens: u32, context_size: u32) -> bool {
    let safe_budget = context_size.saturating_sub(MIN_NEW_TOKENS as u32);
    prompt_tokens < safe_budget
}

/// Sizes the output-token budget for a generation call from the input's
/// character length: a 1:1 token estimate (with headroom) floored at
/// [`MIN_NEW_TOKENS`], capped at [`MAX_NEW_TOKENS_CEILING`], and further
/// clamped to whatever room is actually left in the context after the
/// prompt. Factored out of `generate` so it's unit-testable on every
/// platform. Caller must have already checked [`prompt_fits_context`].
fn max_new_tokens_for(input_char_len: usize, prompt_tokens: u32, context_size: u32) -> i32 {
    let estimated = ((input_char_len as f64 / CHARS_PER_TOKEN_ESTIMATE) * OUTPUT_TOKEN_HEADROOM).ceil() as i32;
    estimated
        .clamp(MIN_NEW_TOKENS, MAX_NEW_TOKENS_CEILING)
        .min((context_size - prompt_tokens) as i32)
}

#[cfg(target_os = "macos")]
mod llama_impl {
    use super::*;
    use llama_cpp_2::context::params::LlamaContextParams;
    use llama_cpp_2::llama_backend::LlamaBackend;
    use llama_cpp_2::llama_batch::LlamaBatch;
    use llama_cpp_2::model::params::LlamaModelParams;
    use llama_cpp_2::model::{AddBos, LlamaChatMessage, LlamaChatTemplate, LlamaModel};
    use llama_cpp_2::sampling::LlamaSampler;
    use std::num::NonZeroU32;

    pub struct LlamaCleanupProvider {
        model: LlamaModel,
        backend: LlamaBackend,
        chat_template: LlamaChatTemplate,
        pub load_time: std::time::Duration,
    }

    impl LlamaCleanupProvider {
        pub fn load(model_path: &Path) -> Result<Self> {
            if !model_path.exists() {
                anyhow::bail!(
                    "cleanup model not found at {}. Run `flow models download cleanup` first.",
                    model_path.display()
                );
            }
            let started = Instant::now();
            let backend = LlamaBackend::init().context("failed to init llama.cpp backend")?;
            // Offload every layer to Metal; llama.cpp caps this at the
            // model's actual layer count, so a large number just means
            // "as much as fits".
            let model_params = LlamaModelParams::default().with_n_gpu_layers(1_000_000);
            let model = LlamaModel::load_from_file(&backend, model_path, &model_params)
                .with_context(|| format!("failed to load cleanup model from {}", model_path.display()))?;
            let chat_template = model
                .chat_template(None)
                .context("cleanup model has no baked-in chat template")?;
            Ok(Self { model, backend, chat_template, load_time: started.elapsed() })
        }

        fn generate(
            &self,
            prompt_messages: &[(&str, String)],
            input_char_len: usize,
            cancel: &AtomicBool,
        ) -> Result<String> {
            let messages: Vec<LlamaChatMessage> = prompt_messages
                .iter()
                .map(|(role, content)| LlamaChatMessage::new(role.to_string(), content.clone()))
                .collect::<std::result::Result<_, _>>()
                .context("failed to build chat messages")?;
            let prompt = self
                .model
                .apply_chat_template(&self.chat_template, &messages, true)
                .context("failed to apply chat template")?;

            let ctx_params = LlamaContextParams::default().with_n_ctx(NonZeroU32::new(CONTEXT_SIZE));
            let mut llama_ctx = self
                .model
                .new_context(&self.backend, ctx_params)
                .context("failed to create llama context")?;

            let tokens = self
                .model
                .str_to_token(&prompt, AddBos::Always)
                .context("failed to tokenize prompt")?;
            if tokens.is_empty() {
                return Ok(String::new());
            }

            // If the prompt alone doesn't leave a safe budget for output,
            // don't attempt cleanup at all — SKIP deliberately (raw
            // fallback, via the same empty-output path the caller already
            // treats as "no usable output") rather than truncate the
            // speaker's words to fit.
            let prompt_tokens = tokens.len() as u32;
            if !prompt_fits_context(prompt_tokens, CONTEXT_SIZE) {
                eprintln!(
                    "[vzt-flow] cleanup: prompt is {prompt_tokens} tokens (context budget {CONTEXT_SIZE}); \
                     skipping cleanup and pasting the raw transcript rather than truncating it"
                );
                return Ok(String::new());
            }

            // Size the output-token budget from the input length so long
            // dictations aren't cut off at the old flat 300-token limit;
            // still clamped to the remaining context room so it can never
            // overrun what's actually left after the prompt.
            let max_new_tokens = max_new_tokens_for(input_char_len, prompt_tokens, CONTEXT_SIZE);

            let mut batch = LlamaBatch::new(tokens.len().max(512), 1);
            let last_index = tokens.len() - 1;
            for (i, token) in tokens.iter().enumerate() {
                batch.add(*token, i as i32, &[0], i == last_index)?;
            }
            llama_ctx.decode(&mut batch).context("initial prompt decode failed")?;

            let mut sampler = LlamaSampler::chain_simple([LlamaSampler::dist(1234), LlamaSampler::greedy()]);
            let mut n_cur = batch.n_tokens();
            let mut decoder = encoding_rs::UTF_8.new_decoder();
            let mut output = String::new();

            for _ in 0..max_new_tokens {
                // Checked once per token (not mid-decode — llama.cpp gives
                // no hook to interrupt a single `decode` call in flight) so
                // a timeout from the caller stops us within one token's
                // worth of work instead of running to max_new_tokens.
                if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                let token = sampler.sample(&llama_ctx, batch.n_tokens() - 1);
                sampler.accept(token);
                if self.model.is_eog_token(token) {
                    break;
                }
                let piece = self
                    .model
                    .token_to_piece(token, &mut decoder, true, None)
                    .context("failed to decode token")?;
                output.push_str(&piece);

                batch.clear();
                batch.add(token, n_cur, &[0], true)?;
                n_cur += 1;
                llama_ctx.decode(&mut batch).context("decode step failed")?;
            }

            Ok(strip_think_block(&output))
        }

        /// Summarizes a meeting transcript into `## Summary` + `## Action
        /// items` markdown (see [`build_summary_prompt`]). Reuses the same
        /// token-budgeted, cooperatively-cancellable generation path as
        /// cleanup — the caller (meeting mode) truncates an over-long
        /// transcript to a context-safe tail before calling, so `generate`'s
        /// prompt-fits-context guard is never the thing that trips here.
        pub fn summarize(&self, transcript: &str, cancel: &AtomicBool) -> Result<String> {
            let system = build_summary_prompt();
            // Same Qwen3 `/no_think` suppression rationale as `clean`.
            let user = format!("{transcript} /no_think");
            self.generate(&[("system", system), ("user", user)], transcript.chars().count(), cancel)
        }
    }

    impl CleanupProvider for LlamaCleanupProvider {
        fn clean(&self, raw: &str, mode: Mode, ctx: &CleanupContext, cancel: &AtomicBool) -> Result<String> {
            // An empty (or whitespace-only) user turn has nothing for the
            // model to "correct" — sent through the dictionary-injected
            // system prompt anyway (any mode but Raw, which never touches
            // the LLM to begin with), Qwen3 completes by echoing the
            // glossary term list back instead (e.g. `flow clean-test ""`
            // pasted 19 brand names on a silent hotkey press). Short-circuit
            // before the prompt is even built, for every mode.
            // `Ok(String::new())` (not `Ok(raw.to_string())`) so this hits
            // the exact same `text.trim().is_empty()` "no usable output"
            // branch every caller already has for a failed/empty
            // generation — both `cleanup_manager::spawn` and
            // `clean_test::run` fall back to the original raw text there,
            // which for an empty/whitespace `raw` means pasting nothing (or
            // harmless whitespace), never LLM output.
            if raw.trim().is_empty() {
                return Ok(String::new());
            }
            if mode == Mode::Raw {
                return Ok(raw.to_string());
            }
            let system = build_system_prompt(mode, ctx);
            // Qwen3's chat template enables its <think>...</think>
            // reasoning mode by default; left on, the model spends the
            // entire token budget reasoning out loud and never reaches the
            // actual corrected text within the deadline. `/no_think` is
            // Qwen3's own documented per-turn switch to suppress it — far
            // cheaper than trying to parse past a (possibly truncated)
            // thinking block.
            let user = format!("{raw} /no_think");
            self.generate(&[("system", system), ("user", user)], raw.chars().count(), cancel)
        }
    }
}

#[cfg(target_os = "macos")]
pub use llama_impl::LlamaCleanupProvider;

/// Strips a leading `<think>...</think>` block (and any surrounding
/// whitespace) from a generation. Even with `/no_think` appended to the
/// user turn, Qwen3 still emits an (empty) think block as an artifact of
/// its chat template before the real answer — this removes it rather than
/// leaving `<think>\n\n</think>\n\n` glued onto the front of every result.
pub fn strip_think_block(text: &str) -> String {
    if let Some(start) = text.find("<think>") {
        if let Some(end_rel) = text[start..].find("</think>") {
            let end = start + end_rel + "</think>".len();
            let mut stripped = String::new();
            stripped.push_str(&text[..start]);
            stripped.push_str(&text[end..]);
            return stripped.trim().to_string();
        }
    }
    text.trim().to_string()
}

/// Builds the system-message instructions for a given mode/context. Kept as
/// a free function (not tied to the llama-only impl) so it's unit-testable
/// on every platform and reusable by any future provider.
pub fn build_system_prompt(mode: Mode, ctx: &CleanupContext) -> String {
    let mut prompt = match mode {
        Mode::Raw => String::new(),
        Mode::Clean => {
            "Fix transcription of dictated speech: remove filler words (um, uh, like, you know), \
             false starts and repeated words; fix grammar and punctuation. PRESERVE the speaker's \
             meaning, wording and tone — do NOT summarize, shorten, rephrase or add anything. \
             Output only the corrected text. Also strip other verbal fillers such as \"so\", \
             \"well\", \"basically\", \"actually\", \"kind of\", \"sort of\" when they are not part \
             of the sentence's meaning, and collapse any word or short phrase the speaker repeated \
             back-to-back (e.g. \"I I\", \"the the\") down to one occurrence."
                .to_string()
        }
        Mode::Polish => {
            let target = ctx
                .app_name
                .as_deref()
                .map(|a| format!("for {a}"))
                .unwrap_or_else(|| "for this context".to_string());
            format!(
                "Restructure this dictated text into clear, well-formatted, {} writing {target}. \
                 Fix grammar, punctuation, and structure. PRESERVE the speaker's meaning and intent \
                 — do NOT invent new content or add information that wasn't said. Output only the \
                 rewritten text.",
                ctx.tone
            )
        }
    };
    if !ctx.dictionary_terms.is_empty() {
        prompt.push_str("\n\nThese terms are spelled: ");
        prompt.push_str(&ctx.dictionary_terms.join(", "));
        prompt.push('.');
    }
    prompt
}

/// System prompt for meeting-transcript summarization. Asks for exactly two
/// markdown sections — a short `## Summary` bullet list and a `## Action
/// items` checkbox list — so the output can be appended verbatim to the
/// transcript file. Kept as a free function (not tied to the llama-only impl)
/// so it's unit-testable on every platform, mirroring `build_system_prompt`.
pub fn build_summary_prompt() -> String {
    "You summarize a meeting transcript. The transcript lines are labelled \
     \"Them:\" (other participants) and \"Me:\" (the user). Produce EXACTLY two \
     markdown sections and nothing else:\n\n\
     ## Summary\n\
     - 3 to 6 concise bullet points capturing the key topics, decisions, and \
     outcomes.\n\n\
     ## Action items\n\
     - A markdown checkbox list (\"- [ ] ...\") of concrete follow-up tasks, \
     each with the owner if stated. If there are no action items, write \
     exactly \"- [ ] none\".\n\n\
     Base everything strictly on the transcript — do NOT invent participants, \
     decisions, or tasks that weren't said. Output only the two sections."
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_mode_is_a_pure_passthrough() {
        let provider = PassthroughProvider;
        let ctx = CleanupContext::default();
        let cancel = AtomicBool::new(false);
        assert_eq!(
            provider.clean("um so like hello", Mode::Raw, &ctx, &cancel).unwrap(),
            "um so like hello"
        );
    }

    /// A provider that ignores its input entirely and "echoes" the
    /// dictionary terms baked into `ctx`, standing in for what a real LLM
    /// does with a dictionary-injected system prompt and an empty user
    /// turn (nothing to correct, so it completes by reciting the glossary).
    /// Used to prove callers never see that echo once `clean()` itself
    /// short-circuits on empty/whitespace input — this provider intentionally
    /// has no such guard, so a test bug in the guard would show up as this
    /// echo leaking through.
    struct GlossaryEchoProvider;
    impl CleanupProvider for GlossaryEchoProvider {
        fn clean(&self, raw: &str, _mode: Mode, ctx: &CleanupContext, _cancel: &AtomicBool) -> Result<String> {
            if raw.trim().is_empty() {
                return Ok(ctx.dictionary_terms.join(" "));
            }
            Ok(raw.to_string())
        }
    }

    #[test]
    fn glossary_echo_provider_reproduces_the_bug_when_unguarded() {
        // Sanity check on the stand-in itself: without a guard, an empty
        // user turn against a dictionary-injected prompt echoes the
        // glossary — this is the bug `flow clean-test ""` hit in
        // production before the `raw.trim().is_empty()` short-circuit was
        // added to the real `LlamaCleanupProvider::clean`.
        let provider = GlossaryEchoProvider;
        let ctx = CleanupContext {
            dictionary_terms: vec!["Supabase".into(), "Whop".into(), "VZT".into(), "Resend".into()],
            ..Default::default()
        };
        let cancel = AtomicBool::new(false);
        let out = provider.clean("", Mode::Clean, &ctx, &cancel).unwrap();
        assert_eq!(out, "Supabase Whop VZT Resend");
    }

    #[cfg(target_os = "macos")]
    mod llama_provider_empty_input {
        use super::*;

        /// End-to-end regression test for the empty-input glossary echo,
        /// against the real `LlamaCleanupProvider` (not a mock) — but never
        /// reaches generation, since `clean()` short-circuits before
        /// building the prompt or creating an llama.cpp context, so it's
        /// cheap: only pays the one-time model load, no token decoding.
        /// Skips (rather than failing) if the cleanup model isn't
        /// downloaded on this machine, since CI/dev boxes without it
        /// shouldn't fail the suite over a missing multi-GB download.
        fn load_or_skip() -> Option<LlamaCleanupProvider> {
            let path = match crate::models::cleanup_model_path() {
                Ok(p) if p.exists() => p,
                _ => {
                    eprintln!("skipping: cleanup model not downloaded on this machine");
                    return None;
                }
            };
            LlamaCleanupProvider::load(&path).ok()
        }

        #[test]
        fn empty_input_returns_empty_without_touching_the_glossary() {
            let Some(provider) = load_or_skip() else { return };
            let ctx = CleanupContext {
                dictionary_terms: vec!["Supabase".into(), "Whop".into(), "VZT".into(), "Resend".into()],
                ..Default::default()
            };
            let cancel = AtomicBool::new(false);
            let out = provider.clean("", Mode::Clean, &ctx, &cancel).unwrap();
            assert_eq!(out, "");
        }

        #[test]
        fn whitespace_only_input_returns_empty_without_touching_the_glossary() {
            let Some(provider) = load_or_skip() else { return };
            let ctx = CleanupContext {
                dictionary_terms: vec!["Supabase".into(), "Whop".into(), "VZT".into(), "Resend".into()],
                ..Default::default()
            };
            let cancel = AtomicBool::new(false);
            let out = provider.clean("   \n\t  ", Mode::Polish, &ctx, &cancel).unwrap();
            assert_eq!(out, "");
        }
    }

    /// A provider that "generates" one token per loop iteration and
    /// checks `cancel` between iterations, exactly like
    /// `LlamaCleanupProvider::generate` — exercises the cooperative-cancel
    /// contract in a unit test without needing a real model.
    struct CountingProvider;
    impl CleanupProvider for CountingProvider {
        fn clean(&self, _raw: &str, _mode: Mode, _ctx: &CleanupContext, cancel: &AtomicBool) -> Result<String> {
            let mut tokens_emitted = 0;
            for _ in 0..1_000_000 {
                if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                tokens_emitted += 1;
                if tokens_emitted >= 5 {
                    // Simulate "someone flips cancel mid-generation" after a
                    // few tokens, the way a deadline-timer thread would.
                    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
            Ok(format!("{tokens_emitted} tokens"))
        }
    }

    #[test]
    fn cooperative_cancel_stops_generation_promptly() {
        let provider = CountingProvider;
        let ctx = CleanupContext::default();
        let cancel = AtomicBool::new(false);
        let result = provider.clean("raw text", Mode::Clean, &ctx, &cancel).unwrap();
        // Stopped at 5 tokens, not the full 1_000_000-iteration budget —
        // proves the loop actually honors `cancel` instead of running to
        // completion regardless.
        assert_eq!(result, "5 tokens");
        assert!(cancel.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn cancel_flag_preset_stops_before_any_work() {
        let provider = CountingProvider;
        let ctx = CleanupContext::default();
        let cancel = AtomicBool::new(true); // already cancelled before the call
        let result = provider.clean("raw text", Mode::Clean, &ctx, &cancel).unwrap();
        assert_eq!(result, "0 tokens");
    }

    #[test]
    fn strip_think_block_removes_empty_think_artifact() {
        assert_eq!(strip_think_block("<think>\n\n</think>\n\nHello there."), "Hello there.");
    }

    #[test]
    fn strip_think_block_removes_populated_think_block() {
        let text = "<think>reasoning about it here</think>\nThe final answer.";
        assert_eq!(strip_think_block(text), "The final answer.");
    }

    #[test]
    fn strip_think_block_is_noop_without_a_think_tag() {
        assert_eq!(strip_think_block("just the answer"), "just the answer");
    }

    #[test]
    fn mode_parse_defaults_to_clean() {
        assert_eq!(Mode::parse("raw"), Mode::Raw);
        assert_eq!(Mode::parse("polish"), Mode::Polish);
        assert_eq!(Mode::parse("clean"), Mode::Clean);
        assert_eq!(Mode::parse("nonsense"), Mode::Clean);
        assert_eq!(Mode::parse("CLEAN"), Mode::Clean);
    }

    #[test]
    fn clean_prompt_includes_core_instructions() {
        let ctx = CleanupContext { dictionary_terms: vec!["Supabase".into()], ..Default::default() };
        let prompt = build_system_prompt(Mode::Clean, &ctx);
        assert!(prompt.contains("remove filler words"));
        assert!(prompt.contains("do NOT summarize"));
        assert!(prompt.contains("These terms are spelled: Supabase"));
    }

    #[test]
    fn polish_prompt_mentions_tone_and_app() {
        let ctx = CleanupContext {
            app_name: Some("Mail".to_string()),
            tone: "formal".to_string(),
            dictionary_terms: vec![],
        };
        let prompt = build_system_prompt(Mode::Polish, &ctx);
        assert!(prompt.contains("formal"));
        assert!(prompt.contains("for Mail"));
        assert!(!prompt.contains("These terms are spelled"));
    }

    #[test]
    fn raw_mode_prompt_is_empty() {
        assert_eq!(build_system_prompt(Mode::Raw, &CleanupContext::default()), "");
    }

    #[test]
    fn summary_prompt_requests_both_sections_and_grounding() {
        let p = build_summary_prompt();
        assert!(p.contains("## Summary"));
        assert!(p.contains("## Action items"));
        assert!(p.contains("- [ ] none"));
        // Must instruct the model not to fabricate content.
        assert!(p.contains("do NOT invent"));
    }

    #[test]
    fn short_input_uses_the_min_token_floor() {
        // A one-sentence dictation (~40 chars) shouldn't get a smaller
        // output budget than the old flat 300-token default.
        assert_eq!(max_new_tokens_for(40, 200, CONTEXT_SIZE), MIN_NEW_TOKENS);
    }

    #[test]
    fn long_input_scales_the_token_budget_up() {
        // ~1500-word ramble (~8000 chars): 8000/4 * 1.3 = 2600 estimated
        // tokens, well above the 300 floor, below the 3000 ceiling, and
        // comfortably inside an 8192 context after a modest prompt.
        let budget = max_new_tokens_for(8_000, 2_500, CONTEXT_SIZE);
        assert!(budget > MIN_NEW_TOKENS, "long input should scale past the floor: got {budget}");
        assert_eq!(budget, 2600);
    }

    #[test]
    fn token_budget_is_capped_regardless_of_input_length() {
        let budget = max_new_tokens_for(1_000_000, 200, CONTEXT_SIZE);
        assert_eq!(budget, MAX_NEW_TOKENS_CEILING);
    }

    #[test]
    fn token_budget_never_exceeds_remaining_context() {
        // A near-full context (8000 of 8192 tokens already used by the
        // prompt) must clamp the output budget to what's actually left,
        // even though the input-length estimate would ask for more.
        let budget = max_new_tokens_for(8_000, 8_000, CONTEXT_SIZE);
        assert_eq!(budget, (CONTEXT_SIZE - 8_000) as i32);
        assert!(budget < MIN_NEW_TOKENS, "sanity: this is the near-overflow case prompt_fits_context should catch");
    }

    #[test]
    fn prompt_fits_context_true_for_small_prompt() {
        assert!(prompt_fits_context(500, CONTEXT_SIZE));
    }

    #[test]
    fn prompt_fits_context_false_when_no_room_for_min_output() {
        // Exactly at the boundary: context_size - MIN_NEW_TOKENS tokens of
        // prompt leaves zero room, must be rejected (not just "tight").
        let boundary = CONTEXT_SIZE - MIN_NEW_TOKENS as u32;
        assert!(!prompt_fits_context(boundary, CONTEXT_SIZE));
        assert!(prompt_fits_context(boundary - 1, CONTEXT_SIZE));
    }

    #[test]
    fn prompt_fits_context_never_panics_on_pathological_input() {
        assert!(!prompt_fits_context(u32::MAX, CONTEXT_SIZE));
        assert!(!prompt_fits_context(CONTEXT_SIZE, 0));
    }
}
