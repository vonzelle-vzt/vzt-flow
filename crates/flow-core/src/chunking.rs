//! Silence-aware chunked transcription for long dictations.
//!
//! The bundled Parakeet TDT ONNX engine (`crate::engine`) has no internal
//! streaming and shows faster-than-linear (≈quadratic) memory growth in the
//! length of a single `transcribe` call — measured on an M5 Mac: ~15.6GB peak
//! for 49s of audio, ~37GB for 93s, and an out-of-memory kill for a ~146s
//! clip. With the hold-to-talk cap raised to 600s a real multi-minute
//! dictation would be OOM-killed and the user's words lost.
//!
//! [`transcribe_long`] fixes that by splitting long audio into ~30s chunks and
//! transcribing them **sequentially on the one engine**, so peak memory stays
//! bounded to a single chunk's footprint (~single-chunk peak, comfortably
//! under 16GB) instead of scaling with total length.
//!
//! Where possible a chunk boundary is placed in the **quietest** region of a
//! 25–35s window from the chunk start: cutting inside a silence means the two
//! chunks can simply be concatenated with no risk of a word being split or
//! duplicated across the seam. Only when a window contains no quiet frame
//! (continuous speech with no pause) do we hard-cut at 35s with a 1.5s overlap
//! and de-duplicate the repeated words at the seam ([`dedup_seam`]).
//!
//! The RMS-frame helpers and thresholds are borrowed from
//! [`crate::meeting::transcriber`] rather than re-implemented — the notion of
//! "a ~100ms frame whose energy is below the silence bar" is identical here.

use anyhow::{Context, Result};

use crate::engine::{Transcriber, Transcript, TranscriptSegment};
use crate::meeting::transcriber::{rms, FRAME_SECS, SILENCE_RMS_THRESHOLD};

/// Sample rate assumed for the `samples_16k_mono` input. The dictation and
/// standalone capture paths all resample to 16 kHz mono before transcription.
pub(crate) const SAMPLE_RATE: u32 = 16_000;

/// Audio at or below this length is transcribed in a single pass (unchanged
/// behavior). 35s sits safely under the ~16GB peak the engine reaches around
/// 49s, so short and medium dictations pay no chunking overhead. Also the
/// threshold the rolling path (`crate::rolling`) uses to decide when enough
/// settled audio has accumulated to cut a chunk mid-recording.
pub(crate) const SINGLE_PASS_MAX_SECS: f32 = 35.0;

/// Earliest offset (from a chunk's start) at which a silence cut is allowed —
/// the low edge of the search window. Keeps chunks from being pointlessly
/// short when an early pause exists.
const CUT_WINDOW_MIN_SECS: f32 = 25.0;

/// Latest offset at which a chunk may end. Also the hard-cut point when no
/// quiet frame is found in the window. Chunks are therefore always ≤35s, which
/// bounds the per-chunk memory peak.
const CUT_WINDOW_MAX_SECS: f32 = 35.0;

/// Overlap re-fed into the next chunk after a hard (mid-speech) cut, so a word
/// straddling the boundary is captured whole by at least one chunk. The
/// repeated words are removed by [`dedup_seam`].
pub(crate) const OVERLAP_SECS: f32 = 1.5;

/// Upper bound on how many leading words of a post-hard-cut chunk are examined
/// for a seam overlap. 1.5s of speech is only a handful of words; capping the
/// search keeps a legitimately repeated phrase deep in the text from being
/// mistaken for an overlap echo.
const MAX_SEAM_OVERLAP_WORDS: usize = 20;

/// Where a chunk should end and whether the cut lands in silence (clean
/// concatenation) or mid-speech (needs an overlap + seam dedup).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CutKind {
    /// Cut inside a quiet frame — the chunks concatenate cleanly.
    Silence,
    /// No quiet frame in the window — hard cut at 35s; the following chunk
    /// overlaps by [`OVERLAP_SECS`] and must be seam-deduped.
    Hard,
}

/// One planned chunk: a sample span of the original audio plus whether it
/// begins inside the previous chunk (a hard-cut overlap) and therefore needs
/// its leading duplicated words removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkPlan {
    pub start: usize,
    pub len: usize,
    pub needs_dedup: bool,
}

