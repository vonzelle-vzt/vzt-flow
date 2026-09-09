//! The per-chunk meeting pipeline, extracted from the macOS-only session
//! worker so it is unit-testable (and offline-measurable) on every platform.
//! The engine is injected as a closure: this module never touches ONNX,
//! ScreenCaptureKit, or cpal.
//!
//! The order of operations here is the order the live worker used before the
//! extraction and must stay that way — accuracy work downstream measures
//! itself against a baseline taken through this code.

use std::collections::VecDeque;
use std::sync::Arc;

use crate::dictionary;

use super::dedup::{is_echo, time_overlaps, DEFAULT_ECHO_THRESHOLD};
use super::transcriber::{Chunk, Source};

/// How long a transcribed segment stays eligible for echo comparison. A
/// `Me:` chunk is only ever dropped as an echo of a `Them:` chunk it
/// overlaps in time with, so we only need to retain very recent history.
pub const DEDUP_RETAIN_SECS: f32 = 30.0;

/// One transcribed, dictionary-corrected line, retained briefly for the
/// echo-dedup time/word comparison.
struct RecordedSeg {
    source: Source,
    start: f32,
    end: f32,
    text: String,
}

/// What the pipeline decided about one chunk.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Write this line.
    Emit { start: f32, source: Source, text: String },
    /// Dropped as an echo of an overlapping `Them` line (text kept for the log).
    DroppedEcho(String),
    /// The chunk was not worth transcribing / produced nothing.
    Skipped(SkipReason),
    /// The engine errored (text is the error).
    Failed(String),
}

/// Why a chunk produced no transcript line.
#[derive(Debug, Clone, PartialEq)]
pub enum SkipReason {
    /// No frame in the chunk cleared the speech threshold (dead air flushed
    /// by the 30s cap).
    NoSpeech,
    /// The chunk carried no audio at all.
    EmptySamples,
    /// The engine returned nothing but whitespace.
    EmptyText,
}

/// The per-chunk meeting pipeline: resample, transcribe, dictionary-correct,
/// echo-dedup. Owns the short window of recent lines the echo check compares
/// against, so it must be driven by a single thread (the live session runs one
/// transcription worker for exactly this reason).
pub struct ChunkPipeline {
    recent: VecDeque<RecordedSeg>,
    dict: Arc<Vec<dictionary::DictionaryTerm>>,
}

impl ChunkPipeline {
    pub fn new(dict: Arc<Vec<dictionary::DictionaryTerm>>) -> Self {
        Self { recent: VecDeque::new(), dict }
    }

    /// Runs one chunk end to end. `transcribe` receives 16 kHz mono samples
    /// and returns the raw engine text.
    pub fn process<F>(&mut self, chunk: &Chunk, transcribe: F) -> Decision
    where
        F: FnOnce(&[f32]) -> anyhow::Result<String>,
    {
        if !chunk.has_speech {
            return Decision::Skipped(SkipReason::NoSpeech); // dead-air chunk flushed by the 30s cap
        }
        if chunk.samples.is_empty() {
            return Decision::Skipped(SkipReason::EmptySamples);
        }
        let start = chunk.start_offset;
        let end = chunk.end_offset();

        // Resample native -> 16 kHz for the engine.
        let samples = crate::audio::resample_linear(
            &chunk.samples,
            chunk.sample_rate,
            crate::audio::TARGET_SAMPLE_RATE,
        );

        let text = match transcribe(&samples) {
            Ok(t) => t.trim().to_string(),
            Err(e) => return Decision::Failed(e.to_string()),
        };
        if text.is_empty() {
            return Decision::Skipped(SkipReason::EmptyText);
        }
        let corrected = dictionary::correct(&text, &self.dict);

        // Prune history older than the dedup window relative to this chunk.
        while let Some(front) = self.recent.front() {
            if front.end < start - DEDUP_RETAIN_SECS {
                self.recent.pop_front();
            } else {
                break;
            }
        }

        // Echo dedup: drop a Me line that time-overlaps a recent Them line
        // and is textually near-identical (the no-headphones case).
        if chunk.source == Source::Me {
            let echo = self.recent.iter().any(|seg| {
                seg.source == Source::Them
                    && time_overlaps(start, end, seg.start, seg.end)
                    && is_echo(&corrected, &seg.text, DEFAULT_ECHO_THRESHOLD)
            });
            if echo {
                return Decision::DroppedEcho(corrected);
            }
        }

        self.recent.push_back(RecordedSeg {
            source: chunk.source,
            start,
            end,
            text: corrected.clone(),
        });
        Decision::Emit { start, source: chunk.source, text: corrected }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// A chunk of `len_secs` of (unused) 16 kHz audio starting at `start`.
    /// The samples are never analysed by the pipeline — the injected
    /// transcriber decides the text — so silence is fine here.
    fn chunk_at(source: Source, start: f32, len_secs: f32) -> Chunk {
        let sample_rate = 16_000u32;
        Chunk {
            source,
            samples: vec![0.0; (len_secs * sample_rate as f32) as usize],
            sample_rate,
            start_offset: start,
            has_speech: true,
            ..Default::default()
        }
    }

    /// A transcriber that always answers `text`.
    fn says(text: &str) -> impl FnOnce(&[f32]) -> anyhow::Result<String> + '_ {
        move |_| Ok(text.to_string())
    }

    #[test]
    fn emits_a_dictionary_corrected_line_for_a_speech_chunk() {
        let dict = Arc::new(vec![dictionary::DictionaryTerm {
            term: "Supabase".to_string(),
            hints: Vec::new(),
        }]);
        let mut pipeline = ChunkPipeline::new(dict);

        let decision = pipeline.process(
            &chunk_at(Source::Them, 12.0, 4.0),
            says("  we moved it to supabase  "),
        );

        assert_eq!(
            decision,
            Decision::Emit {
                start: 12.0,
                source: Source::Them,
                text: "we moved it to Supabase".to_string(),
            }
        );
    }

