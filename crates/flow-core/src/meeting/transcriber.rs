//! Per-source streaming chunker + the small pure helpers around it
//! (flush-decision, timestamp formatting, summary-input truncation), all
//! factored out so they're unit-testable on every platform without a model,
//! an audio device, or ScreenCaptureKit.
//!
//! Each capture source (system audio, microphone) owns its own
//! [`StreamingChunker`], fed blocks of native-rate mono `f32` as they arrive.
//! The chunker decides when a contiguous span of speech has ended — a
//! trailing run of near-silence, or the hard 30s cap — and emits it as a
//! [`Chunk`] to be transcribed. At the cap the cut lands at the quietest
//! frame of the last few seconds ([`plan_cap_cut`]) rather than exactly on
//! the boundary, and the audio after that cut is *retained* for the next
//! chunk instead of dropped. Working at the source's native sample rate
//! (rather than resampling every incoming block) keeps the audio path free of
//! per-block resampling artifacts; the flushed chunk is resampled to 16 kHz
//! once, by the caller, right before it goes to Parakeet.

/// Hard cap on a single chunk's length. Even mid-sentence, a chunk this long
/// is flushed so a monologue with no pauses still produces timely transcript
/// lines instead of buffering for minutes.
pub const CHUNK_MAX_SECS: f32 = 30.0;

/// Trailing near-silence required (after speech has been heard) to close a
/// chunk at a natural pause. 1.2s per the feature spec — long enough not to
/// cut on the micro-pauses inside a sentence, short enough to keep latency low.
pub const SILENCE_HOLD_SECS: f32 = 1.2;

/// RMS at or above which a frame counts as containing speech. Also the floor
/// that arms a chunk: until one frame clears this bar, the buffer is treated
/// as leading silence and its start offset keeps advancing.
pub const SPEECH_RMS_THRESHOLD: f32 = 0.010;

/// RMS below which a frame counts toward the trailing-silence run that closes
/// a chunk. Kept below [`SPEECH_RMS_THRESHOLD`] (hysteresis) so a frame
/// hovering right at the speech bar doesn't rapidly toggle the silence
/// counter on and off.
pub const SILENCE_RMS_THRESHOLD: f32 = 0.006;

/// Frame granularity for RMS analysis (~100ms), matching the dictation
/// path's VAD frame size in `audio.rs`.
pub const FRAME_SECS: f32 = 0.1;

/// Offset from a chunk's start at which the cap-cut search window opens.
/// Mirrors `chunking::CUT_WINDOW_MIN_SECS`'s role for the batch path: at the
/// 30s cap we would rather cut at the quietest moment of the last ~6s than
/// exactly on the boundary, which lands mid-word.
pub const CHUNK_CUT_WINDOW_MIN_SECS: f32 = 24.0;

/// Overlap re-fed into the next chunk after a hard (no-quiet-frame) cap cut,
/// so a word straddling the boundary is captured whole by one side. The
/// repeated words are removed with `chunking::dedup_seam`.
pub const CHUNK_OVERLAP_SECS: f32 = 1.0;

/// Minimum above-threshold speech in a chunk before it is worth transcribing.
/// A sub-0.3s blip (door, keyboard, mouse click) is not a word, and Parakeet
/// answers noise with hallucinated filler.
pub const MIN_SPEECH_SECS: f32 = 0.3;

/// Which capture source a chunk came from — labels the transcript line and
/// selects the dedup direction (only `Me` chunks are ever dropped as echoes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// System / application audio (the other participants), via
    /// ScreenCaptureKit.
    Them,
    /// The local microphone (the user).
    Me,
}

impl Source {
    /// Speaker label as it appears in the transcript file.
    pub fn label(&self) -> &'static str {
        match self {
            Source::Them => "Them",
            Source::Me => "Me",
        }
    }
}

/// A closed span of buffered audio ready for transcription.
#[derive(Debug, Clone)]
pub struct Chunk {
    pub source: Source,
    /// Native-rate mono samples. The caller resamples to 16 kHz before
    /// handing them to the engine.
    pub samples: Vec<f32>,
    /// Sample rate of `samples` (the source's native capture rate).
    pub sample_rate: u32,
    /// Meeting-relative offset (seconds) at which this chunk's audio began —
    /// used for the `[HH:MM:SS]` timestamp and for time-overlap dedup.
    pub start_offset: f32,
    /// Whether any frame in this chunk cleared the speech threshold. A chunk
    /// flushed purely by the 30s cap during dead air carries `false`, and the
    /// worker skips transcribing it rather than emit an empty line.
    pub has_speech: bool,
    /// Total duration (seconds) of frames that cleared the speech threshold.
    /// `has_speech == speech_secs > 0.0` — kept as a separate field so the
    /// caller can apply a minimum-length gate ([`MIN_SPEECH_SECS`]) without
    /// re-analysing audio.
    pub speech_secs: f32,
    /// Whether this chunk's leading words duplicate the previous chunk's
    /// trailing words (it began inside a hard cap cut's overlap) and must be
    /// seam-deduped against them with `chunking::dedup_seam`.
    pub seam_dedup: bool,
    /// This chunk ended at a hard cap cut; its last word may be incomplete.
    pub hard_cut: bool,
}