/// Transcribe arbitrarily long 16 kHz mono audio without the engine's
/// quadratic-memory blowup.
///
/// Audio ≤35s takes the single-pass path unchanged. Longer audio is split by
/// [`plan_chunks`] and transcribed sequentially on `transcriber`; texts are
/// joined with a space (seam duplicates removed after hard cuts) and, when the
/// engine returns segments, each chunk's segment timestamps are offset by the
/// chunk's start so the merged segment list stays monotonic and coherent.
pub fn transcribe_long(
    samples_16k_mono: &[f32],
    transcriber: &mut dyn Transcriber,
) -> Result<Transcript> {
    let single_pass_max = (SINGLE_PASS_MAX_SECS * SAMPLE_RATE as f32) as usize;
    if samples_16k_mono.len() <= single_pass_max {
        // Short/medium dictation: unchanged single pass, no progress log.
        return transcriber.transcribe(samples_16k_mono);
    }

    let plans = plan_chunks(samples_16k_mono, SAMPLE_RATE);
    let total = plans.len();

    // Transcribe each chunk sequentially on the one engine (so peak memory
    // stays bounded to a single chunk), collecting per-chunk transcripts to
    // stitch afterwards. Only one chunk's slice is touched at a time.
    let mut transcripts: Vec<Transcript> = Vec::with_capacity(total);
    for (i, plan) in plans.iter().enumerate() {
        let dur_secs = plan.len as f32 / SAMPLE_RATE as f32;
        eprintln!(
            "[vzt-flow] transcribing chunk {}/{} ({:.0}s)...",
            i + 1,
            total,
            dur_secs
        );

        let chunk = &samples_16k_mono[plan.start..plan.start + plan.len];
        let t = transcriber
            .transcribe(chunk)
            .with_context(|| format!("transcribing chunk {}/{}", i + 1, total))?;
        transcripts.push(t);
    }

    Ok(assemble(&plans, &transcripts, SAMPLE_RATE))
}

/// Stitches per-chunk transcripts back into one, applying the seam de-dup at
/// hard-cut boundaries and offsetting each chunk's segment timestamps by the
/// chunk's start. Factored out of [`transcribe_long`] so the rolling path
/// (`crate::rolling`), which transcribes chunks *during* recording and only
/// has them all in hand at release, reuses the identical seam/segment logic
/// instead of re-deriving it. `plans` and `transcripts` must be the same
/// length and in the same chunk order.
pub fn assemble(plans: &[ChunkPlan], transcripts: &[Transcript], sample_rate: u32) -> Transcript {
    let mut out_text = String::new();
    let mut out_segments: Vec<TranscriptSegment> = Vec::new();
    let mut any_segments = false;
    // Previous chunk's emitted (post-dedup) text, for seam de-duplication.
    let mut prev_text = String::new();

    for (plan, t) in plans.iter().zip(transcripts.iter()) {
        let text = if plan.needs_dedup {
            dedup_seam(&prev_text, &t.text)
        } else {
            t.text.trim().to_string()
        };

        if !text.is_empty() {
            if !out_text.is_empty() {
                out_text.push(' ');
            }
            out_text.push_str(&text);
        }

        if let Some(segments) = &t.segments {
            any_segments = true;
            let offset = plan.start as f32 / sample_rate as f32;
            for s in segments {
                out_segments.push(TranscriptSegment {
                    start: s.start + offset,
                    end: s.end + offset,
                    text: s.text.clone(),
                });
            }
        }

        prev_text = text;
    }

    Transcript {
        text: out_text,
        segments: if any_segments { Some(out_segments) } else { None },
    }
}

