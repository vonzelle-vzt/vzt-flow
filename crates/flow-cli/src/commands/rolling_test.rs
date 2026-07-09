//! Hidden `flow rolling-test <file>` — drives a wav through the rolling
//! transcription pipeline in simulated real time and reports the latency win
//! versus the batch (transcribe-at-release) path.
//!
//! It feeds the audio to a [`flow_core::rolling`] worker slice-by-slice, pacing
//! each push to wall-clock (so chunks dispatch at the same offsets a live
//! recording would produce), then signals "release" and measures how long the
//! final tail takes to come back. The batch baseline is a plain
//! [`transcribe_long`] over the whole clip — what the app used to do only after
//! the user let go. Reuses the exact production engine path (the shared
//! `model_manager` + `rolling` modules), so the numbers reflect real behavior.

use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::Result;
use flow_core::model_manager::{self, ModelCommand};
use flow_core::rolling::{spawn_rolling_worker, RollingInput, RollingOutput};
use flow_core::{parakeet_model_dir, transcribe_long, ParakeetTranscriber};

use super::load_audio_as_f32;

/// Real-time feed slice — half a second of 16 kHz mono audio per push.
const FEED_SLICE_SAMPLES: usize = 8_000;
const SAMPLE_RATE: usize = 16_000;

pub fn run(file: &Path, speed: f64, skip_batch: bool) -> Result<()> {
    let speed = if speed <= 0.0 { 1.0 } else { speed };
    println!("Loading audio: {}", file.display());
    let (samples, duration) = load_audio_as_f32(file)?;
    println!("Audio duration: {:.2}s ({} samples @ 16kHz)", duration.as_secs_f64(), samples.len());
    println!("Feed speed: {speed}x real time\n");

    let model_dir = parakeet_model_dir()?;

    // ---- Batch baseline: transcribe the whole clip in one shot (what the app
    // did only after release). This is the "end-latency before". ----
    if !skip_batch {
        println!("== Batch baseline (transcribe-at-release) ==");
        let mut engine = ParakeetTranscriber::load(&model_dir)?;
        let started = Instant::now();
        let batch = transcribe_long(&samples, &mut engine)?;
        let batch_time = started.elapsed();
        println!(
            "batch transcription wall time : {:.2}s  (RTF {:.3})",
            batch_time.as_secs_f64(),
            batch_time.as_secs_f64() / duration.as_secs_f64().max(0.001)
        );
        println!("batch transcript chars        : {}", batch.text.chars().count());
        println!(
            "=> end-latency BEFORE rolling  : {:.2}s (paid entirely after release)\n",
            batch_time.as_secs_f64()
        );
        drop(engine); // free the model before the rolling run loads its own
    }

    // ---- Rolling run: feed in simulated real time, measure tail latency. ----
    println!("== Rolling run (transcribe-during-recording) ==");
    let (model_cmd_tx, model_cmd_rx) = mpsc::channel::<ModelCommand>();
    let (status_tx, _status_rx) = mpsc::channel();
    model_manager::spawn(
        model_dir,
        Duration::from_secs(3600),
        model_cmd_rx,
        status_tx,
    );

    let (out_tx, out_rx) = mpsc::channel::<RollingOutput>();
    let rolling_in = spawn_rolling_worker(model_cmd_tx, out_tx);

    let feed_start = Instant::now();
    let mut chunk_completions: Vec<(usize, f64)> = Vec::new(); // (chunk#, offset s)

    for slice in samples.chunks(FEED_SLICE_SAMPLES) {
        let _ = rolling_in.send(RollingInput::Samples(slice.to_vec()));
        // Pace to wall-clock: this slice represents slice.len()/16000 seconds.
        let slice_secs = slice.len() as f64 / SAMPLE_RATE as f64;
        std::thread::sleep(Duration::from_secs_f64(slice_secs / speed));
        // Collect any chunk previews that completed while recording continued.
        while let Ok(msg) = out_rx.try_recv() {
            if let RollingOutput::Preview { chunk_text } = msg {
                let n = chunk_completions.len() + 1;
                let off = feed_start.elapsed().as_secs_f64();
                println!(
                    "  chunk {n:>2} transcribed at recording-offset {off:>6.2}s  ({} chars)",
                    chunk_text.chars().count()
                );
                chunk_completions.push((n, off));
            }
        }
    }

    // "Release": everything is fed, now measure how long only the tail takes.
    let release_off = feed_start.elapsed().as_secs_f64();
    println!("  -- release at {release_off:.2}s --");
    let _ = rolling_in.send(RollingInput::Finish);

    let release_instant = Instant::now();
    let (final_text, tail_latency) = loop {
        match out_rx.recv() {
            Ok(RollingOutput::Preview { chunk_text }) => {
                // A chunk that finished right around release; count it.
                let n = chunk_completions.len() + 1;
                let off = feed_start.elapsed().as_secs_f64();
                println!("  chunk {n:>2} transcribed at recording-offset {off:>6.2}s  ({} chars)", chunk_text.chars().count());
                chunk_completions.push((n, off));
            }
            Ok(RollingOutput::Final { raw_text, .. }) => {
                break (raw_text, release_instant.elapsed().as_secs_f64());
            }
            Err(_) => {
                anyhow::bail!("rolling worker exited without producing a final transcript");
            }
        }
    };

    let total = feed_start.elapsed().as_secs_f64();
    println!();
    println!("chunks transcribed during recording : {}", chunk_completions.len());
    println!("final transcript chars              : {}", final_text.chars().count());
    println!(
        "=> end-latency AFTER rolling         : {tail_latency:.2}s (only the <35s tail runs after release)"
    );
    println!("total wall (feed + tail)            : {total:.2}s");
    println!();
    println!("--- Assembled transcript (raw, pre-cleanup) ---");
    println!("{final_text}");
    println!("-----------------------------------------------");

    Ok(())
}
