//! Rolling (during-recording) transcription.
//!
//! The batch path buffers the whole recording and only starts transcribing at
//! release, so a 10-minute hold pays ~30s of chunked transcription *after* the
//! user lets go. This module instead transcribes silence-completed chunks
//! **while recording continues**: as settled audio accumulates past ~35s it is
//! cut (reusing [`crate::chunking::plan_cut`]) and dispatched to the model
//! manager in the background, so at release only the final <35s tail remains —
//! dropping end-latency from ~30s to a single tail transcription.
//!
//! Two independent pieces live here:
//!   - [`IncrementalResampler`] — the streaming 16 kHz resampler the audio
//!     worker feeds mic chunks into, emitting settled 16 kHz mono increments
//!     and dropping consumed input so the capture buffer doesn't grow with the
//!     recording length.
//!   - [`RollingSession`] + [`spawn_rolling_worker`] — accumulate those
//!     increments, cut chunks with the shared silence-aware policy, dispatch
//!     them to the engine as they settle, and (at release) assemble every
//!     chunk + the tail back into one transcript via
//!     [`crate::chunking::assemble`].

use std::collections::VecDeque;
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::Duration;

use crate::chunking::{self, ChunkPlan, CutKind, OVERLAP_SECS, SAMPLE_RATE, SINGLE_PASS_MAX_SECS};
use crate::engine::Transcript;
use crate::model_manager::ModelCommand;

/// Streaming linear resampler that reproduces [`crate::audio::resample_linear`]
/// exactly, but incrementally: input is pushed a mic-callback chunk at a time,
/// settled output samples are drained as soon as the input they depend on is
/// available, and consumed input is dropped so memory stays bounded to a small
/// window rather than the whole recording.
///
/// "Settled" means both input samples a linear-interpolated output reads
/// (`floor(i*ratio)` and the next one) have arrived; the final one-sample tail
/// is emitted by [`Self::finish`]. Output index `i` maps to input position
/// `i * in_rate/out_rate`, identical to the batch resampler, so the streamed
/// concatenation is bit-for-bit equal to resampling the whole buffer at once.
pub struct IncrementalResampler {
    ratio: f64,
    passthrough: bool,
    /// Number of output samples already produced (drained or finished).
    produced: u64,
    /// Total input samples ever pushed.
    total_in: u64,
    /// Input samples from `buf_base` onward; earlier ones have been dropped.
    buf: VecDeque<f32>,
    buf_base: u64,
}

impl IncrementalResampler {
    pub fn new(in_rate: u32, out_rate: u32) -> Self {
        let passthrough = in_rate == out_rate || in_rate == 0 || out_rate == 0;
        Self {
            ratio: in_rate as f64 / out_rate.max(1) as f64,
            passthrough,
            produced: 0,
            total_in: 0,
            buf: VecDeque::new(),
            buf_base: 0,
        }
    }

    /// Feed one chunk of input (native-rate mono) samples.
    pub fn push(&mut self, mono: &[f32]) {
        self.buf.extend(mono.iter().copied());
        self.total_in += mono.len() as u64;
    }

    /// Input sample at absolute index `idx`, or `fallback` if it has been
    /// dropped or never existed. Never called for an already-dropped index in
    /// practice (we only drop below the next output's first input), so this is
    /// really the "past the end" guard.
    fn input_at(&self, idx: u64, fallback: f32) -> f32 {
        if idx < self.buf_base {
            return fallback;
        }
        self.buf
            .get((idx - self.buf_base) as usize)
            .copied()
            .unwrap_or(fallback)
    }

    /// Produce output sample `i` using the same linear formula as the batch
    /// resampler: `a + (b-a)*frac` with `a = input[floor(i*ratio)]` (0.0 past
    /// the end) and `b = input[floor(i*ratio)+1]` (falls back to `a`).
    fn sample(&self, i: u64) -> f32 {
        if self.passthrough {
            return self.input_at(i, 0.0);
        }
        let src_pos = i as f64 * self.ratio;
        let idx = src_pos.floor() as u64;
        let frac = (src_pos - idx as f64) as f32;
        let a = self.input_at(idx, 0.0);
        let b = self.input_at(idx + 1, a);
        a + (b - a) * frac
    }