    #[test]
    fn skips_a_chunk_with_no_speech() {
        let mut pipeline = ChunkPipeline::new(Arc::new(Vec::new()));
        let mut chunk = chunk_at(Source::Them, 0.0, 3.0);
        chunk.has_speech = false;

        let called = Cell::new(false);
        let decision = pipeline.process(&chunk, |_| {
            called.set(true);
            Ok("should never run".to_string())
        });

        assert_eq!(decision, Decision::Skipped(SkipReason::NoSpeech));
        assert!(!called.get(), "a no-speech chunk must not reach the engine");
    }

    #[test]
    fn skips_a_chunk_with_empty_samples() {
        let mut pipeline = ChunkPipeline::new(Arc::new(Vec::new()));
        let mut chunk = chunk_at(Source::Them, 0.0, 3.0);
        chunk.samples.clear();

        let called = Cell::new(false);
        let decision = pipeline.process(&chunk, |_| {
            called.set(true);
            Ok("should never run".to_string())
        });

        assert_eq!(decision, Decision::Skipped(SkipReason::EmptySamples));
        assert!(!called.get(), "an empty chunk must not reach the engine");
    }

    #[test]
    fn skips_an_empty_transcription() {
        let mut pipeline = ChunkPipeline::new(Arc::new(Vec::new()));

        let decision = pipeline.process(&chunk_at(Source::Them, 0.0, 3.0), says("   \n  "));

        assert_eq!(decision, Decision::Skipped(SkipReason::EmptyText));
    }

    #[test]
    fn reports_an_engine_error_without_recording_it() {
        let mut pipeline = ChunkPipeline::new(Arc::new(Vec::new()));

        let decision = pipeline
            .process(&chunk_at(Source::Them, 0.0, 3.0), |_| anyhow::bail!("model exploded"));

        assert_eq!(decision, Decision::Failed("model exploded".to_string()));
        assert!(pipeline.recent.is_empty());
    }

    #[test]
    fn drops_a_me_chunk_that_echoes_an_overlapping_them_chunk() {
        let mut pipeline = ChunkPipeline::new(Arc::new(Vec::new()));
        let line = "so the migration lands on thursday";

        let them = pipeline.process(&chunk_at(Source::Them, 0.0, 5.0), says(line));
        assert!(matches!(them, Decision::Emit { .. }));

        // Speaker heard through laptop speakers, picked up by the mic 1s later.
        let me = pipeline.process(&chunk_at(Source::Me, 1.0, 5.0), says(line));

        assert_eq!(me, Decision::DroppedEcho(line.to_string()));
        assert_eq!(pipeline.recent.len(), 1, "a dropped echo is not retained");
    }

    #[test]
    fn keeps_a_me_chunk_that_does_not_overlap_in_time() {
        let mut pipeline = ChunkPipeline::new(Arc::new(Vec::new()));
        let line = "so the migration lands on thursday";

        pipeline.process(&chunk_at(Source::Them, 0.0, 5.0), says(line));
        // Same words, but spoken well after the Them line ended.
        let me = pipeline.process(&chunk_at(Source::Me, 10.0, 5.0), says(line));

        assert_eq!(
            me,
            Decision::Emit { start: 10.0, source: Source::Me, text: line.to_string() }
        );
    }

    #[test]
    fn keeps_a_short_backchannel() {
        let mut pipeline = ChunkPipeline::new(Arc::new(Vec::new()));

        pipeline.process(&chunk_at(Source::Them, 0.0, 5.0), says("yeah"));
        // Overlapping and textually identical, but under MIN_TOKENS_FOR_ECHO:
        // agreeing at the same moment is not an echo.
        let me = pipeline.process(&chunk_at(Source::Me, 1.0, 2.0), says("yeah"));

        assert_eq!(
            me,
            Decision::Emit { start: 1.0, source: Source::Me, text: "yeah".to_string() }
        );
    }

    #[test]
    fn prunes_history_older_than_the_retain_window() {
        let mut pipeline = ChunkPipeline::new(Arc::new(Vec::new()));

        // Ends at 5.0.
        pipeline.process(&chunk_at(Source::Them, 0.0, 5.0), says("first line here"));
        assert_eq!(pipeline.recent.len(), 1);

        // 5.0 is not older than 30.0 - DEDUP_RETAIN_SECS, so both survive.
        pipeline.process(&chunk_at(Source::Them, 30.0, 5.0), says("second line here"));
        assert_eq!(pipeline.recent.len(), 2);

        // 5.0 < 40.0 - DEDUP_RETAIN_SECS: the first line ages out.
        pipeline.process(&chunk_at(Source::Them, 40.0, 5.0), says("third line here"));
        assert_eq!(pipeline.recent.len(), 2);
        assert_eq!(pipeline.recent.front().map(|s| s.start), Some(30.0));
    }

    #[test]
    fn never_drops_a_them_chunk_as_an_echo() {
        let mut pipeline = ChunkPipeline::new(Arc::new(Vec::new()));
        let line = "so the migration lands on thursday";

        pipeline.process(&chunk_at(Source::Them, 0.0, 5.0), says(line));
        // A second Them chunk that overlaps and repeats it word for word is
        // still written: dedup only ever suppresses the mic side.
        let them = pipeline.process(&chunk_at(Source::Them, 1.0, 5.0), says(line));

        assert_eq!(
            them,
            Decision::Emit { start: 1.0, source: Source::Them, text: line.to_string() }
        );
    }
}