impl Default for Chunk {
    /// An empty `Them` chunk at 16 kHz. Exists so a caller can construct one
    /// with `..Default::default()` and keep compiling as fields are added —
    /// `speech_secs` and `seam_dedup` arrived after the first consumers.
    fn default() -> Self {
        Self {
            source: Source::Them,
            samples: Vec::new(),
            sample_rate: 16_000,
            start_offset: 0.0,
            has_speech: false,
            speech_secs: 0.0,
            seam_dedup: false,
            hard_cut: false,
        }
    }
}

impl Chunk {
    /// Meeting-relative offset (seconds) at which this chunk's audio ended.
    pub fn end_offset(&self) -> f32 {
        self.start_offset + self.samples.len() as f32 / self.sample_rate.max(1) as f32
    }
}

/// Formats a whole-second meeting offset as `HH:MM:SS`.
pub fn format_offset(total_secs: u64) -> String {
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    format!("{:02}:{:02}:{:02}", h, m, s)
}

/// Whether the current buffer should be flushed as a chunk, given how many
/// samples are buffered, whether speech has been heard, and the length of the
/// current trailing-silence run — all in samples at `sample_rate`. Pure and
/// platform-independent so the flush policy is testable without audio.
pub fn should_flush_chunk(
    buffered_samples: usize,
    has_speech: bool,
    trailing_silence_samples: usize,
    sample_rate: u32,
    hold_secs: f32,
) -> bool {
    let sr = sample_rate.max(1) as f32;
    let max_samples = (CHUNK_MAX_SECS * sr) as usize;
    if buffered_samples >= max_samples {
        return true;
    }
    let hold_samples = (hold_secs * sr) as usize;
    has_speech && trailing_silence_samples >= hold_samples
}

/// Whether a cap cut landed in silence (the chunks concatenate cleanly) or
/// mid-speech (the next chunk overlaps and must be seam-deduped). Mirrors
/// `chunking::CutKind` for the streaming path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapCut {
    /// Cut just after a quiet frame — nothing is repeated across the seam.
    Silence,
    /// No quiet frame in the window: cut at the cap, hand the following chunk
    /// a [`CHUNK_OVERLAP_SECS`] overlap and let `dedup_seam` clean it up.
    Hard,
}

/// Where a capped chunk should end, given one energy value per ~100ms frame.
/// Scans frames from [`CHUNK_CUT_WINDOW_MIN_SECS`] onward for the quietest; if
/// it is below [`SILENCE_RMS_THRESHOLD`] the cut lands just after it
/// ([`CapCut::Silence`], clean concatenation), otherwise the window is
/// continuous speech and we cut at the cap with an overlap ([`CapCut::Hard`]).
/// Pure, so the policy is testable with synthetic energy profiles — the same
/// separation `chunking::plan_cut` uses.
pub fn plan_cap_cut(frame_energies: &[f32]) -> (usize, CapCut) {
    let hard_cut = frame_energies.len();
    let win_start = (CHUNK_CUT_WINDOW_MIN_SECS / FRAME_SECS) as usize;
    if win_start >= hard_cut {
        // Shorter than the search window (only reachable if the cap itself is
        // reduced) — there is nowhere to look, so cut at the end.
        return (hard_cut, CapCut::Hard);
    }

    let mut best_rms = f32::INFINITY;
    let mut best_at = hard_cut;
    for (idx, &energy) in frame_energies.iter().enumerate().skip(win_start) {
        if energy < best_rms {
            best_rms = energy;
            // Cut *after* the quiet frame so the silence closes the emitted
            // chunk rather than opening the next one.
            best_at = idx + 1;
        }
    }

    if best_rms < SILENCE_RMS_THRESHOLD {
        (best_at, CapCut::Silence)
    } else {
        (hard_cut, CapCut::Hard)
    }
}