    /// The first input index output `i` needs — everything below the value for
    /// the *next* output can be dropped.
    fn first_input_for(&self, i: u64) -> u64 {
        if self.passthrough {
            i
        } else {
            (i as f64 * self.ratio).floor() as u64
        }
    }

    /// Drain every output sample whose input has fully arrived, dropping input
    /// that no future output will read.
    pub fn drain_ready(&mut self) -> Vec<f32> {
        let mut out = Vec::new();
        loop {
            let i = self.produced;
            // Output `i` reads input floor(i*ratio) and the next one; only emit
            // once that next sample has arrived (so its value is final).
            let need = self.first_input_for(i) + 1;
            if need >= self.total_in {
                break;
            }
            out.push(self.sample(i));
            self.produced += 1;
        }
        self.drop_consumed_input();
        out
    }

    /// Emit any remaining output (the final tail sample the streaming rule held
    /// back) and release all buffered input. Total output length equals
    /// `ceil(total_in / ratio)`, matching the batch resampler.
    pub fn finish(&mut self) -> Vec<f32> {
        let out_len = if self.total_in == 0 {
            0
        } else if self.passthrough {
            self.total_in
        } else {
            (self.total_in as f64 / self.ratio).ceil() as u64
        };
        let mut out = Vec::new();
        while self.produced < out_len {
            out.push(self.sample(self.produced));
            self.produced += 1;
        }
        self.buf.clear();
        self.buf_base = self.total_in;
        out
    }

    fn produced_first_input(&self) -> u64 {
        self.first_input_for(self.produced)
    }

    /// Drop input samples strictly below the first index the next output needs,
    /// but never more than are actually buffered — preserving the invariant
    /// `buf_base + buf.len() == total_in`. (When the needed index is still
    /// ahead of everything buffered we simply drop the whole window.)
    fn drop_consumed_input(&mut self) {
        let keep_from = self.produced_first_input();
        let droppable = keep_from
            .saturating_sub(self.buf_base)
            .min(self.buf.len() as u64) as usize;
        for _ in 0..droppable {
            self.buf.pop_front();
        }
        self.buf_base += droppable as u64;
    }
}

/// Accumulates settled 16 kHz mono audio during a recording and cuts it into
/// transcription chunks with the same silence-aware policy as the batch path,
/// dropping each chunk's audio once handed off so peak buffered audio stays
/// ~one chunk rather than the whole recording.
pub struct RollingSession {
    /// Settled 16 kHz mono audio from `base` onward; earlier chunks dropped.
    buf: Vec<f32>,
    /// Absolute sample index of `buf[0]` in the full recording.
    base: usize,
    /// Total samples pushed so far.
    total: usize,
    /// Whether the *next* cut/tail chunk begins inside the previous chunk's
    /// hard-cut overlap and therefore needs seam de-dup.
    needs_dedup: bool,
    single_pass_max: usize,
    overlap: usize,
}

impl Default for RollingSession {
    fn default() -> Self {
        Self::new()
    }
}

