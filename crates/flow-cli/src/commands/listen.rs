//! `flow listen`: daemon-first, standalone fallback.
//!
//! Only the final text goes to stdout (so `flow listen | pbcopy` works);
//! every diagnostic (recording status, model load, timing) goes to stderr.

use std::time::{Duration, Instant};

use anyhow::Result;
use flow_core::ipc::Request;
use flow_core::{dictionary, parakeet_model_dir, AudioRecorder, ParakeetTranscriber, Transcriber};

use super::daemon_client;

pub fn run(mode: Option<String>, max_secs: Option<u64>) -> Result<()> {
    if daemon_client::is_daemon_running() {
        return run_via_daemon(mode, max_secs);
    }
    eprintln!("[vzt-flow] no daemon running; using standalone capture");
    run_standalone(mode, max_secs)
}

fn run_via_daemon(mode: Option<String>, max_secs: Option<u64>) -> Result<()> {
    let req = Request::Listen { mode, timeout_secs: None, max_secs };
    // Generous read timeout: recording (up to max_secs, default 300s in the
    // daemon's own config) plus transcription plus cleanup, with headroom.
    let budget = max_secs.unwrap_or(300) + 60;
    let resp = daemon_client::call_required(&req, Some(Duration::from_secs(budget)))?;
    if !resp.ok {
        anyhow::bail!("daemon error: {}", resp.error.as_deref().unwrap_or("unknown error"));
    }
    eprintln!(
        "[vzt-flow] daemon captured {:.2}s, mode={}",
        resp.duration_s.unwrap_or(0.0),
        resp.mode.as_deref().unwrap_or("clean")
    );
    println!("{}", resp.text.unwrap_or_default());
    Ok(())
}

fn run_standalone(mode: Option<String>, max_secs: Option<u64>) -> Result<()> {
    let (samples, duration) = AudioRecorder::record_until_enter(max_secs)?;
    eprintln!("[vzt-flow] captured audio duration: {:.2}s", duration.as_secs_f64());

    if samples.is_empty() {
        eprintln!("[vzt-flow] no audio captured");
        println!();
        return Ok(());
    }

    let model_dir = parakeet_model_dir()?;
    eprintln!("[vzt-flow] loading Parakeet model from {}...", model_dir.display());
    let mut engine = ParakeetTranscriber::load(&model_dir)?;
    eprintln!("[vzt-flow] model load time: {:.2}s", engine.load_time.as_secs_f64());

    let started = Instant::now();
    let transcript = engine.transcribe(&samples)?;
    let elapsed = started.elapsed();
    let rtf = if duration.as_secs_f64() > 0.0 { elapsed.as_secs_f64() / duration.as_secs_f64() } else { 0.0 };
    eprintln!(
        "[vzt-flow] transcription wall time: {:.3}s | audio: {:.2}s | realtime factor: {:.3}x",
        elapsed.as_secs_f64(),
        duration.as_secs_f64(),
        rtf
    );

    let dict = dictionary::load_or_seed().unwrap_or_default();
    let corrected = dictionary::correct(&transcript.text, &dict);

    let mode = mode.unwrap_or_else(|| "clean".to_string());
    let final_text = apply_standalone_pipeline(&corrected, &mode);

    println!("{final_text}");
    Ok(())
}

/// Standalone (no-daemon) post-processing: code mode is deterministic and
/// always available; cleanup (clean/polish) only runs if the cleanup GGUF
/// is actually present on disk, since there's no daemon here to have
/// warmed it up — otherwise we fall back to the dictionary-corrected text.
/// Shared with `flow transcribe --mode`.
pub(crate) fn apply_standalone_pipeline(corrected: &str, mode: &str) -> String {
    if mode == "code" {
        return flow_core::codemode::transform(corrected);
    }
    if mode == "raw" {
        return corrected.to_string();
    }

    #[cfg(target_os = "macos")]
    {
        use flow_core::cleanup::{CleanupContext, CleanupProvider, LlamaCleanupProvider, Mode};
        use std::sync::atomic::AtomicBool;

        let Ok(cleanup_path) = flow_core::models::cleanup_model_path() else {
            return corrected.to_string();
        };
        if !cleanup_path.exists() {
            eprintln!("[vzt-flow] cleanup model not installed; skipping cleanup (run `flow models download cleanup`)");
            return corrected.to_string();
        }
        eprintln!("[vzt-flow] loading cleanup model for standalone cleanup pass...");
        match LlamaCleanupProvider::load(&cleanup_path) {
            Ok(provider) => {
                let cancel = AtomicBool::new(false);
                let parsed_mode = Mode::parse(mode);
                let ctx = CleanupContext::default();
                match provider.clean(corrected, parsed_mode, &ctx, &cancel) {
                    Ok(text) if !text.trim().is_empty() => text,
                    Ok(_) => corrected.to_string(),
                    Err(e) => {
                        eprintln!("[vzt-flow] cleanup generation failed ({e}); using dictionary-corrected text");
                        corrected.to_string()
                    }
                }
            }
            Err(e) => {
                eprintln!("[vzt-flow] cleanup model failed to load ({e}); using dictionary-corrected text");
                corrected.to_string()
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        corrected.to_string()
    }
}
