use std::time::Instant;

use anyhow::Result;
use flow_core::{parakeet_model_dir, AudioRecorder, ParakeetTranscriber, Transcriber};

pub fn run(seconds: Option<u64>) -> Result<()> {
    let (samples, duration) = AudioRecorder::record_until_enter(seconds)?;
    println!("Captured audio duration: {:.2}s", duration.as_secs_f64());

    if samples.is_empty() {
        println!("No audio captured.");
        return Ok(());
    }

    let model_dir = parakeet_model_dir()?;
    println!("Loading Parakeet model from {}...", model_dir.display());
    let mut engine = ParakeetTranscriber::load(&model_dir)?;
    println!("Model load time: {:.2}s", engine.load_time.as_secs_f64());

    let started = Instant::now();
    let transcript = engine.transcribe(&samples)?;
    let elapsed = started.elapsed();

    let rtf = if duration.as_secs_f64() > 0.0 {
        elapsed.as_secs_f64() / duration.as_secs_f64()
    } else {
        0.0
    };

    println!("\n--- Transcript ---");
    println!("{}", transcript.text);
    println!("------------------\n");
    println!(
        "Transcription wall time: {:.3}s | audio: {:.2}s | realtime factor: {:.3}x",
        elapsed.as_secs_f64(),
        duration.as_secs_f64(),
        rtf
    );

    Ok(())
}