impl RollingSession {
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            base: 0,
            total: 0,
            needs_dedup: false,
            single_pass_max: (SINGLE_PASS_MAX_SECS * SAMPLE_RATE as f32) as usize,
            overlap: (OVERLAP_SECS * SAMPLE_RATE as f32) as usize,
        }
    }

    pub fn push(&mut self, samples: &[f32]) {
        self.buf.extend_from_slice(samples);
        self.total += samples.len();
    }

    /// Total samples pushed across the whole recording.
    pub fn total_pushed(&self) -> usize {
        self.total
    }

    /// If more than one chunk's worth of settled audio has accumulated, cut the
    /// next chunk and return its plan (with an absolute `start`) plus its owned
    /// samples; the audio is dropped from the session. Returns `None` while the
    /// buffer is ≤35s (keep it as the live tail).
    pub fn try_cut(&mut self) -> Option<(ChunkPlan, Vec<f32>)> {
        if self.buf.len() <= self.single_pass_max {
            return None;
        }
        let (cut, kind) = chunking::plan_cut(&self.buf, SAMPLE_RATE);
        let plan = ChunkPlan {
            start: self.base,
            len: cut,
            needs_dedup: self.needs_dedup,
        };
        let samples = self.buf[..cut].to_vec();
        let (drop_count, next_dedup) = match kind {
            // Clean concatenation: the next chunk starts exactly at the cut.
            CutKind::Silence => (cut, false),
            // Mid-speech hard cut: keep the trailing overlap so the next chunk
            // re-hears it, and flag that chunk for seam de-dup.
            CutKind::Hard => (cut - self.overlap, true),
        };
        self.buf.drain(..drop_count);
        self.base += drop_count;
        self.needs_dedup = next_dedup;
        Some((plan, samples))
    }

    /// The final tail chunk (whatever settled audio remains, always ≤35s since
    /// [`Self::try_cut`] cuts everything above that). Consumes the buffer.
    pub fn finish(&mut self) -> (ChunkPlan, Vec<f32>) {
        let plan = ChunkPlan {
            start: self.base,
            len: self.buf.len(),
            needs_dedup: self.needs_dedup,
        };
        let samples = std::mem::take(&mut self.buf);
        self.base += samples.len();
        (plan, samples)
    }
}

/// Input to a [`spawn_rolling_worker`] thread.
pub enum RollingInput {
    /// Newly-settled 16 kHz mono audio from the capture path.
    Samples(Vec<f32>),
    /// The recording ended: transcribe the tail, then assemble and emit
    /// [`RollingOutput::Final`].
    Finish,
}

/// Output from a [`spawn_rolling_worker`] thread.
pub enum RollingOutput {
    /// A chunk finished transcribing during recording. `chunk_text` is its raw
    /// (no-dictionary, no-LLM) text, for the live preview pill.
    Preview { chunk_text: String },
    /// Every chunk plus the tail is transcribed and stitched. `raw_text` is the
    /// assembled raw transcript; the caller runs the normal pipeline
    /// (dictionary → cleanup → paste) on it.
    Final {
        raw_text: String,
        audio_duration: Duration,
    },
}

/// Spawns the background thread that owns a [`RollingSession`], dispatches
/// settled chunks to the model manager as they cut, and assembles the final
/// transcript at release. Returns the channel to feed it audio + the finish
/// signal on; drop that sender (without sending [`RollingInput::Finish`]) to
/// abandon the recording — the worker exits without emitting `Final`.
///
/// The engine is shared (chunks queue as [`ModelCommand::TranscribeChunk`] on
/// the one model-manager thread), so a rolling chunk mid-transcribe at release
/// finishes before the tail runs — dictation stays correctly ordered.
pub fn spawn_rolling_worker(
    model_cmd_tx: Sender<ModelCommand>,
    output_tx: Sender<RollingOutput>,
) -> Sender<RollingInput> {
    let (in_tx, in_rx) = mpsc::channel::<RollingInput>();

    thread::spawn(move || {
        let mut session = RollingSession::new();
        let mut plans: Vec<ChunkPlan> = Vec::new();
        let mut results: Vec<Option<Transcript>> = Vec::new();
        // Completed chunk transcriptions, tagged with their plan index so they
        // can be reassembled in order regardless of completion order.
        let (res_tx, res_rx) = mpsc::channel::<(usize, Result<Transcript, String>)>();

        let dispatch = |idx: usize, samples: Vec<f32>| {
            let (rtx, rrx) = mpsc::channel();
            if model_cmd_tx
                .send(ModelCommand::TranscribeChunk { samples, reply: rtx })
                .is_ok()
            {
                let res_tx = res_tx.clone();
                thread::spawn(move || {
                    let r = rrx
                        .recv()
                        .unwrap_or_else(|_| Err("chunk transcriber dropped".to_string()));
                    let _ = res_tx.send((idx, r));
                });
            } else {
                // Model manager gone: record an empty result so assembly never
                // waits on a chunk that will never complete.
                let _ = res_tx.send((idx, Err("transcriber unavailable".to_string())));
            }
        };

        loop {
            match in_rx.recv() {
                Ok(RollingInput::Samples(s)) => {
                    session.push(&s);
                    while let Some((plan, chunk)) = session.try_cut() {
                        let idx = plans.len();
                        plans.push(plan);
                        results.push(None);
                        dispatch(idx, chunk);
                    }
                    // Non-blockingly collect any completed chunks and preview
                    // the most recent one.
                    while let Ok((idx, r)) = res_rx.try_recv() {
                        if store_result(&mut results, idx, r) {
                            if let Some(t) = &results[idx] {
                                let _ = output_tx.send(RollingOutput::Preview {
                                    chunk_text: t.text.trim().to_string(),
                                });
                            }
                        }
                    }
                }
                Ok(RollingInput::Finish) => {
                    let (tail_plan, tail) = session.finish();
                    let idx = plans.len();
                    plans.push(tail_plan);
                    results.push(None);
                    dispatch(idx, tail);

                    // Block until every dispatched chunk (incl. the tail) has a
                    // result. No previews now — we're about to emit Final (the
                    // tail itself is intentionally never previewed, which is why
                    // preview lives only in the Samples arm above).
                    while results.iter().any(|r| r.is_none()) {
                        match res_rx.recv() {
                            Ok((i, r)) => {
                                store_result(&mut results, i, r);
                            }
                            Err(_) => break, // all forwarders gone; stop waiting
                        }
                    }

                    let transcripts: Vec<Transcript> = results
                        .into_iter()
                        .map(|o| o.unwrap_or_else(|| Transcript { text: String::new(), segments: None }))
                        .collect();
                    let assembled = chunking::assemble(&plans, &transcripts, SAMPLE_RATE);
                    let audio_duration =
                        Duration::from_secs_f64(session.total_pushed() as f64 / SAMPLE_RATE as f64);
                    let _ = output_tx.send(RollingOutput::Final {
                        raw_text: assembled.text,
                        audio_duration,
                    });
                    break;
                }
                Err(_) => break, // input dropped: recording abandoned (cancel).
            }
        }
    });

    in_tx
}

