//! Ad-hoc measurement tool (not part of the CLI): loads `ParakeetTranscriber`
//! once in a single process and times four things, so first-inference cost
//! can be separated from load cost and from later cache-warm inference:
//!
//! - `load_secs`       — `ParakeetTranscriber::load`'s wall time
//! - `infer1_x_secs`   — the very first inference this engine ever runs
//! - `infer2_x_secs`   — the same clip again (page-cache / shape warm)
//! - `infer_y_secs`    — a *different-length* clip, after both x runs
//!
//! plus RSS immediately after load and RSS peak (sampled after every
//! inference), via `ps -o rss=` — no new dependency.
//!
//! This exists to answer whether transcribe-rs/CoreML has a first-run "JIT"
//! cost separate from `load_time`, and if so whether it is shape-independent
//! (see docs/PRD.md's ASR warm-up analysis). It is not wired into `flow` and
//! is not covered by the test suite; run it by hand.
//!
//! Usage:
//!
//! ```text
//! cargo run --release --example asr_bench -- <clip_x.wav> <clip_y.wav>
//! ```
//!
//! Both clips must be <= 35s: transcribe-rs's Parakeet path has no internal
//! chunking and is quadratic in audio length (measured OOM at ~146s on this
//! repo's dev machine — see CLAUDE.md gotcha (b) and docs/PRD.md). This tool
//! calls `transcribe()` directly, not `transcribe_long`, precisely so the
//! four timings are not confounded by the chunker — so it refuses longer
//! clips rather than silently doing the wrong thing.

use std::time::Instant;

use anyhow::{bail, Context, Result};
use flow_core::audio::load_audio_file_as_f32;
use flow_core::{parakeet_model_dir, ParakeetTranscriber, Transcriber};

const MAX_CLIP_SECS: f64 = 35.0;

fn rss_kb() -> Result<u64> {
    let pid = std::process::id();
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p"])
        .arg(pid.to_string())
        .output()
        .context("failed to shell out to `ps` for RSS")?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.trim()
        .parse::<u64>()
        .with_context(|| format!("could not parse `ps` output as RSS kb: {text:?}"))
}

fn load_clip(label: &str, path: &std::path::Path) -> Result<Vec<f32>> {
    let (samples, duration) = load_audio_file_as_f32(path)
        .with_context(|| format!("failed to load {label} clip {}", path.display()))?;
    if duration.as_secs_f64() > MAX_CLIP_SECS {
        bail!(
            "{label} clip {} is {:.1}s, over the {:.0}s single-pass ceiling (CLAUDE.md gotcha b) \
             — asr_bench calls transcribe() directly and refuses to risk the quadratic-memory OOM. \
             Use a shorter clip.",
            path.display(),
            duration.as_secs_f64(),
            MAX_CLIP_SECS
        );
    }
    Ok(samples)
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let x_path = args
        .next()
        .map(std::path::PathBuf::from)
        .context("usage: asr_bench <clip_x.wav> <clip_y.wav>")?;
    let y_path = args
        .next()
        .map(std::path::PathBuf::from)
        .context("usage: asr_bench <clip_x.wav> <clip_y.wav>")?;

    let x_samples = load_clip("x", &x_path)?;
    let y_samples = load_clip("y", &y_path)?;

    let model_dir = parakeet_model_dir()?;
    let mut engine = ParakeetTranscriber::load(&model_dir)?;
    let load_secs = engine.load_time.as_secs_f64();

    let mut rss_peak_kb = rss_kb()?;
    let rss_after_load_kb = rss_peak_kb;

    let started = Instant::now();
    let _ = engine.transcribe(&x_samples).context("infer1_x failed")?;
    let infer1_x_secs = started.elapsed().as_secs_f64();
    rss_peak_kb = rss_peak_kb.max(rss_kb()?);

    let started = Instant::now();
    let _ = engine.transcribe(&x_samples).context("infer2_x failed")?;
    let infer2_x_secs = started.elapsed().as_secs_f64();
    rss_peak_kb = rss_peak_kb.max(rss_kb()?);

    let started = Instant::now();
    let _ = engine.transcribe(&y_samples).context("infer_y failed")?;
    let infer_y_secs = started.elapsed().as_secs_f64();
    rss_peak_kb = rss_peak_kb.max(rss_kb()?);

    println!("load_secs={load_secs:.3}");
    println!("infer1_x_secs={infer1_x_secs:.3}");
    println!("infer2_x_secs={infer2_x_secs:.3}");
    println!("infer_y_secs={infer_y_secs:.3}");
    println!("rss_after_load_kb={rss_after_load_kb}");
    println!("rss_peak_kb={rss_peak_kb}");

    Ok(())
}
