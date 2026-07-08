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
//! parameter), plus [`MAX_NEW_TOKENS`] as an absolute safety valve. The hard
//! deadline itself lives one level up, in `cleanup_manager::spawn`: it races
//! the call on a worker thread against a timer, and if the timer wins it
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

/// Safety valve on a single generation call so a wedged/looping model can't
/// run forever even though the *deadline* enforcement lives in the manager.
const MAX_NEW_TOKENS: i32 = 300;
const CONTEXT_SIZE: u32 = 4096;

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

        fn generate(&self, prompt_messages: &[(&str, String)], cancel: &AtomicBool) -> Result<String> {
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

            for _ in 0..MAX_NEW_TOKENS {
                // Checked once per token (not mid-decode — llama.cpp gives
                // no hook to interrupt a single `decode` call in flight) so
                // a timeout from the caller stops us within one token's
                // worth of work instead of running to MAX_NEW_TOKENS.
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
    }

    impl CleanupProvider for LlamaCleanupProvider {
        fn clean(&self, raw: &str, mode: Mode, ctx: &CleanupContext, cancel: &AtomicBool) -> Result<String> {
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
            self.generate(&[("system", system), ("user", user)], cancel)
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
}