/// Stores a chunk result, turning an error into empty text (a single failed
/// chunk degrades to a gap rather than failing the whole dictation). Returns
/// whether the stored text is non-empty (worth previewing).
fn store_result(
    results: &mut [Option<Transcript>],
    idx: usize,
    r: Result<Transcript, String>,
) -> bool {
    let t = match r {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[vzt-flow] rolling chunk {idx} failed: {e}");
            Transcript { text: String::new(), segments: None }
        }
    };
    let non_empty = !t.text.trim().is_empty();
    if let Some(slot) = results.get_mut(idx) {
        *slot = Some(t);
    }
    non_empty
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::resample_linear;

    // ---- IncrementalResampler matches the batch resampler ----

    /// Push `input` through the incremental resampler in arbitrarily-sized
    /// slices, then finish, and return the full streamed output.
    fn stream(input: &[f32], in_rate: u32, out_rate: u32, chunk: usize) -> Vec<f32> {
        let mut r = IncrementalResampler::new(in_rate, out_rate);
        let mut out = Vec::new();
        for c in input.chunks(chunk.max(1)) {
            r.push(c);
            out.extend(r.drain_ready());
        }
        out.extend(r.finish());
        out
    }

    fn ramp(n: usize) -> Vec<f32> {
        (0..n).map(|i| (i as f32 * 0.013).sin()).collect()
    }

    #[test]
    fn incremental_matches_batch_48k_to_16k() {
        let input = ramp(48_000); // 1s at 48k
        let batch = resample_linear(&input, 48_000, 16_000);
        for chunk in [1usize, 7, 512, 4096, 100_000] {
            let streamed = stream(&input, 48_000, 16_000, chunk);
            assert_eq!(streamed.len(), batch.len(), "len mismatch at chunk {chunk}");
            for (i, (a, b)) in streamed.iter().zip(batch.iter()).enumerate() {
                assert!((a - b).abs() < 1e-6, "sample {i} differs at chunk {chunk}: {a} vs {b}");
            }
        }
    }

    #[test]
    fn incremental_matches_batch_44100_to_16k_non_integer_ratio() {
        let input = ramp(44_100);
        let batch = resample_linear(&input, 44_100, 16_000);
        let streamed = stream(&input, 44_100, 16_000, 333);
        assert_eq!(streamed.len(), batch.len());
        for (a, b) in streamed.iter().zip(batch.iter()) {
            assert!((a - b).abs() < 1e-6);
        }
    }

    #[test]
    fn incremental_passthrough_when_rates_equal() {
        let input = ramp(1000);
        let streamed = stream(&input, 16_000, 16_000, 64);
        assert_eq!(streamed, input);
    }

    #[test]
    fn incremental_drops_consumed_input_bounding_memory() {
        // After streaming a long input in small pushes, the retained window is
        // tiny (a couple samples), not the whole input.
        let mut r = IncrementalResampler::new(48_000, 16_000);
        for c in ramp(48_000 * 30).chunks(1024) {
            r.push(c);
            let _ = r.drain_ready();
            assert!(r.buf.len() < 4096, "retained window grew to {}", r.buf.len());
        }
    }

    // ---- RollingSession cutting mirrors plan_chunks ----

    fn secs(n: f32) -> usize {
        (n * SAMPLE_RATE as f32) as usize
    }

    #[test]
    fn rolling_session_no_cut_below_35s() {
        let mut s = RollingSession::new();
        s.push(&vec![0.3; secs(20.0)]);
        assert!(s.try_cut().is_none(), "must not cut a <35s buffer");
        let (plan, tail) = s.finish();
        assert_eq!(plan.start, 0);
        assert_eq!(tail.len(), secs(20.0));
        assert!(!plan.needs_dedup);
    }

    #[test]
    fn rolling_session_hard_cut_overlaps_and_flags_dedup() {
        // 75s of continuous speech pushed in 5s increments — same shape as
        // chunking's plan_chunks hard-cut test, but produced incrementally.
        let mut s = RollingSession::new();
        let block = vec![0.3f32; secs(5.0)];
        let mut cut_plans = Vec::new();
        for _ in 0..15 {
            s.push(&block);
            while let Some((plan, samples)) = s.try_cut() {
                assert_eq!(samples.len(), plan.len);
                cut_plans.push(plan);
            }
        }
        let (tail_plan, tail) = s.finish();
        cut_plans.push(tail_plan);

        // Three chunks total, first not deduped, the rest overlapped+deduped.
        assert_eq!(cut_plans.len(), 3);
        assert!(!cut_plans[0].needs_dedup);
        assert!(cut_plans[1].needs_dedup && cut_plans[2].needs_dedup);

        // Second chunk starts one overlap before the first chunk's end.
        let overlap = (OVERLAP_SECS * SAMPLE_RATE as f32) as usize;
        assert_eq!(cut_plans[1].start, cut_plans[0].start + cut_plans[0].len - overlap);
        // Every chunk ≤35s (bounds per-chunk memory / keeps single-pass).
        for p in &cut_plans {
            assert!(p.len <= secs(35.0) + 1);
        }
        // Tail is the final ≤35s remainder.
        assert!(tail.len() <= secs(35.0) + 1);
    }

    #[test]
    fn rolling_session_clean_silence_cut_has_no_overlap() {
        // 40s with a silence gap at 30s → one clean cut there, 10s tail.
        let mut samples = vec![0.3f32; secs(40.0)];
        for x in &mut samples[secs(29.9)..secs(30.1)] {
            *x = 0.0;
        }
        let mut s = RollingSession::new();
        s.push(&samples);
        let (plan0, c0) = s.try_cut().expect("should cut at the silence");
        assert!(!plan0.needs_dedup);
        assert!(s.try_cut().is_none());
        let (plan1, tail) = s.finish();
        // Second chunk starts exactly where the first ends (no overlap).
        assert_eq!(plan1.start, plan0.start + plan0.len);
        assert!(!plan1.needs_dedup);
        assert_eq!(c0.len() + tail.len(), samples.len());
        let cut_secs = plan0.len as f32 / SAMPLE_RATE as f32;
        assert!((cut_secs - 30.0).abs() < 0.3, "cut at {cut_secs}s");
    }
}
