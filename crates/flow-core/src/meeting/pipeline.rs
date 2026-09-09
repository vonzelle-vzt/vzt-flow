//! The per-chunk meeting pipeline, extracted from the macOS-only session
//! worker so it is unit-testable (and offline-measurable) on every platform.
//! The engine is injected as a closure: this module never touches ONNX,
//! ScreenCaptureKit, or cpal.
//!
//! The order of operations here is the order the live worker used before the
//! extraction and must stay that way — accuracy work downstream measures
//! itself against a baseline taken through this code.
//!
//! Accuracy changes (min-speech gate, normalization, seam dedup, low-
//! information drop, echo tolerance/containment) are each independently
//! switchable via [`PipelineOptions`], so an offline replay harness can
//! attribute a WER change to one flip at a time. All are `true` in
//! production (`PipelineOptions::default()`); [`PipelineOptions::legacy()`]
//! reproduces the pre-accuracy-work (B0) behaviour exactly.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use crate::chunking;
use crate::dictionary;

use super::dedup::{is_echo_with, overlaps_within, time_overlaps, DEFAULT_ECHO_THRESHOLD, ECHO_TIME_TOLERANCE_SECS};
use super::transcriber::{Chunk, Source, MIN_SPEECH_SECS};

/// How long a transcribed segment stays eligible for echo comparison. A
/// `Me:` chunk is only ever dropped as an echo of a `Them:` chunk it
/// overlaps in time with, so we only need to retain very recent history.
pub const DEDUP_RETAIN_SECS: f32 = 30.0;

/// Peak below which a chunk is considered too quiet for the engine and is
/// gain-scaled before inference. ScreenCaptureKit system audio routinely
/// arrives far below microphone level (see `SourceStats`' peak report), and
/// Parakeet's features are amplitude-sensitive.
pub const NORMALIZE_PEAK_THRESHOLD: f32 = 0.1;
/// Peak a normalized chunk is scaled to. Well under 1.0 so no sample clips.
pub const NORMALIZE_TARGET_PEAK: f32 = 0.5;
/// Ceiling on the applied gain, so a near-silent chunk of pure noise is not
/// amplified into something the engine will hallucinate words from.
pub const NORMALIZE_MAX_GAIN: f32 = 12.0;

/// How long an emitted line is held before it is written. Two jobs: it lets a
/// `Them` chunk that finishes transcribing *after* an overlapping `Me` chunk
/// still veto that `Me` line as an echo (the two sources chunk independently,
/// so completion order is not span order), and it makes the transcript file
/// closer to monotonic in timestamp. The cost is that a crash loses up to
/// this much transcript instead of ~nothing — bounded deliberately small.
///
/// Applies to `Me` lines only. `Them` lines pass straight through
/// unheld: they are never vetoed themselves (only `Me` lines are ever
/// dropped as echoes), and holding them would delay the interview coach and
/// the live notepad by up to this long for no benefit.
pub const LINE_HOLD_SECS: f32 = 5.0;

/// Peak-normalizes `samples` in place when the peak is under
/// [`NORMALIZE_PEAK_THRESHOLD`]. Returns the gain applied (1.0 = untouched).
/// A silent (all-zero) buffer is left untouched and returns 1.0 rather than
/// dividing by zero.
pub fn normalize_for_asr(samples: &mut [f32]) -> f32 {
    let peak = samples.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
    if peak <= 0.0 || peak >= NORMALIZE_PEAK_THRESHOLD {
        return 1.0;
    }
    let gain = (NORMALIZE_TARGET_PEAK / peak).min(NORMALIZE_MAX_GAIN);
    for s in samples.iter_mut() {
        *s *= gain;
    }
    gain
}

/// Vocalizations that carry no lexical content on their own. A single-token
/// transcription of exactly one of these is dropped; the same word inside a
/// longer real sentence is left alone.
const BARE_VOCALIZATIONS: [&str; 5] = ["uh", "um", "mm", "hmm", "mhm"];

/// Normalizes `text` into lowercase, alphanumeric-only word tokens, dropping
/// tokens that go fully empty (pure punctuation).
fn alnum_tokens(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(|w| w.chars().filter(|c| c.is_alphanumeric()).flat_map(|c| c.to_lowercase()).collect::<String>())
        .filter(|w| !w.is_empty())
        .collect()
}

