use std::path::Path;
use std::time::Instant;

use anyhow::Result;
use flow_core::{dictionary, parakeet_model_dir, transcribe_long, ParakeetTranscriber};

use super::listen::apply_standalone_pipeline;
use super::load_audio_as_f32;

pub fn run(file: &Path, mode: Option<&str>) -> Result<()> {
    println!("Loading audio: {}", file.display());
    let (samples, duration) = load_audio_as_f32(file)?;
    println!("Audio duration: {:.2}s", duration.as_secs_f64());

    let model_dir = parakeet_model_dir()?;
    println!("Loading Parakeet model from {}...", model_dir.display());
    let mut engine = ParakeetTranscriber::load(&model_dir)?;
    println!("Model load time: {:.2}s", engine.load_time.as_secs_f64());

    let started = Instant::now();
    // Route through the chunker so multi-minute clips don't OOM the engine;
    // audio ≤35s still takes the single-pass path inside `transcribe_long`.
    let transcript = transcribe_long(&samples, &mut engine)?;
    let elapsed = started.elapsed();

    let rtf = if duration.as_secs_f64() > 0.0 {
        elapsed.as_secs_f64() / duration.as_secs_f64()
    } else {
        0.0
    };

    let dict = dictionary::load_or_seed().unwrap_or_default();
    let corrected = dictionary::correct(&transcript.text, &dict);

    let final_text = match mode {
        Some(m) => apply_standalone_pipeline(&corrected, m),
        None => corrected,
    };

    println!("\n--- Transcript ---");
    println!("{final_text}");
    println!("------------------\n");
    println!(
        "Transcription wall time: {:.3}s | audio: {:.2}s | realtime factor: {:.3}x",
        elapsed.as_secs_f64(),
        duration.as_secs_f64(),
        rtf
    );

    if let Some(segments) = transcript.segments {
        println!("\nSegments:");
        for seg in segments {
            println!("  [{:.2}s - {:.2}s] {}", seg.start, seg.end, seg.text);
        }
    }

    Ok(())
}