/// Splits `body` into windows of at most `window_chars` characters for the
/// hierarchical summarizer, splitting only on line boundaries so a single
/// `Them:`/`Me:` line is never cut in half. A line longer than the window on
/// its own becomes its own (over-budget) window rather than being split
/// mid-line. Empty input yields an empty vec.
pub fn split_for_summary(body: &str, window_chars: usize) -> Vec<String> {
    let window_chars = window_chars.max(1);
    let mut windows = Vec::new();
    let mut current = String::new();

    for line in body.lines() {
        let line_len = line.chars().count();
        let sep_len = if current.is_empty() { 0 } else { 1 };
        if !current.is_empty() && current.chars().count() + sep_len + line_len > window_chars {
            windows.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(line);
    }
    if !current.is_empty() {
        windows.push(current);
    }
    windows
}

/// Returns the tail of `transcript` to feed the summarizer, plus whether it
/// was truncated. If the transcript fits within `max_chars` it's returned
/// whole (`false`); otherwise only the last `max_chars` characters are kept
/// (`true`), so the summary reflects the final — usually most conclusive —
/// portion of a very long meeting rather than crashing on an over-budget
/// prompt. Char-based (not byte-based) so it never splits a UTF-8 sequence.
pub fn truncate_for_summary(transcript: &str, max_chars: usize) -> (String, bool) {
    let total = transcript.chars().count();
    if total <= max_chars {
        return (transcript.to_string(), false);
    }
    let tail: String = transcript.chars().skip(total - max_chars).collect();
    (tail, true)
}

/// Root-mean-square energy of a frame. Shared with the long-form dictation
/// chunker (`crate::chunking`), which reuses this module's RMS-frame helpers
/// and thresholds rather than re-implementing them. (The copy in `audio.rs`
/// stays private to that module.)
pub(crate) fn rms(frame: &[f32]) -> f32 {
    if frame.is_empty() {
        return 0.0;
    }
    let sum_sq: f32 = frame.iter().map(|s| s * s).sum();
    (sum_sq / frame.len() as f32).sqrt()
}

/// Accumulates native-rate mono samples for one source and emits [`Chunk`]s
/// at natural pauses or the 30s cap. Not `Send`-bound to anything; the caller
/// runs one per source on that source's driver thread.
pub struct StreamingChunker {
    source: Source,
    sample_rate: u32,
    /// Samples buffered since the last flush.
    buffer: Vec<f32>,
    /// Leftover samples not yet forming a full analysis frame.
    frame_accum: Vec<f32>,
    frame_samples: usize,
    /// Whether any frame since the last flush cleared the speech threshold.
    has_speech: bool,
    /// Length (samples) of the current trailing run of sub-silence-threshold
    /// audio.
    trailing_silence: usize,
    /// Total samples consumed since the meeting began, for offset math.
    elapsed_samples: u64,
    /// Meeting-relative offset (seconds) of `buffer[0]`. Advances with the
    /// leading silence that precedes speech so the timestamp marks when the
    /// speaker actually started, not when the buffer opened.
    chunk_start_offset: f32,
    /// Trailing near-silence required to close a chunk at a natural pause,
    /// per source. Defaults to [`SILENCE_HOLD_SECS`]; see
    /// [`Self::set_silence_hold_secs`].
    silence_hold_secs: f32,
    /// RMS of every buffered frame, in order — `buffer` is exactly
    /// `frame_energies.len() * frame_samples` samples long. Stored so the cap
    /// cut is a pure decision over energies ([`plan_cap_cut`]) and so the
    /// retained tail's `has_speech` / `trailing_silence` can be re-derived
    /// without re-analysing audio.
    frame_energies: Vec<f32>,
    /// How many buffered frames cleared [`SPEECH_RMS_THRESHOLD`] — the
    /// numerator of [`Chunk::speech_secs`].
    speech_frames: usize,
    /// Whether the *next* emitted chunk begins inside the previous chunk's
    /// hard-cut overlap and therefore needs a seam dedup.
    pending_seam_dedup: bool,
}

impl StreamingChunker {
    pub fn new(source: Source, sample_rate: u32) -> Self {
        let sr = sample_rate.max(1);
        Self {
            source,
            sample_rate: sr,
            buffer: Vec::new(),
            frame_accum: Vec::new(),
            frame_samples: ((FRAME_SECS * sr as f32) as usize).max(1),
            has_speech: false,
            trailing_silence: 0,
            elapsed_samples: 0,
            chunk_start_offset: 0.0,
            silence_hold_secs: SILENCE_HOLD_SECS,
            frame_energies: Vec::new(),
            speech_frames: 0,
            pending_seam_dedup: false,
        }
    }

    /// Overrides this chunker's trailing-silence hold, clamped to
    /// `0.3..=3.0` seconds. Lets a per-source policy (e.g. a chattier
    /// microphone vs. a system-audio feed with cross-talk) diverge from the
    /// documented default without touching it for every caller.
    pub fn set_silence_hold_secs(&mut self, secs: f32) {
        self.silence_hold_secs = secs.clamp(0.3, 3.0);
    }

    /// Feeds a block of native-rate mono samples. Returns any chunks that
    /// closed as a result (usually zero or one; more only if a single block
    /// were longer than the 30s cap, which never happens with real capture
    /// block sizes).
    pub fn push(&mut self, samples: &[f32]) -> Vec<Chunk> {
        let mut out = Vec::new();
        self.frame_accum.extend_from_slice(samples);
        while self.frame_accum.len() >= self.frame_samples {
            let frame: Vec<f32> = self.frame_accum.drain(..self.frame_samples).collect();
            if let Some(chunk) = self.push_frame(&frame) {
                out.push(chunk);
            }
        }
        out
    }

    /// Processes exactly one analysis frame.
    fn push_frame(&mut self, frame: &[f32]) -> Option<Chunk> {
        let energy = rms(frame);
        let is_speech = energy >= SPEECH_RMS_THRESHOLD;

        if self.buffer.is_empty() && !self.has_speech && !is_speech {
            // Leading silence: don't buffer it, just advance the clock so the
            // next chunk's start offset reflects real speech onset.
            self.elapsed_samples += frame.len() as u64;
            self.chunk_start_offset = self.elapsed_samples as f32 / self.sample_rate as f32;
            return None;
        }

        self.buffer.extend_from_slice(frame);
        self.frame_energies.push(energy);
        self.elapsed_samples += frame.len() as u64;

        if is_speech {
            self.has_speech = true;
            self.speech_frames += 1;
            self.trailing_silence = 0;
        } else if energy < SILENCE_RMS_THRESHOLD {
            self.trailing_silence += frame.len();
        }
        // Frames between the two thresholds neither arm speech nor extend the
        // silence run (hysteresis dead-band): they're just buffered.

        if should_flush_chunk(
            self.buffer.len(),
            self.has_speech,
            self.trailing_silence,
            self.sample_rate,
            self.silence_hold_secs,
        ) {
            // `should_flush_chunk` decides *whether* to close the chunk; at the
            // cap, `plan_cap_cut` decides *where*.
            let at_cap = self.buffer.len()
                >= (CHUNK_MAX_SECS * self.sample_rate as f32) as usize;
            return Some(if at_cap {
                self.take_capped_chunk()
            } else {
                self.take_chunk()
            });
        }
        None
    }

    /// Flushes whatever is buffered as a final chunk (called once per source
    /// when the meeting stops). Returns `None` if nothing is buffered.
    pub fn flush(&mut self) -> Option<Chunk> {
        if self.buffer.is_empty() {
            return None;
        }
        Some(self.take_chunk())
    }

    /// Emits the whole buffer — the natural-pause and end-of-meeting path,
    /// where nothing is retained.
    fn take_chunk(&mut self) -> Chunk {
        let samples = std::mem::take(&mut self.buffer);
        let speech_secs = self.frames_to_secs(self.speech_frames);
        let chunk = Chunk {
            source: self.source,
            samples,
            sample_rate: self.sample_rate,
            start_offset: self.chunk_start_offset,
            has_speech: self.has_speech,
            speech_secs,
            seam_dedup: self.pending_seam_dedup,
            hard_cut: false,
        };
        // Reset for the next span; the next buffered sample sets the offset.
        self.frame_energies.clear();
        self.speech_frames = 0;
        self.pending_seam_dedup = false;
        self.has_speech = false;
        self.trailing_silence = 0;
        self.chunk_start_offset = self.elapsed_samples as f32 / self.sample_rate as f32;
        chunk
    }

    /// Emits a chunk at the 30s cap, cutting at the quietest frame of the
    /// search window instead of exactly on the boundary and *retaining* the
    /// audio past the cut for the next chunk (plus a [`CHUNK_OVERLAP_SECS`]
    /// overlap when there was no quiet frame to cut at).
    ///
    /// The offset arithmetic is the load-bearing part: `chunk_start_offset`
    /// must stay the true meeting offset of `buffer[0]`, so it advances by
    /// exactly the frames that leave the buffer — the ones before the retain
    /// point, then any leading silence trimmed off the retained tail.
    fn take_capped_chunk(&mut self) -> Chunk {
        if self.frame_energies.is_empty() {
            // Unreachable with real capture (the cap needs 30s of frames
            // buffered) — fall back rather than index into nothing.
            return self.take_chunk();
        }

        let (cut_frame, kind) = plan_cap_cut(&self.frame_energies);
        let cut_frame = cut_frame.clamp(1, self.frame_energies.len());
        let overlap_frames = match kind {
            // A silence cut concatenates cleanly: nothing to repeat.
            CapCut::Silence => 0,
            CapCut::Hard => {
                ((CHUNK_OVERLAP_SECS / FRAME_SECS).round() as usize).min(cut_frame)
            }
        };
        let retain_from = cut_frame - overlap_frames;

        // What goes out: everything up to the cut. `has_speech` and
        // `speech_secs` describe the *emitted* frames, not the buffer.
        let emitted_speech = self.frame_energies[..cut_frame]
            .iter()
            .filter(|e| **e >= SPEECH_RMS_THRESHOLD)
            .count();
        let speech_secs = self.frames_to_secs(emitted_speech);
        let chunk = Chunk {
            source: self.source,
            samples: self.buffer[..cut_frame * self.frame_samples].to_vec(),
            sample_rate: self.sample_rate,
            start_offset: self.chunk_start_offset,
            has_speech: emitted_speech > 0,
            speech_secs,
            seam_dedup: self.pending_seam_dedup,
            hard_cut: matches!(kind, CapCut::Hard),
        };

        // What stays: the tail from `retain_from` on (the overlap included).
        self.buffer.drain(..retain_from * self.frame_samples);
        self.frame_energies.drain(..retain_from);
        self.chunk_start_offset += self.frames_to_secs(retain_from);

        // Normalise the retained tail with the same leading-silence rule
        // `push_frame` applies to a fresh buffer: the offset marks speech
        // onset, not when the buffer happened to open.
        match self
            .frame_energies
            .iter()
            .position(|e| *e >= SPEECH_RMS_THRESHOLD)
        {
            Some(first_speech) => {
                self.buffer.drain(..first_speech * self.frame_samples);
                self.frame_energies.drain(..first_speech);
                self.chunk_start_offset += self.frames_to_secs(first_speech);
            }
            None => {
                // Nothing but silence after the cut — retaining it would only
                // pin the next chunk's timestamp back here. Drop it and reopen
                // at "now", exactly as the leading-silence arm does.
                self.buffer.clear();
                self.frame_energies.clear();
                self.chunk_start_offset =
                    self.elapsed_samples as f32 / self.sample_rate as f32;
            }
        }

        // Re-derive the running state from the retained energies (no re-RMS),
        // replaying `push_frame`'s rules: speech resets the silence run, a
        // dead-band frame leaves it alone.
        let mut speech_frames = 0usize;
        let mut trailing_silence = 0usize;
        for &energy in &self.frame_energies {
            if energy >= SPEECH_RMS_THRESHOLD {
                speech_frames += 1;
                trailing_silence = 0;
            } else if energy < SILENCE_RMS_THRESHOLD {
                trailing_silence += self.frame_samples;
            }
        }
        self.speech_frames = speech_frames;
        self.has_speech = speech_frames > 0;
        self.trailing_silence = trailing_silence;
        self.pending_seam_dedup = matches!(kind, CapCut::Hard);

        chunk
    }

    /// Duration of `frames` whole analysis frames. Derived from the frame's
    /// real sample count rather than [`FRAME_SECS`] so the offset arithmetic
    /// stays exact at capture rates where `FRAME_SECS * sample_rate` truncates.
    fn frames_to_secs(&self, frames: usize) -> f32 {
        (frames * self.frame_samples) as f32 / self.sample_rate as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_offset_pads_hms() {
        assert_eq!(format_offset(0), "00:00:00");
        assert_eq!(format_offset(9), "00:00:09");
        assert_eq!(format_offset(72), "00:01:12"); // 1m12s
        assert_eq!(format_offset(3_723), "01:02:03"); // 1h2m3s
    }

    #[test]
    fn flush_fires_on_trailing_silence_after_speech() {
        // 1.2s of silence at 16kHz = 19200 samples; just at the bar flushes.
        let sr = 16_000;
        let hold = (SILENCE_HOLD_SECS * sr as f32) as usize;
        assert!(should_flush_chunk(hold + 1000, true, hold, sr, SILENCE_HOLD_SECS));
        assert!(!should_flush_chunk(hold + 1000, true, hold - 1, sr, SILENCE_HOLD_SECS)); // one sample short
    }

    #[test]
    fn flush_never_fires_on_silence_without_prior_speech() {
        let sr = 16_000;
        let hold = (SILENCE_HOLD_SECS * sr as f32) as usize;
        // Long silence run but no speech ever heard -> must not flush.
        assert!(!should_flush_chunk(hold * 10, false, hold * 10, sr, SILENCE_HOLD_SECS));
    }

    #[test]
    fn flush_fires_at_the_hard_cap_even_mid_speech() {
        let sr = 16_000;
        let max = (CHUNK_MAX_SECS * sr as f32) as usize;
        // No trailing silence, still speaking, but 30s buffered -> cap flush.
        assert!(should_flush_chunk(max, true, 0, sr, SILENCE_HOLD_SECS));
        assert!(!should_flush_chunk(max - 1, true, 0, sr, SILENCE_HOLD_SECS));
    }

    #[test]
    fn truncate_keeps_short_transcripts_whole() {
        let (out, truncated) = truncate_for_summary("short meeting", 6_000);
        assert_eq!(out, "short meeting");
        assert!(!truncated);
    }

    #[test]
    fn truncate_returns_the_tail_of_long_transcripts() {
        let long: String = "abcdefghij".repeat(1_000); // 10k chars
        let (out, truncated) = truncate_for_summary(&long, 6_000);
        assert!(truncated);
        assert_eq!(out.chars().count(), 6_000);
        // It's the *tail*, so it ends with the transcript's ending.
        assert!(long.ends_with(&out));
    }

    #[test]
    fn truncate_is_utf8_safe_on_multibyte_boundaries() {
        let s = "é".repeat(100); // 100 chars, 200 bytes
        let (out, truncated) = truncate_for_summary(&s, 40);
        assert!(truncated);
        assert_eq!(out.chars().count(), 40);
    }

    /// Builds a block of `secs` seconds of samples at `amp` amplitude
    /// (constant DC — RMS == |amp|), at 16kHz.
    fn block(secs: f32, amp: f32) -> Vec<f32> {
        vec![amp; (secs * 16_000.0) as usize]
    }

    #[test]
    fn chunker_emits_a_chunk_after_speech_then_silence() {
        let mut c = StreamingChunker::new(Source::Them, 16_000);
        // 1s of clear speech (amp 0.3 >> speech threshold).
        let chunks = c.push(&block(1.0, 0.3));
        assert!(chunks.is_empty(), "shouldn't flush mid-speech");
        // 1.3s of silence (amp 0.0) -> past the 1.2s hold -> one chunk.
        let chunks = c.push(&block(1.3, 0.0));
        assert_eq!(chunks.len(), 1);
        let chunk = &chunks[0];
        assert_eq!(chunk.source, Source::Them);
        assert!(chunk.has_speech);
        // Speech started at offset ~0 (no leading silence trimmed here).
        assert!(chunk.start_offset < 0.2, "start offset {}", chunk.start_offset);
    }

    #[test]
    fn chunker_trims_leading_silence_from_the_start_offset() {
        let mut c = StreamingChunker::new(Source::Me, 16_000);
        // 2s of dead air first — must not buffer, but advances the clock.
        assert!(c.push(&block(2.0, 0.0)).is_empty());
        // Then speech, then a pause.
        assert!(c.push(&block(1.0, 0.3)).is_empty());
        let chunks = c.push(&block(1.3, 0.0));
        assert_eq!(chunks.len(), 1);
        // Speech onset was ~2s in, so the timestamp should be ~2s, not 0.
        assert!(chunks[0].start_offset >= 1.8, "start offset {}", chunks[0].start_offset);
    }

    #[test]
    fn chunker_flush_returns_buffered_tail_at_stop() {
        let mut c = StreamingChunker::new(Source::Them, 16_000);
        // Speech with no trailing silence yet — nothing emitted...
        assert!(c.push(&block(0.5, 0.3)).is_empty());
        // ...until the explicit end-of-meeting flush.
        let tail = c.flush().expect("buffered speech should flush at stop");
        assert!(tail.has_speech);
        assert!(c.flush().is_none(), "second flush on empty buffer is None");
    }

    #[test]
    fn chunker_end_offset_reflects_chunk_duration() {
        let mut c = StreamingChunker::new(Source::Them, 16_000);
        c.push(&block(1.0, 0.3));
        let chunks = c.push(&block(1.3, 0.0));
        let chunk = &chunks[0];
        // ~2.3s of audio buffered (1s speech + 1.3s trailing silence).
        let dur = chunk.end_offset() - chunk.start_offset;
        assert!(dur > 2.0 && dur < 2.6, "duration {dur}");
    }

    #[test]
    fn flush_hold_is_parameterised() {
        let sr = 16_000;
        let hold_at_0_8s = (0.8 * sr as f32) as usize;
        assert!(should_flush_chunk(hold_at_0_8s + 1000, true, hold_at_0_8s, sr, 0.8));
        assert!(!should_flush_chunk(hold_at_0_8s + 1000, true, hold_at_0_8s, sr, 1.2));
    }

    #[test]
    fn default_hold_matches_the_documented_constant() {
        let sr = 16_000;
        let hold = (SILENCE_HOLD_SECS * sr as f32) as usize;
        assert!(should_flush_chunk(hold, true, hold, sr, SILENCE_HOLD_SECS));
        assert!(!should_flush_chunk(hold, true, hold - 1, sr, SILENCE_HOLD_SECS));
    }

    #[test]
    fn split_never_breaks_a_line() {
        let body = "Them: line one\nMe: line two\nThem: line three\n";
        // Force a window small enough that each line must be its own window.
        let windows = split_for_summary(body, 15);
        for w in &windows {
            for line in body.lines() {
                if w.contains(line) {
                    // The full line must appear intact, not a fragment of it.
                    assert!(w.lines().any(|l| l == line), "line split: {w:?}");
                }
            }
        }
        // Reassembling the windows' lines in order reproduces every line.
        let reassembled: Vec<&str> = windows.iter().flat_map(|w| w.lines()).collect();
        let original: Vec<&str> = body.lines().collect();
        assert_eq!(reassembled, original);
    }

    #[test]
    fn split_returns_one_window_for_short_bodies() {
        let body = "Them: hi\nMe: hello\n";
        let windows = split_for_summary(body, 6_000);
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0], "Them: hi\nMe: hello");
    }

    #[test]
    fn split_puts_an_over_long_line_in_its_own_window() {
        let long_line = "Them: ".to_string() + &"x".repeat(100);
        let body = format!("Me: short\n{long_line}\nMe: another short line");
        let windows = split_for_summary(&body, 20);
        // The over-long line must appear whole, in a window by itself.
        assert!(windows.iter().any(|w| w == &long_line));
    }

    #[test]
    fn plan_cap_cut_picks_the_quiet_frame_in_the_window() {
        // 30s of loud frames with one quiet frame at 27.0s — inside the window.
        let mut energies = vec![0.3_f32; 300];
        energies[270] = 0.001;
        let (cut, kind) = plan_cap_cut(&energies);
        assert_eq!(kind, CapCut::Silence);
        assert_eq!(cut, 271, "the cut lands just after the quiet frame");
    }

    #[test]
    fn plan_cap_cut_ignores_a_quiet_frame_before_the_window() {
        // The only quiet frame is at 10.0s: cutting there would throw away 20s
        // of speech, so the window (24s+) must not see it.
        let mut energies = vec![0.3_f32; 300];
        energies[100] = 0.001;
        let (cut, kind) = plan_cap_cut(&energies);
        assert_eq!(kind, CapCut::Hard);
        assert_eq!(cut, energies.len());
    }

    #[test]
    fn plan_cap_cut_hard_cuts_continuous_speech() {
        let energies = vec![0.3_f32; 300];
        let (cut, kind) = plan_cap_cut(&energies);
        assert_eq!(kind, CapCut::Hard);
        assert_eq!(cut, 300);
    }

    #[test]
    fn cap_cut_at_a_pause_retains_the_tail_and_keeps_offsets_monotonic() {
        let mut c = StreamingChunker::new(Source::Them, 16_000);
        // 27s of speech, a 0.3s breath (far short of the 1.2s hold), then more
        // speech — so the 30s cap, not the pause, closes the chunk.
        assert!(c.push(&block(27.0, 0.3)).is_empty());
        assert!(c.push(&block(0.3, 0.0)).is_empty());
        let chunks = c.push(&block(5.0, 0.3));
        assert_eq!(chunks.len(), 1);

        let first = &chunks[0];
        let dur = first.end_offset() - first.start_offset;
        // Cut at the breath (~27.1s), not mid-word on the 30s boundary.
        assert!((27.0..27.5).contains(&dur), "duration {dur}");
        assert!(!first.seam_dedup, "a silence cut repeats nothing");
        assert!(!first.hard_cut);

        // The retained tail keeps the clock exact: the next chunk starts where
        // speech resumed after the breath, ~27.3s in.
        let tail = c.flush().expect("the retained tail should flush at stop");
        assert!(
            tail.start_offset >= first.start_offset,
            "start offsets went backwards: {} after {}",
            tail.start_offset,
            first.start_offset
        );
        assert!(
            (tail.start_offset - 27.3).abs() < 0.15,
            "start offset {}",
            tail.start_offset
        );
        assert!(!tail.seam_dedup);
    }

    #[test]
    fn hard_cap_cut_overlaps_and_flags_the_next_chunk() {
        let mut c = StreamingChunker::new(Source::Me, 16_000);
        // 60s without a single quiet frame: both caps must hard-cut.
        let chunks = c.push(&block(60.0, 0.3));
        assert_eq!(chunks.len(), 2, "expected two capped chunks");
        assert!(chunks.iter().all(|chunk| chunk.hard_cut));
        assert!(!c.flush().unwrap().hard_cut, "stop flush is not a hard cut");
        assert!(!chunks[0].seam_dedup, "the first chunk overlaps nothing");
        assert!(
            chunks[1].seam_dedup,
            "a hard cut hands its overlap to the next chunk"
        );
        // The second chunk re-reads the last second of the first.
        let expected = chunks[0].end_offset() - CHUNK_OVERLAP_SECS;
        assert!(
            (chunks[1].start_offset - expected).abs() < 0.05,
            "start offset {} expected {expected}",
            chunks[1].start_offset
        );
    }

    #[test]
    fn chunk_reports_speech_seconds() {
        let mut c = StreamingChunker::new(Source::Them, 16_000);
        c.push(&block(1.0, 0.3));
        let chunks = c.push(&block(1.3, 0.0));
        assert_eq!(chunks.len(), 1);
        let speech = chunks[0].speech_secs;
        assert!((0.9..=1.1).contains(&speech), "speech_secs {speech}");
        // The 1.3s of trailing silence is in the chunk but is not speech.
        let dur = chunks[0].end_offset() - chunks[0].start_offset;
        assert!(speech < dur, "speech {speech} should be under duration {dur}");
        assert!(chunks[0].has_speech);

        // A 0.1s blip arms a chunk but falls under the transcribe-it bar.
        c.push(&block(0.1, 0.3));
        let blip = c.push(&block(1.3, 0.0));
        assert_eq!(blip.len(), 1);
        assert!(
            blip[0].speech_secs < MIN_SPEECH_SECS,
            "blip speech_secs {}",
            blip[0].speech_secs
        );

        // Pure silence never arms a chunk at all, so there is nothing to
        // report: no zero-speech chunk is ever emitted.
        assert!(c.push(&block(2.0, 0.0)).is_empty());
        assert!(c.flush().is_none());
    }

    #[test]
    fn a_retained_tail_of_pure_silence_is_dropped_and_the_offset_jumps() {
        let mut c = StreamingChunker::new(Source::Them, 16_000);
        assert!(c.push(&block(29.5, 0.3)).is_empty());
        // Dead air from 29.5s on: the cap fires at 30s, the quiet frame at
        // 29.5s wins the window, and the 0.5s tail it would retain is silence.
        let chunks = c.push(&block(3.0, 0.0));
        assert_eq!(chunks.len(), 1);
        let dur = chunks[0].end_offset() - chunks[0].start_offset;
        assert!((29.4..29.8).contains(&dur), "duration {dur}");

        // Speech resumes at 32.5s and the next chunk says so — the dropped
        // tail did not pin the timestamp back at 29.6s.
        assert!(c.push(&block(1.0, 0.3)).is_empty());
        let next = c.push(&block(1.3, 0.0));
        assert_eq!(next.len(), 1);
        assert!(
            (next[0].start_offset - 32.5).abs() < 0.15,
            "start offset {}",
            next[0].start_offset
        );
    }

    #[test]
    fn start_offsets_are_monotonic_over_ninety_seconds() {
        let mut c = StreamingChunker::new(Source::Them, 16_000);
        // A mixed 90s profile: gaps long enough to close a chunk, pauses too
        // short to, and monologues long enough to hit the cap twice.
        let profile: [(f32, f32); 8] = [
            (7.0, 0.3),
            (1.5, 0.0),
            (22.0, 0.3),
            (0.4, 0.0),
            (40.0, 0.3),
            (2.0, 0.0),
            (14.0, 0.3),
            (3.1, 0.0),
        ];
        let mut chunks = Vec::new();
        let mut pushed = 0.0_f32;
        for (secs, amp) in profile {
            let secs = secs.min(90.0 - pushed);
            if secs <= 0.0 {
                break;
            }
            chunks.extend(c.push(&block(secs, amp)));
            pushed += secs;
        }
        chunks.extend(c.flush());

        assert!(chunks.len() >= 3, "expected several chunks, got {}", chunks.len());
        let mut prev_start = f32::NEG_INFINITY;
        for chunk in &chunks {
            assert!(
                chunk.start_offset >= prev_start,
                "start offsets went backwards: {} after {prev_start}",
                chunk.start_offset
            );
            prev_start = chunk.start_offset;
            let dur = chunk.end_offset() - chunk.start_offset;
            assert!(
                dur <= CHUNK_MAX_SECS + FRAME_SECS,
                "chunk of {dur}s exceeds the cap"
            );
            assert!(
                chunk.start_offset <= pushed,
                "start offset {} past the {pushed}s of audio",
                chunk.start_offset
            );
        }
    }
}
