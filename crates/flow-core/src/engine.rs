//! Speech-to-text engine abstraction.
//!
//! Kept as a small trait so alternative engines (e.g. Whisper via
//! transcribe-rs's `whisper-cpp` feature) can be dropped in later without
//! touching call sites in flow-cli.

use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};
use transcribe_rs::onnx::parakeet::{ParakeetModel, ParakeetParams, TimestampGranularity};
use transcribe_rs::onnx::Quantization;

/// A single transcribed segment with timing, when the engine provides it.
#[derive(Debug, Clone)]
pub struct TranscriptSegment {
    pub start: f32,
    pub end: f32,
    pub text: String,
}

/// Result of a transcription call.
#[derive(Debug, Clone)]
pub struct Transcript {
    pub text: String,
    pub segments: Option<Vec<TranscriptSegment>>,
}

/// Engine-agnostic transcription trait. Implementations own their model
/// state and may require `&mut self` because inference sessions are not
/// guaranteed to be safely re-entrant.
pub trait Transcriber: Send {
    fn transcribe(&mut self, samples: &[f32]) -> Result<Transcript>;
}

/// Parakeet TDT 0.6B v3 (ONNX, int8) via transcribe-rs.
pub struct ParakeetTranscriber {
    model: ParakeetModel,
    /// Wall time taken by `ParakeetModel::load`.
    pub load_time: std::time::Duration,
}

impl ParakeetTranscriber {
    /// Load the model from `model_dir` (expects the encoder/decoder/
    /// preprocessor ONNX files + vocab.txt produced by `flow models
    /// download`).
    pub fn load(model_dir: &Path) -> Result<Self> {
        if !model_dir.exists() {
            anyhow::bail!(
                "model directory {} does not exist. Run `flow models download parakeet-v3` first.",
                model_dir.display()
            );
        }
        let started = Instant::now();
        let model = ParakeetModel::load(model_dir, &Quantization::Int8)
            .with_context(|| format!("failed to load Parakeet model from {}", model_dir.display()))?;
        let load_time = started.elapsed();
        Ok(Self { model, load_time })
    }
}

impl Transcriber for ParakeetTranscriber {
    fn transcribe(&mut self, samples: &[f32]) -> Result<Transcript> {
        let params = ParakeetParams {
            timestamp_granularity: Some(TimestampGranularity::Segment),
            ..Default::default()
        };
        let result = self
            .model
            .transcribe_with(samples, &params)
            .context("Parakeet transcription failed")?;

        let segments = result.segments.map(|segs| {
            segs.into_iter()
                .map(|s| TranscriptSegment {
                    start: s.start,
                    end: s.end,
                    text: s.text,
                })
                .collect()
        });

        Ok(Transcript {
            text: result.text,
            segments,
        })
    }
}
