//! Per-source streaming chunker + the small pure helpers around it
//! (flush-decision, timestamp formatting, summary-input truncation), all
//! factored out so they're unit-testable on every platform without a model,
//! an audio device, or ScreenCaptureKit.
//!
//! Each capture source (system audio, microphone) owns its own
//! [`StreamingChunker`], fed blocks of native-rate mono `f32` as they arrive.
//! The chunker decides when a contiguous span of speech has ended — a
//! trailing run of near-silence, or a hard 30s cap — and emits it as a
//! [`Chunk`] to be transcribed. Working at the source's native sample rate
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
) -> bool {
    let sr = sample_rate.max(1) as f32;
    let max_samples = (CHUNK_MAX_SECS * sr) as usize;
    if buffered_samples >= max_samples {
        return true;
    }
    let hold_samples = (SILENCE_HOLD_SECS * sr) as usize;
    has_speech && trailing_silence_samples >= hold_samples
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
        }
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
        self.elapsed_samples += frame.len() as u64;

        if is_speech {
            self.has_speech = true;
            self.trailing_silence = 0;
        } else if energy < SILENCE_RMS_THRESHOLD {
            self.trailing_silence += frame.len();
        }
        // Frames between the two thresholds neither arm speech nor extend the
        // silence run (hysteresis dead-band): they're just buffered.

        if should_flush_chunk(self.buffer.len(), self.has_speech, self.trailing_silence, self.sample_rate) {
            return Some(self.take_chunk());
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

    fn take_chunk(&mut self) -> Chunk {
        let samples = std::mem::take(&mut self.buffer);
        let chunk = Chunk {
            source: self.source,
            samples,
            sample_rate: self.sample_rate,
            start_offset: self.chunk_start_offset,
            has_speech: self.has_speech,
        };
        // Reset for the next span; the next buffered sample sets the offset.
        self.has_speech = false;
        self.trailing_silence = 0;
        self.chunk_start_offset = self.elapsed_samples as f32 / self.sample_rate as f32;
        chunk
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
        assert!(should_flush_chunk(hold + 1000, true, hold, sr));
        assert!(!should_flush_chunk(hold + 1000, true, hold - 1, sr)); // one sample short
    }

    #[test]
    fn flush_never_fires_on_silence_without_prior_speech() {
        let sr = 16_000;
        let hold = (SILENCE_HOLD_SECS * sr as f32) as usize;
        // Long silence run but no speech ever heard -> must not flush.
        assert!(!should_flush_chunk(hold * 10, false, hold * 10, sr));
    }

    #[test]
    fn flush_fires_at_the_hard_cap_even_mid_speech() {
        let sr = 16_000;
        let max = (CHUNK_MAX_SECS * sr as f32) as usize;
        // No trailing silence, still speaking, but 30s buffered -> cap flush.
        assert!(should_flush_chunk(max, true, 0, sr));
        assert!(!should_flush_chunk(max - 1, true, 0, sr));
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
}
