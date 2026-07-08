//! Hidden diagnostic: runs the LLM cleanup pass on arbitrary text outside
//! the desktop app, racing it against the same deadline `cleanup_manager`
//! uses, and reporting which path won plus the latency.
//!
//! Runs an untimed warm-up generation first (mirroring what the coordinator
//! does when a recording *starts*, in parallel with the user speaking) so
//! the reported latency reflects steady-state behavior — model load and the
//! one-time-per-process Metal kernel-pipeline JIT compilation are real
//! costs (several seconds on first use) but they're not supposed to be
//! paid out of the 2.5s deadline in normal operation.

use std::sync::atomic::AtomicBool;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::Result;
use flow_core::cleanup::{CleanupContext, CleanupProvider, Mode};
use flow_core::dictionary;

#[cfg(target_os = "macos")]
fn load_provider() -> Result<Box<dyn CleanupProvider>> {
    let path = flow_core::models::cleanup_model_path()?;
    Ok(Box::new(flow_core::cleanup::LlamaCleanupProvider::load(&path)?))
}

#[cfg(not(target_os = "macos"))]
fn load_provider() -> Result<Box<dyn CleanupProvider>> {
    anyhow::bail!("embedded llama.cpp cleanup provider is only implemented for macOS")
}

pub fn run(text: &str, mode: &str, timeout_ms: u64) -> Result<()> {
    let mode = Mode::parse(mode);
    println!("mode        : {}", mode.label());
    println!("input       : {text:?}");
    println!("timeout_ms  : {timeout_ms}");

    if mode == Mode::Raw {
        println!("output      : {text:?} (raw mode never touches the LLM)");
        return Ok(());
    }

    let load_started = Instant::now();
    let provider: std::sync::Arc<dyn CleanupProvider> = match load_provider() {
        Ok(p) => std::sync::Arc::from(p),
        Err(e) => {
            println!("model load  : FAILED ({e})");
            println!("output      : {text:?} (passthrough — no model)");
            return Ok(());
        }
    };
    println!("model load  : {:.2}s", load_started.elapsed().as_secs_f64());

    let dict = dictionary::load_or_seed().unwrap_or_default();
    let ctx = CleanupContext {
        app_name: None,
        tone: "neutral".to_string(),
        dictionary_terms: dict.iter().map(|d| d.term.clone()).collect(),
    };

    // Untimed warm-up: forces Metal kernel-pipeline JIT compilation (a
    // one-time per-process cost, several seconds cold) outside the timed
    // section below — exactly what `coordinator::start_recording` triggers
    // in the desktop app while the user is still talking.
    let warmup_started = Instant::now();
    let warmup_cancel = AtomicBool::new(false);
    match provider.clean("this is a warm up call", Mode::Clean, &CleanupContext::default(), &warmup_cancel) {
        Ok(_) => println!("warm-up     : {:.2}s", warmup_started.elapsed().as_secs_f64()),
        Err(e) => println!("warm-up     : FAILED (non-fatal): {e}"),
    }

    // Match the daemon pipeline: dictionary correction runs BEFORE cleanup.
    let text_owned = dictionary::correct(text, &dict);
    if text_owned != text {
        println!("dictionary  : {text_owned:?}");
    }
    let (tx, rx) = mpsc::channel();
    let cancel = std::sync::Arc::new(AtomicBool::new(false));
    let cancel_for_gen = cancel.clone();
    let gen_started = Instant::now();
    let handle = std::thread::spawn(move || {
        let result = provider.clean(&text_owned, mode, &ctx, &cancel_for_gen);
        let _ = tx.send(result);
    });

    match rx.recv_timeout(Duration::from_millis(timeout_ms)) {
        Ok(Ok(cleaned)) if !cleaned.trim().is_empty() => {
            println!("latency     : {:.3}s (llm path won)", gen_started.elapsed().as_secs_f64());
            println!("output      : {cleaned:?}");
            let _ = handle.join();
        }
        Ok(Ok(_empty)) => {
            println!("latency     : {:.3}s (llm produced empty output)", gen_started.elapsed().as_secs_f64());
            println!("output      : {text:?} (raw fallback)");
            let _ = handle.join();
        }
        Ok(Err(e)) => {
            println!("latency     : {:.3}s (generation error: {e})", gen_started.elapsed().as_secs_f64());
            println!("output      : {text:?} (raw fallback)");
            let _ = handle.join();
        }
        Err(_) => {
            // Deadline hit: cancel cooperatively, give the worker a short
            // grace period, then join it — never leave it detached (a
            // detached Metal-backed generation thread previously crashed
            // the process at exit).
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            let _ = rx.recv_timeout(Duration::from_millis(1500));
            println!(
                "latency     : >{:.3}s ({timeout_ms}ms DEADLINE EXCEEDED — cancelled generation, falling \
                 back to raw)",
                gen_started.elapsed().as_secs_f64()
            );
            println!("output      : {text:?} (raw fallback — deadline path)");
            let _ = handle.join();
        }
    }

    Ok(())
}