/// Splits `samples` into chunk spans, choosing each boundary via
/// [`plan_cut`]. Pure and audio-only (no engine) so the cut policy is unit
/// testable with synthetic RMS profiles.
///
/// Every chunk is ≤35s. After a silence cut the next chunk starts exactly at
/// the boundary; after a hard cut it starts [`OVERLAP_SECS`] earlier and is
/// flagged `needs_dedup`. The final chunk absorbs the remaining ≤35s tail.
pub fn plan_chunks(samples: &[f32], sample_rate: u32) -> Vec<ChunkPlan> {
    let single_pass_max = (SINGLE_PASS_MAX_SECS * sample_rate as f32) as usize;
    let overlap = (OVERLAP_SECS * sample_rate as f32) as usize;

    let mut plans = Vec::new();
    let mut pos = 0usize;
    let mut needs_dedup = false;

    while pos < samples.len() {
        let remaining = samples.len() - pos;
        if remaining <= single_pass_max {
            plans.push(ChunkPlan { start: pos, len: remaining, needs_dedup });
            break;
        }

        let (cut, kind) = plan_cut(&samples[pos..], sample_rate);
        plans.push(ChunkPlan { start: pos, len: cut, needs_dedup });

        match kind {
            CutKind::Silence => {
                pos += cut;
                needs_dedup = false;
            }
            CutKind::Hard => {
                // Rewind into the just-emitted chunk by the overlap so the
                // next chunk re-hears the last 1.5s (a word split by the cut
                // is captured whole by one side). cut ≥ 25s ≫ overlap, so pos
                // always advances — no risk of a stall.
                pos = (pos + cut).saturating_sub(overlap);
                needs_dedup = true;
            }
        }
    }

    plans
}

/// Decides where to end a chunk that starts at `remaining[0]`, given
/// `remaining.len()` > 35s. Scans the 25–35s window in ~100ms frames for the
/// quietest one; if that frame is below the silence bar it's a clean
/// [`CutKind::Silence`] cut (returned at the frame's midpoint), otherwise the
/// window is continuous speech and we [`CutKind::Hard`]-cut at 35s.
pub(crate) fn plan_cut(remaining: &[f32], sample_rate: u32) -> (usize, CutKind) {
    let frame = ((FRAME_SECS * sample_rate as f32) as usize).max(1);
    let win_start = (CUT_WINDOW_MIN_SECS * sample_rate as f32) as usize;
    let win_end = ((CUT_WINDOW_MAX_SECS * sample_rate as f32) as usize).min(remaining.len());
    let hard_cut = (CUT_WINDOW_MAX_SECS * sample_rate as f32) as usize;

    let mut best_rms = f32::INFINITY;
    let mut best_at = hard_cut;

    let mut pos = win_start;
    while pos + frame <= win_end {
        let energy = rms(&remaining[pos..pos + frame]);
        if energy < best_rms {
            best_rms = energy;
            best_at = pos + frame / 2; // cut in the middle of the quiet frame
        }
        pos += frame;
    }

    if best_rms < SILENCE_RMS_THRESHOLD {
        (best_at, CutKind::Silence)
    } else {
        (hard_cut, CutKind::Hard)
    }
}