/// Repair a hard-cut seam using at most 20 overlapping words. Exact overlaps
/// preserve the previous line. If at least two preceding words match but the
/// previous final word does not, drop that uncertain final word and keep the
/// successor's complete word. Unrelated text (or a one-word coincidence) cannot
/// justify changing the previous line. Original casing/punctuation is retained.
/// The optional string replaces the entire previous line when a repair occurs.
pub fn seam_repair(prev: &str, next: &str) -> (Option<String>, String) {
    let p: Vec<&str> = prev.split_whitespace().collect();
    let n: Vec<&str> = next.split_whitespace().collect();
    let norm = |w: &&str| w.chars().filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase).collect::<String>();
    let pn: Vec<String> = p.iter().map(norm).collect();
    let nn: Vec<String> = n.iter().map(norm).collect();
    let overlap = |end: usize| {
        (1..=20.min(end).min(nn.len())).rev().find(|&len| {
            pn[end - len..end].iter().zip(&nn[..len])
                .all(|(a, b)| !a.is_empty() && a == b)
        }).unwrap_or(0)
    };
    let exact = overlap(p.len());
    let without_tail = if p.is_empty() { 0 } else { overlap(p.len() - 1) };
    if without_tail >= 2 && without_tail > exact && without_tail < n.len() {
        (Some(p[..p.len() - 1].join(" ")), n[without_tail..].join(" "))
    } else {
        (None, n[exact..].join(" "))
    }
}

/// Whether a transcribed line carries no information and is almost certainly
/// an ASR artifact rather than speech: no alphanumeric token; six or more
/// tokens with two or fewer distinct ones (the repeated-token loop Parakeet
/// falls into on noise); or a single token that is a pure vocalization
/// ("uh", "um", "mm", "hmm", "mhm"). Deliberately narrow — real
/// back-channels ("yeah", "right", "for sure") are speech and must survive,
/// which is the same commitment `dedup::MIN_TOKENS_FOR_ECHO` makes.
pub fn is_low_information(text: &str) -> bool {
    let tokens = alnum_tokens(text);
    if tokens.is_empty() {
        return true;
    }
    if tokens.len() >= 6 {
        let distinct: HashSet<&String> = tokens.iter().collect();
        if distinct.len() <= 2 {
            return true;
        }
    }
    if tokens.len() == 1 && BARE_VOCALIZATIONS.contains(&tokens[0].as_str()) {
        return true;
    }
    false
}

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
    /// `chunk.speech_secs` was under `MIN_SPEECH_SECS` — a blip, not a word.
    TooShort,
    /// The corrected text carried no information (see [`is_low_information`]).
    LowInformation,
}

/// Which accuracy changes are active in a [`ChunkPipeline`]. Every field
/// defaults to `true` (production behaviour); flip individual fields to
/// attribute a WER change to one mechanism at a time, or use
/// [`PipelineOptions::legacy`] to reproduce the pre-accuracy-work pipeline.
#[derive(Clone, Copy, Debug)]
pub struct PipelineOptions {
    /// Gate 1: skip a chunk whose `speech_secs` is under `MIN_SPEECH_SECS`
    /// before it ever reaches the engine.
    pub min_speech: bool,
    /// Gate 2: peak-normalize quiet chunks before transcription.
    pub normalize: bool,
    /// Gate 3: seam-dedup a hard-cap-cut chunk against the previous chunk
    /// from the same source before dictionary correction.
    pub seam_dedup: bool,
    /// Repair uncertain Them tails while both sides of a hard seam are held.
    pub seam_repair: bool,
    /// Gate 4: drop corrected text that carries no information.
    pub low_information: bool,
    /// Echo check: use `overlaps_within` (a small time-tolerance window)
    /// instead of a strict half-open `time_overlaps`.
    pub echo_tolerance: bool,
    /// Echo check: also treat a mic line that is mostly *contained* in the
    /// system-audio line (not just Jaccard-similar) as an echo.
    pub echo_containment: bool,
    /// Echo check: tolerate spelling differences using character-bigram Dice.
    pub echo_fuzzy: bool,
}

impl Default for PipelineOptions {
    fn default() -> Self {
        Self {
            min_speech: true,
            normalize: true,
            seam_dedup: true,
            seam_repair: true,
            low_information: true,
            echo_tolerance: true,
            echo_containment: true,
            echo_fuzzy: true,
        }
    }
}

impl PipelineOptions {
    /// All accuracy gates off — reproduces the B0 extraction's behaviour
    /// exactly, for the replay harness's baseline column.
    pub fn legacy() -> Self {
        Self {
            min_speech: false,
            normalize: false,
            seam_dedup: false,
            seam_repair: false,
            low_information: false,
            echo_tolerance: false,
            echo_containment: false,
            echo_fuzzy: false,
        }
    }
}

