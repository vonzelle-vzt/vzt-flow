//! Ad-hoc verification helper (not part of the CLI): replays a corpus of raw
//! transcripts through the real `LlamaCleanupProvider` and reports how many
//! came back as a glossary echo — the failure where the model, handed a
//! dictionary-injected system prompt, answers by reciting the term list
//! instead of correcting the speaker's words (see `cleanup::is_glossary_echo`).
//!
//! Exists because the per-input alternative (`flow clean-test`) pays the
//! model load plus several seconds of one-time Metal pipeline JIT on *every*
//! invocation, which is far too slow for a corpus. This loads the provider
//! once and reuses it, so a 70-line corpus runs in under a minute.
//!
//! Usage:
//!
//! ```text
//! cargo run --release --example cleanup_replay -- <corpus.jsonl>
//! ```
//!
//! The corpus is JSON Lines, one record per dictation, mirroring the shape
//! of `~/.config/vzt-flow/history.jsonl`:
//!
//! ```json
//! {"raw_text": "Merge.", "mode": "clean"}
//! ```
//!
//! `mode` is optional and defaults to `clean`. An optional `expect_clean`
//! field (the `clean_text` the same row produced historically) is ignored by
//! the pass/fail verdict — small-model output is not stable enough across
//! builds to assert on verbatim — but is printed so a control run can be
//! eyeballed for "still cleaned, not silently falling back to raw".
//!
//! Runs against the machine's REAL dictionary (`dictionary::load_or_seed`),
//! because the term list is exactly the input that triggers the bug.
//!
//! Exits non-zero if any input echoed the glossary, so it works as a gate.

use std::sync::atomic::AtomicBool;

use flow_core::cleanup::{CleanupContext, CleanupProvider, Mode};
use flow_core::dictionary;

#[derive(serde::Deserialize)]
struct Row {
    raw_text: String,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    expect_clean: Option<String>,
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).expect("usage: cleanup_replay <corpus.jsonl>");
    let corpus = std::fs::read_to_string(&path)?;
    let rows: Vec<Row> = corpus
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;

    let dict = dictionary::load_or_seed().unwrap_or_default();
    let terms: Vec<String> = dict.iter().map(|d| d.term.clone()).collect();
    eprintln!("dictionary  : {} terms", terms.len());

    let model_path = flow_core::models::cleanup_model_path()?;
    let provider = flow_core::cleanup::LlamaCleanupProvider::load(&model_path)?;
    eprintln!("model load  : {:.2}s", provider.load_time.as_secs_f64());

    // Untimed warm-up, same rationale as `clean_test`: forces the one-time
    // Metal kernel-pipeline JIT outside the measured replay.
    let cancel = AtomicBool::new(false);
    let _ = provider.clean("this is a warm up call", Mode::Clean, &CleanupContext::default(), &cancel);

    let mut echoes = 0usize;
    let mut fallbacks = 0usize;
    for (i, row) in rows.iter().enumerate() {
        let ctx = CleanupContext {
            app_name: None,
            tone: "neutral".to_string(),
            dictionary_terms: terms.clone(),
        };
        let mode = Mode::parse(row.mode.as_deref().unwrap_or("clean"));
        // The corpus holds `raw_text` straight from history, which is already
        // dictionary-corrected — the same string the coordinator hands to
        // cleanup (`coordinator.rs`: `dictionary::correct` runs first) — so
        // no extra correction pass here.
        let cancel = AtomicBool::new(false);
        let out = provider.clean(&row.raw_text, mode, &ctx, &cancel)?;

        // Judge the *generation*, not the guard: `clean()` converts a
        // rejected echo into an empty string (the "no usable output → raw
        // fallback" contract every caller shares), so an empty result for a
        // non-empty input is exactly what a working guard looks like, and a
        // surviving echo is what a broken one looks like. Both are counted.
        let echoed = flow_core::cleanup::is_glossary_echo(&out, &terms);
        let fell_back = out.trim().is_empty() && !row.raw_text.trim().is_empty();
        if echoed {
            echoes += 1;
        }
        if fell_back {
            fallbacks += 1;
        }

        let verdict = if echoed {
            "ECHO"
        } else if fell_back {
            "fallback"
        } else {
            "ok"
        };
        println!("[{:>3}] {:<8} in={:?}", i, verdict, truncate(&row.raw_text, 60));
        println!("            out={:?}", truncate(&out, 90));
        if let Some(expected) = &row.expect_clean {
            if expected.trim() != out.trim() {
                println!("            was={:?}", truncate(expected, 90));
            }
        }
    }

    println!();
    println!("echoes    : {}/{}", echoes, rows.len());
    println!("fallbacks : {}/{}", fallbacks, rows.len());

    // Drop the provider (and with it the llama.cpp model, backend and Metal
    // residency sets) BEFORE exiting. `std::process::exit` skips destructors,
    // and tearing the process down with a live Metal context trips ggml's
    // `GGML_ASSERT([rsets->data count] == 0)` — which aborts with 134 and
    // makes this harness's exit code useless as a gate.
    drop(provider);
    if echoes > 0 {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let head: String = s.chars().take(n).collect();
    format!("{head}…")
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
fn main() {
    eprintln!("cleanup_replay needs the embedded llama.cpp provider (Apple Silicon macOS only)");
    std::process::exit(2);
}