/// Normalizes a single word for seam comparison: lowercased, non-alphanumeric
/// stripped. Casing and punctuation differ freely between the two ASR passes
/// over the overlapped audio, so both are normalized away.
fn normalize_word(word: &str) -> String {
    word.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// Removes the leading words of `next_text` that duplicate the trailing words
/// of `prev_text` at a hard-cut seam. Finds the **longest** run (up to
/// [`MAX_SEAM_OVERLAP_WORDS`]) where a suffix of `prev` equals a prefix of
/// `next` under [`normalize_word`], and drops that prefix from `next`.
///
/// Because only a hard cut carries a real 1.5s overlap, this is only invoked
/// there; a word legitimately repeated elsewhere in speech is preserved,
/// since only the *matched leading run* is dropped (a longer genuine tail like
/// "win win win" keeps every "win" beyond the single overlapped one).
pub fn dedup_seam(prev_text: &str, next_text: &str) -> String {
    let next_words: Vec<&str> = next_text.split_whitespace().collect();
    if next_words.is_empty() {
        return String::new();
    }
    let prev_words: Vec<&str> = prev_text.split_whitespace().collect();
    if prev_words.is_empty() {
        return next_words.join(" ");
    }

    let cap = MAX_SEAM_OVERLAP_WORDS.min(prev_words.len()).min(next_words.len());
    let prev_norm: Vec<String> = prev_words.iter().map(|w| normalize_word(w)).collect();
    let next_norm: Vec<String> = next_words.iter().map(|w| normalize_word(w)).collect();

    let mut best = 0;
    for l in 1..=cap {
        let prev_suffix = &prev_norm[prev_norm.len() - l..];
        let next_prefix = &next_norm[..l];
        // An all-punctuation token normalizes to "" — never let two empty
        // tokens count as a match (they'd drop real words on a stray comma).
        let matches = prev_suffix
            .iter()
            .zip(next_prefix)
            .all(|(a, b)| !a.is_empty() && a == b);
        if matches {
            best = l;
        }
    }

    next_words[best..].join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `secs` seconds of constant-amplitude samples at 16 kHz. Constant DC
    /// means RMS == |amp|, so amplitude directly controls the frame energy.
    fn block(secs: f32, amp: f32) -> Vec<f32> {
        vec![amp; (secs * SAMPLE_RATE as f32) as usize]
    }

    // ---- cut-point selection (synthetic RMS profiles) ----

    #[test]
    fn plan_cut_picks_the_quiet_region_in_the_window() {
        // 40s of speech with a 200ms silence gap centered at 30s.
        let mut samples = block(40.0, 0.3);
        let gap_start = (29.9 * SAMPLE_RATE as f32) as usize;
        let gap_end = (30.1 * SAMPLE_RATE as f32) as usize;
        for s in &mut samples[gap_start..gap_end] {
            *s = 0.0;
        }
        let (cut, kind) = plan_cut(&samples, SAMPLE_RATE);
        assert_eq!(kind, CutKind::Silence);
        let cut_secs = cut as f32 / SAMPLE_RATE as f32;
        assert!((cut_secs - 30.0).abs() < 0.3, "cut at {cut_secs}s, expected ~30s");
    }

    #[test]
    fn plan_cut_ignores_silence_outside_the_window() {
        // Silence at 10s (before the 25s window) — must NOT be chosen; the
        // rest is continuous speech, so it hard-cuts at 35s.
        let mut samples = block(40.0, 0.3);
        for s in &mut samples[(9.9 * SAMPLE_RATE as f32) as usize..(10.1 * SAMPLE_RATE as f32) as usize] {
            *s = 0.0;
        }
        let (cut, kind) = plan_cut(&samples, SAMPLE_RATE);
        assert_eq!(kind, CutKind::Hard);
        assert_eq!(cut, (CUT_WINDOW_MAX_SECS * SAMPLE_RATE as f32) as usize);
    }

    #[test]
    fn plan_cut_hard_cuts_continuous_speech_at_35s() {
        let samples = block(40.0, 0.3); // loud throughout, no pause
        let (cut, kind) = plan_cut(&samples, SAMPLE_RATE);
        assert_eq!(kind, CutKind::Hard);
        assert_eq!(cut, (CUT_WINDOW_MAX_SECS * SAMPLE_RATE as f32) as usize);
    }

    #[test]
    fn plan_chunks_silence_cut_yields_clean_non_overlapping_spans() {
        // 40s with a pause at 30s -> cut there, tail 10s. No dedup anywhere.
        let mut samples = block(40.0, 0.3);
        for s in &mut samples[(29.9 * SAMPLE_RATE as f32) as usize..(30.1 * SAMPLE_RATE as f32) as usize] {
            *s = 0.0;
        }
        let plans = plan_chunks(&samples, SAMPLE_RATE);
        assert_eq!(plans.len(), 2);
        assert!(!plans[0].needs_dedup && !plans[1].needs_dedup);
        // Second chunk starts exactly where the first ends (no overlap).
        assert_eq!(plans[1].start, plans[0].start + plans[0].len);
    }

    #[test]
    fn plan_chunks_hard_cut_overlaps_and_flags_dedup() {
        // 75s of continuous speech -> hard cuts, overlapping chunks.
        let samples = block(75.0, 0.3);
        let plans = plan_chunks(&samples, SAMPLE_RATE);
        assert_eq!(plans.len(), 3);
        // First chunk is the leading one (no prior overlap).
        assert!(!plans[0].needs_dedup);
        // Subsequent chunks start inside the previous chunk (overlap) and are
        // flagged for seam dedup.
        assert!(plans[1].needs_dedup && plans[2].needs_dedup);
        let overlap = (OVERLAP_SECS * SAMPLE_RATE as f32) as usize;
        assert_eq!(plans[1].start, plans[0].start + plans[0].len - overlap);
        // Every chunk is ≤35s (bounds the memory peak).
        for p in &plans {
            assert!(p.len <= (CUT_WINDOW_MAX_SECS * SAMPLE_RATE as f32) as usize + 1);
        }
        // Spans cover the whole audio (last chunk reaches the end).
        let last = plans.last().unwrap();
        assert_eq!(last.start + last.len, samples.len());
    }

    // ---- seam dedup ----

    #[test]
    fn dedup_clean_multi_word_overlap() {
        // "at the cafe" is re-transcribed at the start of the next chunk.
        let out = dedup_seam("meet me at the cafe", "at the cafe tomorrow");
        assert_eq!(out, "tomorrow");
    }

    #[test]
    fn dedup_no_overlap_leaves_next_untouched() {
        let out = dedup_seam("hello there", "general kenobi");
        assert_eq!(out, "general kenobi");
    }

    #[test]
    fn dedup_is_case_and_punctuation_insensitive() {
        // "End." vs "end," — same word, different case/punctuation.
        let out = dedup_seam("it is the End.", "end, of story");
        assert_eq!(out, "of story");
    }

    #[test]
    fn dedup_preserves_legitimately_repeated_words() {
        // Speaker really said "win win win"; only the single overlapped "win"
        // is dropped, the genuine repeats survive.
        let out = dedup_seam("we will win win win", "win the game");
        assert_eq!(out, "the game");
    }

    #[test]
    fn dedup_drops_entire_next_when_fully_contained_in_overlap() {
        let out = dedup_seam("see you later", "you later");
        assert_eq!(out, "");
    }

    #[test]
    fn dedup_empty_inputs_are_safe() {
        assert_eq!(dedup_seam("", "hello world"), "hello world");
        assert_eq!(dedup_seam("hello world", ""), "");
    }

    #[test]
    fn dedup_single_word_overlap() {
        let out = dedup_seam("the plan is ready", "ready or not");
        assert_eq!(out, "or not");
    }

    #[test]
    fn dedup_ignores_punctuation_only_tokens() {
        // A lone "-" normalizes to "" — two empty tokens must NOT count as a
        // match (that would drop real words on a stray symbol). Nothing here
        // genuinely overlaps, so the next text is returned verbatim.
        let out = dedup_seam("we are done -", "- so we continue");
        assert_eq!(out, "- so we continue");
    }

    // ---- assemble (shared by transcribe_long and the rolling path) ----

    fn t(text: &str) -> Transcript {
        Transcript { text: text.to_string(), segments: None }
    }

    #[test]
    fn assemble_joins_clean_silence_cut_chunks_with_a_space() {
        // Two silence-cut chunks (no dedup): plain space join, trimmed.
        let plans = vec![
            ChunkPlan { start: 0, len: 100, needs_dedup: false },
            ChunkPlan { start: 100, len: 100, needs_dedup: false },
        ];
        let out = assemble(&plans, &[t("  hello there "), t("general kenobi")], SAMPLE_RATE);
        assert_eq!(out.text, "hello there general kenobi");
        assert!(out.segments.is_none());
    }

    #[test]
    fn assemble_dedups_hard_cut_overlap_like_transcribe_long() {
        // Second chunk is a hard-cut overlap: its leading duplicated words are
        // dropped against the previous chunk's emitted text.
        let plans = vec![
            ChunkPlan { start: 0, len: 100, needs_dedup: false },
            ChunkPlan { start: 90, len: 100, needs_dedup: true },
        ];
        let out = assemble(&plans, &[t("meet me at the cafe"), t("at the cafe tomorrow")], SAMPLE_RATE);
        assert_eq!(out.text, "meet me at the cafe tomorrow");
    }

    #[test]
    fn assemble_offsets_segment_timestamps_by_chunk_start() {
        let plans = vec![
            ChunkPlan { start: 0, len: SAMPLE_RATE as usize, needs_dedup: false },
            ChunkPlan { start: SAMPLE_RATE as usize, len: SAMPLE_RATE as usize, needs_dedup: false },
        ];
        let transcripts = vec![
            Transcript { text: "one".into(), segments: Some(vec![TranscriptSegment { start: 0.0, end: 0.5, text: "one".into() }]) },
            Transcript { text: "two".into(), segments: Some(vec![TranscriptSegment { start: 0.0, end: 0.5, text: "two".into() }]) },
        ];
        let out = assemble(&plans, &transcripts, SAMPLE_RATE);
        let segs = out.segments.expect("segments present");
        assert_eq!(segs.len(), 2);
        // Second chunk starts at 1.0s (SAMPLE_RATE samples), so its segment is
        // shifted by +1.0s.
        assert!((segs[1].start - 1.0).abs() < 1e-4);
        assert!((segs[1].end - 1.5).abs() < 1e-4);
    }
}