/// The per-chunk meeting pipeline: resample, transcribe, dictionary-correct,
/// echo-dedup. Owns the short window of recent lines the echo check compares
/// against, so it must be driven by a single thread (the live session runs one
/// transcription worker for exactly this reason).
pub struct ChunkPipeline {
    recent: VecDeque<RecordedSeg>,
    dict: Arc<Vec<dictionary::DictionaryTerm>>,
    opts: PipelineOptions,
    /// Raw (pre-dedup, pre-dictionary-correction) text of the last chunk seen
    /// from each source, for seam dedup. Kept as two plain fields rather than
    /// a `HashMap<Source, _>` because `Source` isn't `Hash` and there are
    /// only ever two sources.
    prev_text_me: String,
    prev_text_them: String,
}

impl ChunkPipeline {
    pub fn new(dict: Arc<Vec<dictionary::DictionaryTerm>>) -> Self {
        Self::with_options(dict, PipelineOptions::default())
    }

    pub fn with_options(dict: Arc<Vec<dictionary::DictionaryTerm>>, opts: PipelineOptions) -> Self {
        Self {
            recent: VecDeque::new(),
            dict,
            opts,
            prev_text_me: String::new(),
            prev_text_them: String::new(),
        }
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
        if self.opts.min_speech && chunk.speech_secs < MIN_SPEECH_SECS {
            return Decision::Skipped(SkipReason::TooShort);
        }
        let start = chunk.start_offset;
        let end = chunk.end_offset();

        // Resample native -> 16 kHz for the engine.
        let mut samples = crate::audio::resample_linear(
            &chunk.samples,
            chunk.sample_rate,
            crate::audio::TARGET_SAMPLE_RATE,
        );

        if self.opts.normalize {
            let peak_before = samples.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
            let gain = normalize_for_asr(&mut samples);
            if gain != 1.0 {
                let peak_after = peak_before * gain;
                eprintln!(
                    "[vzt-flow] normalized {} chunk at {:.1}s: peak {:.3} -> {:.2} (gain {:.1}x)",
                    chunk.source.label(),
                    start,
                    peak_before,
                    peak_after,
                    gain
                );
            }
        }

        let raw = match transcribe(&samples) {
            Ok(t) => t.trim().to_string(),
            Err(e) => return Decision::Failed(e.to_string()),
        };
        if raw.is_empty() {
            return Decision::Skipped(SkipReason::EmptyText);
        }

        // Them seam repair needs both untrimmed lines in LineBuffer. The Me
        // path retains the existing one-sided exact dedup.
        let repair_in_buffer = self.opts.seam_repair && chunk.source == Source::Them;
        let text = if self.opts.seam_dedup && chunk.seam_dedup && !repair_in_buffer {
            let prev = match chunk.source {
                Source::Me => &self.prev_text_me,
                Source::Them => &self.prev_text_them,
            };
            chunking::dedup_seam(prev, &raw)
        } else {
            raw.clone()
        };

        if self.opts.seam_dedup {
            match chunk.source {
                Source::Me => self.prev_text_me = raw.clone(),
                Source::Them => self.prev_text_them = raw.clone(),
            }
        }

        if text.is_empty() {
            return Decision::Skipped(SkipReason::EmptyText);
        }

        let corrected = dictionary::correct(&text, &self.dict);

        if self.opts.low_information && is_low_information(&corrected) {
            return Decision::Skipped(SkipReason::LowInformation);
        }

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
                if seg.source != Source::Them {
                    return false;
                }
                let overlaps = if self.opts.echo_tolerance {
                    overlaps_within(start, end, seg.start, seg.end, ECHO_TIME_TOLERANCE_SECS)
                } else {
                    time_overlaps(start, end, seg.start, seg.end)
                };
                overlaps && is_echo_with(&corrected, &seg.text, DEFAULT_ECHO_THRESHOLD, self.opts.echo_containment, self.opts.echo_fuzzy)
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

/// One line held by a [`LineBuffer`], awaiting release.
struct HeldLine {
    start: f32,
    /// Best-effort end of the held line's span — the meeting offset at which
    /// it was pushed, since `Decision` doesn't carry a span end.
    end: f32,
    text: String,
}

/// Holds emitted `Me` lines briefly, releasing them in `start_offset` order
/// once they are older than [`LINE_HOLD_SECS`] of meeting time, and
/// re-running the echo veto against `Them` lines that arrive late. `Them`
/// lines are never held: they pass straight through immediately, since they
/// are never vetoed themselves and holding them would delay the interview
/// coach and the live notepad for no benefit. Exception: with seam repair
/// enabled, a hard-cut Them tail waits for its successor (or drain_all), so
/// uncertain final words can be repaired before reaching the transcript.
pub struct LineBuffer {
    held: Vec<HeldLine>,
    pending_them: Option<HeldLine>,
    late_echo_dropped: usize,
    opts: PipelineOptions,
}

impl LineBuffer {
    pub fn new(opts: PipelineOptions) -> Self {
        Self { held: Vec::new(), pending_them: None, late_echo_dropped: 0, opts }
    }

    /// Feed a decision at meeting-time `now_offset` (the offset at which the
    /// chunk producing `d` finished processing). Returns any lines now due
    /// for writing, in `start_offset` order — including, immediately, the
    /// `Them` line carried by `d` itself if any.
    pub fn push(&mut self, d: Decision, now_offset: f32) -> Vec<(f32, Source, String)> {
        self.push_with_seam(d, now_offset, false, false, false)
    }

    /// Preserve chunk boundary metadata for two-sided Them seam repair.
    pub fn push_chunk(&mut self, d: Decision, chunk: &Chunk) -> Vec<(f32, Source, String)> {
        self.push_with_seam(d, chunk.end_offset(), chunk.hard_cut, chunk.seam_dedup,
            chunk.source == Source::Them)
    }

    /// Echo vetoes only, excluding text removed by seam repair.
    pub fn dropped_echo_count(&self) -> usize {
        self.late_echo_dropped
    }

    fn push_with_seam(&mut self, d: Decision, now_offset: f32, hard_cut: bool,
        seam_dedup: bool, is_them: bool) -> Vec<(f32, Source, String)> {
        let mut out = Vec::new();
        match d {
            Decision::Emit { start, source: Source::Me, text } => {
                self.held.push(HeldLine { start, end: now_offset, text });
            }
            Decision::Emit { start, source: Source::Them, mut text } => {
                let them_end = now_offset;
                // A Them line that overlaps and textually echoes a held Me
                // line vetoes it, even though the Me line was emitted first —
                // the two sources chunk (and finish transcribing)
                // independently, so completion order is not span order.
                let held_before = self.held.len();
                self.held.retain(|held| {
                    let overlaps = if self.opts.echo_tolerance {
                        overlaps_within(held.start, held.end, start, them_end, ECHO_TIME_TOLERANCE_SECS)
                    } else {
                        time_overlaps(held.start, held.end, start, them_end)
                    };
                    !(overlaps && is_echo_with(&held.text, &text, DEFAULT_ECHO_THRESHOLD, self.opts.echo_containment, self.opts.echo_fuzzy))
                });
                self.late_echo_dropped += held_before - self.held.len();
                if let Some(mut previous) = self.pending_them.take() {
                    if self.opts.seam_repair && seam_dedup {
                        let (repaired, trimmed) = seam_repair(&previous.text, &text);
                        if let Some(repaired) = repaired { previous.text = repaired; }
                        text = trimmed;
                    }
                    out.push((previous.start, Source::Them, previous.text));
                }
                if self.opts.seam_repair && hard_cut && !text.is_empty() {
                    self.pending_them = Some(HeldLine { start, end: them_end, text });
                } else if !text.is_empty() {
                    out.push((start, Source::Them, text));
                }
            }
            // Skipped/Failed/DroppedEcho: nothing new to hold or release
            // directly, but time has still advanced — fall through to the
            // due-line check below.
            _ => {
                // A failed/skipped successor cannot repair the held tail.
                if is_them {
                    if let Some(previous) = self.pending_them.take() {
                        out.push((previous.start, Source::Them, previous.text));
                    }
                }
            }
        }
        out.extend(self.release_due(now_offset));
        out.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        out
    }

    /// Releases held lines older than `LINE_HOLD_SECS`, oldest first.
    fn release_due(&mut self, now_offset: f32) -> Vec<(f32, Source, String)> {
        let (due, remaining): (Vec<_>, Vec<_>) =
            self.held.drain(..).partition(|h| now_offset - h.start >= LINE_HOLD_SECS);
        self.held = remaining;
        let mut due: Vec<(f32, Source, String)> =
            due.into_iter().map(|h| (h.start, Source::Me, h.text)).collect();
        due.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        due
    }

    /// Meeting over: release everything, in `start_offset` order.
    pub fn drain_all(&mut self) -> Vec<(f32, Source, String)> {
        let mut all: Vec<(f32, Source, String)> =
            self.held.drain(..).map(|h| (h.start, Source::Me, h.text)).collect();
        if let Some(previous) = self.pending_them.take() {
            if !previous.text.is_empty() {
                all.push((previous.start, Source::Them, previous.text));
            }
        }
        all.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        all
    }
}

impl Default for LineBuffer {
    fn default() -> Self {
        Self::new(PipelineOptions::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// A chunk of `len_secs` of (unused) 16 kHz audio starting at `start`,
    /// with `speech_secs` set to the full chunk length (tests that care about
    /// the min-speech gate override it explicitly).
    fn chunk_at(source: Source, start: f32, len_secs: f32) -> Chunk {
        let sample_rate = 16_000u32;
        Chunk {
            source,
            samples: vec![0.0; (len_secs * sample_rate as f32) as usize],
            sample_rate,
            start_offset: start,
            has_speech: true,
            speech_secs: len_secs,
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

    // ---- B6: normalize_for_asr ----

    #[test]
    fn normalize_scales_a_quiet_chunk_to_the_target_peak() {
        let mut samples = vec![0.0, 0.05, -0.03, 0.02];
        let gain = normalize_for_asr(&mut samples);
        assert!((gain - (NORMALIZE_TARGET_PEAK / 0.05)).abs() < 1e-4, "gain {gain}");
        let peak = samples.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
        assert!((peak - NORMALIZE_TARGET_PEAK).abs() < 1e-4, "peak {peak}");
    }

    #[test]
    fn normalize_leaves_a_loud_chunk_untouched() {
        let mut samples = vec![0.0, 0.3, -0.5, 0.2];
        let original = samples.clone();
        let gain = normalize_for_asr(&mut samples);
        assert_eq!(gain, 1.0);
        assert_eq!(samples, original);
    }

    #[test]
    fn normalize_respects_the_gain_ceiling() {
        let mut samples = vec![1e-5, -1e-5];
        let gain = normalize_for_asr(&mut samples);
        assert_eq!(gain, NORMALIZE_MAX_GAIN);
    }

    #[test]
    fn normalize_is_a_noop_on_digital_silence() {
        let mut samples = vec![0.0; 100];
        let gain = normalize_for_asr(&mut samples);
        assert_eq!(gain, 1.0);
        assert!(samples.iter().all(|s| *s == 0.0 && !s.is_nan()));
    }

    // ---- B6: is_low_information ----

    #[test]
    fn low_information_drops_punctuation_only() {
        assert!(is_low_information("...  --  ,,,"));
    }

    #[test]
    fn low_information_drops_a_repeated_token_loop() {
        assert!(is_low_information("the the the the the the"));
    }

    #[test]
    fn low_information_drops_a_bare_vocalization() {
        assert!(is_low_information("um"));
        assert!(is_low_information("Uh."));
        assert!(is_low_information("Hmm"));
    }

    #[test]
    fn low_information_keeps_a_real_backchannel() {
        for phrase in ["yeah", "right", "for sure", "makes sense"] {
            assert!(!is_low_information(phrase), "{phrase} should survive");
        }
    }

    #[test]
    fn low_information_keeps_a_short_real_sentence() {
        assert!(!is_low_information("ship it"));
    }

    // ---- B6: min-speech gate ----

    #[test]
    fn too_short_chunks_are_skipped_before_the_engine_runs() {
        let mut pipeline = ChunkPipeline::new(Arc::new(Vec::new()));
        let mut chunk = chunk_at(Source::Them, 0.0, 3.0);
        chunk.speech_secs = 0.1; // under MIN_SPEECH_SECS

        let called = Cell::new(false);
        let decision = pipeline.process(&chunk, |_| {
            called.set(true);
            Ok("should never run".to_string())
        });

        assert_eq!(decision, Decision::Skipped(SkipReason::TooShort));
        assert!(!called.get(), "a too-short chunk must not reach the engine");
    }

    #[test]
    fn a_half_second_utterance_is_still_transcribed() {
        let mut pipeline = ChunkPipeline::new(Arc::new(Vec::new()));
        let mut chunk = chunk_at(Source::Them, 0.0, 0.5);
        chunk.speech_secs = 0.5; // > MIN_SPEECH_SECS (0.3)

        let decision = pipeline.process(&chunk, says("ship it"));

        assert_eq!(
            decision,
            Decision::Emit { start: 0.0, source: Source::Them, text: "ship it".to_string() }
        );
    }

    // ---- B6: seam dedup ----

    #[test]
    fn seam_dedup_removes_the_overlapped_words_at_a_hard_cap_cut() {
        let mut pipeline = ChunkPipeline::with_options(Arc::new(Vec::new()), PipelineOptions {
            seam_repair: false, ..PipelineOptions::default()
        });

        let first = pipeline.process(&chunk_at(Source::Them, 0.0, 5.0), says("meet me at the cafe"));
        assert!(matches!(first, Decision::Emit { .. }));

        let mut second = chunk_at(Source::Them, 5.0, 5.0);
        second.seam_dedup = true;
        let decision = pipeline.process(&second, says("at the cafe tomorrow"));

        assert_eq!(
            decision,
            Decision::Emit { start: 5.0, source: Source::Them, text: "tomorrow".to_string() }
        );
    }

    #[test]
    fn seam_dedup_is_per_source() {
        let mut pipeline = ChunkPipeline::new(Arc::new(Vec::new()));

        pipeline.process(&chunk_at(Source::Me, 0.0, 5.0), says("meet me at the cafe"));

        let mut them = chunk_at(Source::Them, 5.0, 5.0);
        them.seam_dedup = true;
        // A Them seam is compared only against previous Them text, which is
        // empty here, so nothing from the Me line is deduped away.
        let decision = pipeline.process(&them, says("at the cafe tomorrow"));

        assert_eq!(
            decision,
            Decision::Emit {
                start: 5.0,
                source: Source::Them,
                text: "at the cafe tomorrow".to_string(),
            }
        );
    }

    #[test]
    fn legacy_options_reproduce_the_b0_behaviour() {
        // Under legacy() all four new gates are off, so a chunk the new gates
        // would skip (too short *and* a low-information repeated-token loop)
        // is still transcribed exactly like the pre-B6 pipeline.
        let mut pipeline = ChunkPipeline::with_options(Arc::new(Vec::new()), PipelineOptions::legacy());
        let mut chunk = chunk_at(Source::Them, 0.0, 0.2);
        chunk.speech_secs = 0.1; // well under MIN_SPEECH_SECS

        let decision = pipeline.process(&chunk, says("uh uh uh uh uh uh"));

        assert_eq!(
            decision,
            Decision::Emit {
                start: 0.0,
                source: Source::Them,
                text: "uh uh uh uh uh uh".to_string(),
            }
        );
    }

    // ---- B6: LineBuffer ----

    #[test]
    fn me_lines_are_released_in_order_after_the_hold_and_them_lines_pass_straight_through() {
        let mut buf = LineBuffer::new(PipelineOptions::default());

        let r1 = buf.push(Decision::Emit { start: 3.0, source: Source::Me, text: "third".into() }, 3.0);
        assert!(r1.is_empty(), "a Me line is held, not released immediately");
        let r2 = buf.push(Decision::Emit { start: 1.0, source: Source::Me, text: "first".into() }, 1.0);
        assert!(r2.is_empty());

        // A Them line passes straight through immediately, never held.
        let r3 = buf.push(Decision::Emit { start: 2.0, source: Source::Them, text: "them line".into() }, 2.0);
        assert_eq!(r3, vec![(2.0, Source::Them, "them line".to_string())]);

        // Advance past the hold window: both Me lines are now due, oldest
        // start_offset first.
        let r4 = buf.push(Decision::Skipped(SkipReason::NoSpeech), 8.1);
        assert_eq!(
            r4,
            vec![
                (1.0, Source::Me, "first".to_string()),
                (3.0, Source::Me, "third".to_string()),
            ]
        );
    }

    #[test]
    fn line_buffer_releases_in_start_offset_order() {
        let mut buf = LineBuffer::new(PipelineOptions::default());
        // Pushed out of start_offset order (and each push's own now_offset is
        // still under its hold deadline, so nothing releases early).
        buf.push(Decision::Emit { start: 8.0, source: Source::Me, text: "c".into() }, 8.0);
        buf.push(Decision::Emit { start: 2.0, source: Source::Me, text: "a".into() }, 2.0);
        buf.push(Decision::Emit { start: 5.0, source: Source::Me, text: "b".into() }, 5.0);

        let released = buf.push(Decision::Skipped(SkipReason::NoSpeech), 13.0);
        assert_eq!(
            released,
            vec![
                (2.0, Source::Me, "a".to_string()),
                (5.0, Source::Me, "b".to_string()),
                (8.0, Source::Me, "c".to_string()),
            ]
        );
    }

    #[test]
    fn a_late_them_line_still_vetoes_an_earlier_me_echo() {
        let mut buf = LineBuffer::new(PipelineOptions::default());
        let line = "so the migration lands on thursday";

        // Me line finishes transcribing first (its Them counterpart is slower).
        let r1 = buf.push(Decision::Emit { start: 1.0, source: Source::Me, text: line.to_string() }, 1.5);
        assert!(r1.is_empty());

        // The overlapping Them line arrives later and vetoes the held Me line.
        let r2 = buf.push(Decision::Emit { start: 0.0, source: Source::Them, text: line.to_string() }, 5.0);
        assert_eq!(
            r2,
            vec![(0.0, Source::Them, line.to_string())],
            "the them line still passes through immediately"
        );

        // Even after the hold window elapses, the vetoed Me line is gone.
        let r3 = buf.push(Decision::Skipped(SkipReason::NoSpeech), 10.0);
        assert!(r3.is_empty(), "the echoed Me line must never be released");
    }

    #[test]
    fn line_buffer_drain_all_releases_everything_in_order() {
        let mut buf = LineBuffer::new(PipelineOptions::default());
        buf.push(Decision::Emit { start: 5.0, source: Source::Me, text: "b".into() }, 5.0);
        buf.push(Decision::Emit { start: 2.0, source: Source::Me, text: "a".into() }, 2.0);

        let drained = buf.drain_all();
        assert_eq!(
            drained,
            vec![
                (2.0, Source::Me, "a".to_string()),
                (5.0, Source::Me, "b".to_string()),
            ]
        );
        assert!(buf.drain_all().is_empty());
    }

    #[test]
    fn echo_options_control_both_process_and_late_them_veto() {
        let them = "Von Zel Brown met Ifa O Sullivan and Rajesh Krishna Murti on Tuesday.";
        let me = "Bonzell Brown met Ifa Osalivan and Rajesh Krishnamurathi on Tuesday.";
        // Overlapping spans isolate the fuzzy arm; abutting spans also require
        // tolerance. Containment alone cannot match this spelling difference.
        for gap in [false, true] {
            for fuzzy in [false, true] {
                for tolerance in [false, true] {
                    let opts = PipelineOptions {
                        echo_fuzzy: fuzzy, echo_tolerance: tolerance,
                        echo_containment: false, ..PipelineOptions::default()
                    };
                    let me_start = if gap { 1.3 } else { 0.5 };
                    let should_drop = fuzzy && (!gap || tolerance);
                    let mut pipeline = ChunkPipeline::with_options(Arc::new(Vec::new()), opts);
                    pipeline.process(&chunk_at(Source::Them, 0.0, 1.0), says(them));
                    let result = pipeline.process(&chunk_at(Source::Me, me_start, 1.0), says(me));
                    assert_eq!(matches!(result, Decision::DroppedEcho(_)), should_drop);

                    let mut buf = LineBuffer::new(opts);
                    buf.push(Decision::Emit { start: 0.0, source: Source::Me, text: me.into() }, 1.0);
                    buf.push(Decision::Emit { start: me_start, source: Source::Them, text: them.into() }, 2.5);
                    assert_eq!(buf.drain_all().is_empty(), should_drop);
                }
            }
        }
        for (opts, dropped) in [(PipelineOptions::legacy(), false), (PipelineOptions::default(), true)] {
            let mut buf = LineBuffer::new(opts);
            buf.push(Decision::Emit { start: 0.0, source: Source::Me, text: me.into() }, 1.0);
            buf.push(Decision::Emit { start: 0.5, source: Source::Them, text: them.into() }, 2.5);
            assert_eq!(buf.drain_all().is_empty(), dropped);
        }
    }

    #[test]
    fn seam_repair_fixes_the_corpus_boundary_and_partial_words() {
        assert_eq!(seam_repair("the summary and confirm the", "Summary and confirm that early decisions remain"),
            (Some("the summary and confirm".into()), "that early decisions remain".into()));
        assert_eq!(seam_repair("please review the docu", "review the document tomorrow"),
            (Some("please review the".into()), "document tomorrow".into()));
    }

    #[test]
    fn seam_repair_preserves_exact_overlap_and_legitimate_repetition() {
        assert_eq!(seam_repair("meet me at the CAFE,", "At the cafe tomorrow"),
            (None, "tomorrow".into()));
        assert_eq!(seam_repair("we will win", "win win win"), (None, "win win".into()));
        assert_eq!(seam_repair("check this now", "check this now"), (None, "".into()));
    }

    #[test]
    fn seam_repair_needs_two_anchors_to_change_the_previous_tail() {
        for (prev, next) in [("", "hello world"), ("review this tomorrow", "unrelated words here"),
            ("review draft", "review tomorrow"), ("... !!!", "!!! ...")]
        {
            assert_eq!(seam_repair(prev, next), (None, next.to_string()));
        }
        assert_eq!(seam_repair("review the document", ""), (None, String::new()));
    }

    #[test]
    fn hard_them_tail_waits_for_successor_and_repairs_both_lines() {
        let opts = PipelineOptions::default();
        let mut pipeline = ChunkPipeline::with_options(Arc::new(Vec::new()), opts);
        let mut buf = LineBuffer::new(opts);
        let mut first = chunk_at(Source::Them, 0.0, 30.0);
        first.hard_cut = true;
        let d = pipeline.process(&first, says("the summary and confirm the"));
        assert!(buf.push_chunk(d, &first).is_empty());
        // A Me interjection neither replaces nor repairs the held Them tail.
        let me = chunk_at(Source::Me, 28.0, 0.5);
        assert!(buf.push_chunk(Decision::Emit { start: 28.0, source: Source::Me, text: "yes".into() }, &me).is_empty());
        let mut next = chunk_at(Source::Them, 29.0, 15.0);
        next.seam_dedup = true;
        let d = pipeline.process(&next, says("Summary and confirm that early decisions remain"));
        let lines = buf.push_chunk(d, &next);
        assert_eq!(lines, vec![
            (0.0, Source::Them, "the summary and confirm".into()),
            (28.0, Source::Me, "yes".into()),
            (29.0, Source::Them, "that early decisions remain".into()),
        ]);
        assert_eq!(buf.dropped_echo_count(), 0);
        assert!(buf.drain_all().is_empty());
    }

    #[test]
    fn ordinary_them_and_disabled_repair_pass_through() {
        for (opts, hard_cut) in [(PipelineOptions::default(), false), (PipelineOptions::legacy(), true)] {
            let mut buf = LineBuffer::new(opts);
            let mut chunk = chunk_at(Source::Them, 0.0, 30.0);
            chunk.hard_cut = hard_cut;
            assert_eq!(buf.push_chunk(Decision::Emit { start: 0.0, source: Source::Them, text: "original words".into() }, &chunk),
                vec![(0.0, Source::Them, "original words".into())]);
        }
    }

    #[test]
    fn a_chain_of_hard_tails_repairs_each_successor_and_drains_last() {
        let mut buf = LineBuffer::default();
        let mut first = chunk_at(Source::Them, 0.0, 30.0);
        first.hard_cut = true;
        buf.push_chunk(Decision::Emit { start: 0.0, source: Source::Them, text: "we review the docu".into() }, &first);
        let mut next = chunk_at(Source::Them, 29.0, 30.0);
        next.hard_cut = true;
        next.seam_dedup = true;
        assert_eq!(buf.push_chunk(Decision::Emit { start: 29.0, source: Source::Them, text: "review the document and confirm the".into() }, &next),
            vec![(0.0, Source::Them, "we review the".into())]);
        assert_eq!(buf.drain_all(), vec![(29.0, Source::Them, "document and confirm the".into())]);
        assert!(buf.drain_all().is_empty());
    }

    #[test]
    fn skipped_successor_flushes_uncertain_tail_without_losing_it() {
        let mut buf = LineBuffer::default();
        let mut chunk = chunk_at(Source::Them, 0.0, 30.0);
        chunk.hard_cut = true;
        buf.push_chunk(Decision::Emit { start: 0.0, source: Source::Them, text: "unrepaired tail".into() }, &chunk);
        let mut next = chunk_at(Source::Them, 29.0, 1.0);
        next.seam_dedup = true;
        assert_eq!(buf.push_chunk(Decision::Skipped(SkipReason::EmptyText), &next),
            vec![(0.0, Source::Them, "unrepaired tail".into())]);
    }

    #[test]
    fn late_echo_count_excludes_completely_duplicated_seam_lines() {
        let mut buf = LineBuffer::default();
        let mut first = chunk_at(Source::Them, 0.0, 30.0);
        first.hard_cut = true;
        buf.push_chunk(Decision::Emit { start: 0.0, source: Source::Them, text: "check this now".into() }, &first);
        let mut next = chunk_at(Source::Them, 29.0, 1.0);
        next.seam_dedup = true;
        assert_eq!(buf.push_chunk(Decision::Emit { start: 29.0, source: Source::Them, text: "check this now".into() }, &next),
            vec![(0.0, Source::Them, "check this now".into())]);
        assert_eq!(buf.dropped_echo_count(), 0);
        buf.push(Decision::Emit { start: 40.0, source: Source::Me, text: "please check this now".into() }, 41.0);
        buf.push(Decision::Emit { start: 40.0, source: Source::Them, text: "please check this now".into() }, 42.0);
        assert_eq!(buf.dropped_echo_count(), 1);
    }
}
