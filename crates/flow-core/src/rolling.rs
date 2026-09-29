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
use std::time::{Duration, Instant};

use crate::chunking::{self, ChunkPlan, CutKind, OVERLAP_SECS, SAMPLE_RATE, SINGLE_PASS_MAX_SECS};
use crate::engine::Transcript;
use crate::model_manager::ModelCommand;
use crate::recovery::AudioStats;

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
    /// Every chunk plus the tail is transcribed and stitched — or the
    /// post-release watchdog gave up waiting and this is what finished
    /// (`partial`). `raw_text` is the assembled raw transcript; the caller runs
    /// the normal pipeline (dictionary → cleanup → paste) on it.
    Final {
        raw_text: String,
        audio_duration: Duration,
        /// Chunks the recording was cut into (tail included).
        chunks: usize,
        /// Chunks whose words are missing from `raw_text`: errored twice, came
        /// back empty despite speech-level audio (after a re-split), or were
        /// still outstanding when the watchdog fired.
        failed_chunks: usize,
        /// The watchdog fired before every chunk finished. If the stragglers
        /// complete later the full transcript follows as [`RollingOutput::Late`].
        partial: bool,
        /// Signal statistics over the whole recording.
        stats: AudioStats,
        /// Every sample of the recording (16 kHz mono), so the caller can save
        /// it for recovery when the transcript is incomplete.
        audio: Vec<f32>,
    },
    /// Sent only after a `partial` Final: the chunks the watchdog abandoned
    /// finished after all, and this is the complete transcript. Never pasted
    /// (the user has moved on) — the caller puts it on the clipboard.
    Late { raw_text: String, failed_chunks: usize },
}

/// Test-only fault injection for the rolling worker, read from the
/// environment by [`RollingConfig::from_env`]. All `None` in production.
#[derive(Debug, Clone, Default)]
pub struct FaultInjection {
    /// `VZT_FLOW_TEST_FAIL_CHUNK=<n>`: every transcription attempt of chunk
    /// `n` (1-based, in cut order; the tail is the last chunk) fails.
    pub fail_chunk: Option<usize>,
    /// `VZT_FLOW_TEST_STALL_CHUNK=<n>`: chunk `n`'s first result is withheld
    /// for `stall` (`VZT_FLOW_TEST_STALL_SECS`, default 30) — a wedged engine.
    pub stall_chunk: Option<usize>,
    pub stall: Duration,
}

/// Post-release watchdog tuning for [`spawn_rolling_worker_with`].
#[derive(Debug, Clone)]
pub struct RollingConfig {
    /// After release, give up waiting once this long passes with no chunk
    /// result arriving. Progress-based, not scaled to the recording: every
    /// result restarts it, so a long backlog that is still moving is never
    /// cut off, while a wedged engine is detected in bounded time.
    pub no_progress: Duration,
    /// The window also stretches to this many seconds per second of audio in
    /// the longest outstanding chunk, so one legitimately slow chunk is not
    /// mistaken for a wedge. Measured on the dev M5 while parallel builds held
    /// the load average at 20–60: 33s chunks took 77.7s (RTF 2.34) and up to
    /// 180s (RTF 5.2) — a flat 60s window would have abandoned a healthy
    /// engine. 6.0 gives a 35s chunk 210s. Beyond that the take is delivered
    /// as `partial` and the rest follows as `Late`, so a misjudged wedge
    /// costs a second step, never words.
    pub no_progress_per_audio_sec: f32,
    /// Added once, while no chunk of this recording has completed yet — the
    /// first chunk may be paying a (lazy, idle-unloaded) model load.
    pub load_allowance: Duration,
    /// After a partial Final, how long to keep waiting for the stragglers so
    /// they can be delivered as [`RollingOutput::Late`].
    pub late_wait_max: Duration,
    pub fault: FaultInjection,
}

impl Default for RollingConfig {
    fn default() -> Self {
        Self {
            no_progress: Duration::from_secs(60),
            no_progress_per_audio_sec: 6.0,
            load_allowance: Duration::from_secs(60),
            late_wait_max: Duration::from_secs(600),
            fault: FaultInjection::default(),
        }
    }
}

impl RollingConfig {
    /// Production defaults plus the test-only `VZT_FLOW_TEST_*` knobs:
    /// `VZT_FLOW_TEST_WATCHDOG_SECS=<s>` sets `no_progress` and drops the load
    /// allowance; see [`FaultInjection`] for the others.
    pub fn from_env() -> Self {
        fn num(name: &str) -> Option<u64> {
            std::env::var(name).ok().and_then(|v| v.trim().parse().ok())
        }
        let mut cfg = Self::default();
        if let Some(s) = num("VZT_FLOW_TEST_WATCHDOG_SECS") {
            cfg.no_progress = Duration::from_secs(s);
            cfg.no_progress_per_audio_sec = 0.0;
            cfg.load_allowance = Duration::ZERO;
        }
        cfg.fault.fail_chunk = num("VZT_FLOW_TEST_FAIL_CHUNK").map(|n| n as usize);
        cfg.fault.stall_chunk = num("VZT_FLOW_TEST_STALL_CHUNK").map(|n| n as usize);
        cfg.fault.stall = Duration::from_secs(num("VZT_FLOW_TEST_STALL_SECS").unwrap_or(30));
        if cfg.fault.fail_chunk.is_some() || cfg.fault.stall_chunk.is_some() || num("VZT_FLOW_TEST_WATCHDOG_SECS").is_some() {
            eprintln!("[vzt-flow] TEST fault injection active for rolling transcription: {cfg:?}");
        }
        cfg
    }
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
    spawn_rolling_worker_with(RollingConfig::from_env(), model_cmd_tx, output_tx)
}

/// [`spawn_rolling_worker`] with explicit watchdog/fault configuration.
pub fn spawn_rolling_worker_with(
    cfg: RollingConfig,
    model_cmd_tx: Sender<ModelCommand>,
    output_tx: Sender<RollingOutput>,
) -> Sender<RollingInput> {
    let (in_tx, in_rx) = mpsc::channel::<RollingInput>();

    thread::Builder::new()
        .name("vzt-flow-rolling".into())
        .spawn(move || {
            let (res_tx, res_rx) = mpsc::channel::<ChunkResult>();
            let mut w = Worker::new(cfg, model_cmd_tx, res_tx);
            loop {
                match in_rx.recv() {
                    Ok(RollingInput::Samples(s)) => {
                        w.push(&s);
                        // Non-blockingly collect any completed chunks and
                        // preview them.
                        while let Ok(r) = res_rx.try_recv() {
                            if let Some(text) = w.on_result(r) {
                                let _ = output_tx.send(RollingOutput::Preview { chunk_text: text });
                            }
                        }
                    }
                    Ok(RollingInput::Finish) => {
                        // A panic while finishing must still hand the audio
                        // back: without it the take is gone for good.
                        let finished = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            w.finish(&res_rx, &output_tx)
                        }));
                        if finished.is_err() {
                            eprintln!(
                                "[vzt-flow] rolling worker panicked while finishing; returning the \
                                 audio with no transcript so it can be recovered"
                            );
                            let audio = std::mem::take(&mut w.audio);
                            let _ = output_tx.send(RollingOutput::Final {
                                raw_text: String::new(),
                                audio_duration: Duration::from_secs_f64(audio.len() as f64 / SAMPLE_RATE as f64),
                                chunks: w.plans.len(),
                                failed_chunks: w.plans.len(),
                                partial: true,
                                stats: AudioStats::from_samples(&audio, SAMPLE_RATE),
                                audio,
                            });
                        }
                        break;
                    }
                    Err(_) => break, // input dropped: recording abandoned (cancel).
                }
            }
        })
        .expect("failed to spawn rolling worker thread");

    in_tx
}

/// Shortest chunk worth re-splitting when it comes back empty.
const RESPLIT_MIN_SECS: f32 = 8.0;

/// A release tail shorter than this (0.25s) is not transcribed.
const MIN_TAIL_SAMPLES: usize = SAMPLE_RATE as usize / 4;

/// Which part of a chunk a transcription result is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    Whole,
    FirstHalf,
    SecondHalf,
}

/// `(chunk index, part, result)` from a dispatch forwarder.
type ChunkResult = (usize, Part, Result<Transcript, String>);

/// One chunk's progress through transcription.
enum Slot {
    /// The whole chunk is in flight; `attempt` is 0, or 1 for the retry.
    Whole { attempt: u8 },
    /// The whole chunk came back empty despite speech-level audio, so it is
    /// being transcribed as two halves split at `split`. Each half is `None`
    /// while in flight, `Some(Ok(text))` / `Some(Err(()))` once answered.
    Halves { split: usize, first: Option<Result<String, ()>>, second: Option<Result<String, ()>> },
    /// Finished. `failed` means words from this chunk are missing.
    Done { text: String, failed: bool },
}

struct Worker {
    cfg: RollingConfig,
    model_cmd_tx: Sender<ModelCommand>,
    res_tx: Sender<ChunkResult>,
    session: RollingSession,
    /// Every sample pushed, kept for retries, re-splits and recovery (≤38MB
    /// at the 600s cap).
    audio: Vec<f32>,
    plans: Vec<ChunkPlan>,
    slots: Vec<Slot>,
    /// Any result (success or error) has arrived, i.e. the model is loaded.
    any_answer: bool,
}

impl Worker {
    fn new(cfg: RollingConfig, model_cmd_tx: Sender<ModelCommand>, res_tx: Sender<ChunkResult>) -> Self {
        Self {
            cfg,
            model_cmd_tx,
            res_tx,
            session: RollingSession::new(),
            audio: Vec::new(),
            plans: Vec::new(),
            slots: Vec::new(),
            any_answer: false,
        }
    }

    fn push(&mut self, samples: &[f32]) {
        self.audio.extend_from_slice(samples);
        self.session.push(samples);
        while let Some((plan, _chunk)) = self.session.try_cut() {
            self.add_chunk(plan);
        }
    }

    fn add_chunk(&mut self, plan: ChunkPlan) {
        let idx = self.plans.len();
        self.plans.push(plan);
        self.slots.push(Slot::Whole { attempt: 0 });
        self.dispatch(idx, Part::Whole, 0);
    }

    /// The samples of chunk `idx`, or of one of its halves.
    fn span(&self, idx: usize, part: Part) -> &[f32] {
        let p = self.plans[idx];
        let chunk = &self.audio[p.start..p.start + p.len];
        match (part, &self.slots[idx]) {
            (Part::FirstHalf, Slot::Halves { split, .. }) => &chunk[..*split],
            (Part::SecondHalf, Slot::Halves { split, .. }) => &chunk[*split..],
            _ => chunk,
        }
    }

    fn dispatch(&self, idx: usize, part: Part, attempt: u8) {
        let chunk_no = idx + 1;
        if self.cfg.fault.fail_chunk == Some(chunk_no) {
            let _ = self.res_tx.send((
                idx,
                part,
                Err("injected failure (VZT_FLOW_TEST_FAIL_CHUNK)".to_string()),
            ));
            return;
        }
        let stall = (self.cfg.fault.stall_chunk == Some(chunk_no) && part == Part::Whole && attempt == 0)
            .then_some(self.cfg.fault.stall);
        let samples = self.span(idx, part).to_vec();
        let (rtx, rrx) = mpsc::channel();
        if self
            .model_cmd_tx
            .send(ModelCommand::TranscribeChunk { samples, reply: rtx })
            .is_ok()
        {
            let res_tx = self.res_tx.clone();
            thread::spawn(move || {
                let r = rrx
                    .recv()
                    .unwrap_or_else(|_| Err("chunk transcriber dropped".to_string()));
                if let Some(d) = stall {
                    eprintln!("[vzt-flow] TEST: withholding chunk {chunk_no}'s result for {d:?}");
                    thread::sleep(d);
                }
                let _ = res_tx.send((idx, part, r));
            });
        } else {
            // Model manager gone: answer immediately so nothing waits on a
            // chunk that will never complete.
            let _ = self.res_tx.send((idx, part, Err("transcriber unavailable".to_string())));
        }
    }

    /// Applies one chunk result. Returns the chunk's text when it just
    /// finished with words (for the live preview).
    fn on_result(&mut self, (idx, part, r): ChunkResult) -> Option<String> {
        self.any_answer = true;
        let chunk_no = idx + 1;
        match (part, self.slots.get(idx)?) {
            (Part::Whole, Slot::Whole { attempt }) => {
                let attempt = *attempt;
                match r {
                    Ok(t) if !t.text.trim().is_empty() => {
                        let text = t.text.trim().to_string();
                        self.slots[idx] = Slot::Done { text: text.clone(), failed: false };
                        Some(text)
                    }
                    Ok(_) => {
                        let chunk = self.span(idx, Part::Whole);
                        let long_enough = chunk.len() as f32 >= RESPLIT_MIN_SECS * SAMPLE_RATE as f32;
                        if long_enough && AudioStats::from_samples(chunk, SAMPLE_RATE).has_speech() {
                            let split = chunking::quietest_split(chunk, SAMPLE_RATE);
                            eprintln!(
                                "[vzt-flow] rolling chunk {chunk_no} ({:.1}s) came back empty despite \
                                 speech-level audio; re-transcribing it as two halves split at {:.1}s",
                                chunk.len() as f32 / SAMPLE_RATE as f32,
                                split as f32 / SAMPLE_RATE as f32
                            );
                            self.slots[idx] = Slot::Halves { split, first: None, second: None };
                            self.dispatch(idx, Part::FirstHalf, 0);
                            self.dispatch(idx, Part::SecondHalf, 0);
                        } else {
                            self.slots[idx] = Slot::Done { text: String::new(), failed: false };
                        }
                        None
                    }
                    Err(e) if attempt == 0 => {
                        eprintln!("[vzt-flow] rolling chunk {chunk_no} failed ({e}); retrying once");
                        self.slots[idx] = Slot::Whole { attempt: 1 };
                        self.dispatch(idx, Part::Whole, 1);
                        None
                    }
                    Err(e) => {
                        eprintln!(
                            "[vzt-flow] rolling chunk {chunk_no} failed again ({e}); its audio is \
                             missing from the transcript"
                        );
                        self.slots[idx] = Slot::Done { text: String::new(), failed: true };
                        None
                    }
                }
            }
            (Part::FirstHalf | Part::SecondHalf, Slot::Halves { .. }) => {
                let answer = match r {
                    Ok(t) => Ok(t.text.trim().to_string()),
                    Err(e) => {
                        eprintln!("[vzt-flow] rolling chunk {chunk_no} half failed ({e})");
                        Err(())
                    }
                };
                let Slot::Halves { first, second, .. } = &mut self.slots[idx] else { unreachable!() };
                if part == Part::FirstHalf {
                    *first = Some(answer);
                } else {
                    *second = Some(answer);
                }
                let (Some(a), Some(b)) = (first.clone(), second.clone()) else { return None };
                let text = [a.clone().unwrap_or_default(), b.clone().unwrap_or_default()]
                    .into_iter()
                    .filter(|t| !t.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ");
                let failed = a.is_err() || b.is_err() || text.is_empty();
                if failed {
                    eprintln!(
                        "[vzt-flow] rolling chunk {chunk_no} is still incomplete after the re-split; \
                         its audio is kept for recovery"
                    );
                }
                self.slots[idx] = Slot::Done { text: text.clone(), failed };
                (!text.is_empty()).then_some(text)
            }
            // A duplicate or stale answer (e.g. a withheld result arriving
            // after the chunk was already settled) — nothing to update.
            _ => None,
        }
    }

    fn outstanding(&self) -> usize {
        self.slots.iter().filter(|s| !matches!(s, Slot::Done { .. })).count()
    }

    /// The no-progress window while waiting on the current outstanding set:
    /// long enough for the slowest one chunk to finish on a loaded machine,
    /// plus the one-time load allowance until the engine has answered at all.
    fn window(&self) -> Duration {
        let longest = self
            .plans
            .iter()
            .zip(&self.slots)
            .filter(|(_, s)| !matches!(s, Slot::Done { .. }))
            .map(|(p, _)| p.len as f32 / SAMPLE_RATE as f32)
            .fold(0.0f32, f32::max);
        let scaled = Duration::from_secs_f32(longest * self.cfg.no_progress_per_audio_sec);
        let base = self.cfg.no_progress.max(scaled);
        if self.any_answer {
            base
        } else {
            base + self.cfg.load_allowance
        }
    }

    /// Stitches every settled chunk; unsettled ones count as failed gaps.
    fn assemble(&self) -> (String, usize) {
        let mut failed = 0;
        let transcripts: Vec<Transcript> = self
            .slots
            .iter()
            .map(|s| {
                let text = match s {
                    Slot::Done { text, failed: f } => {
                        failed += *f as usize;
                        text.clone()
                    }
                    _ => {
                        failed += 1;
                        String::new()
                    }
                };
                Transcript { text, segments: None }
            })
            .collect();
        (chunking::assemble(&self.plans, &transcripts, SAMPLE_RATE).text, failed)
    }

    /// Release: transcribe the tail, wait (progress-bounded) for everything
    /// outstanding, deliver, then — if the watchdog fired — keep waiting for
    /// the stragglers and deliver them as `Late`.
    fn finish(&mut self, res_rx: &mpsc::Receiver<ChunkResult>, out: &Sender<RollingOutput>) {
        let (tail_plan, _tail) = self.session.finish();
        if tail_plan.len < MIN_TAIL_SAMPLES {
            // Released right after a cut: a few milliseconds hold no word, and
            // sending them to the engine only risks an error that would be
            // reported as a lost chunk.
            self.plans.push(tail_plan);
            self.slots.push(Slot::Done { text: String::new(), failed: false });
        } else {
            self.add_chunk(tail_plan);
        }

        let mut last_progress = Instant::now();
        let mut timed_out = false;
        while self.outstanding() > 0 {
            let window = self.window();
            let deadline = last_progress + window;
            match res_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(r) => {
                    self.on_result(r);
                    last_progress = Instant::now();
                }
                Err(_) => {
                    eprintln!(
                        "[vzt-flow] rolling: no chunk answered for {:.0}s after release; delivering \
                         the {} of {} chunks that finished (partial) and keeping the audio",
                        window.as_secs_f32(),
                        self.plans.len() - self.outstanding(),
                        self.plans.len()
                    );
                    timed_out = true;
                    break;
                }
            }
        }

        let (raw_text, failed_chunks) = self.assemble();
        let audio_duration = Duration::from_secs_f64(self.audio.len() as f64 / SAMPLE_RATE as f64);
        let stats = AudioStats::from_samples(&self.audio, SAMPLE_RATE);
        // Keep our copy while stragglers may still need re-splitting.
        let audio = if timed_out { self.audio.clone() } else { std::mem::take(&mut self.audio) };
        let _ = out.send(RollingOutput::Final {
            raw_text,
            audio_duration,
            chunks: self.plans.len(),
            failed_chunks,
            partial: timed_out,
            stats,
            audio,
        });
        if !timed_out {
            return;
        }

        let give_up = Instant::now() + self.cfg.late_wait_max;
        while self.outstanding() > 0 {
            match res_rx.recv_timeout(give_up.saturating_duration_since(Instant::now())) {
                Ok(r) => {
                    self.on_result(r);
                }
                Err(_) => {
                    eprintln!(
                        "[vzt-flow] rolling: {} chunk(s) never finished; the saved recording is the \
                         only copy of those words",
                        self.outstanding()
                    );
                    return;
                }
            }
        }
        let (raw_text, failed_chunks) = self.assemble();
        eprintln!(
            "[vzt-flow] rolling: the chunks abandoned at the watchdog finished late; delivering the \
             complete transcript ({} chars)",
            raw_text.chars().count()
        );
        let _ = out.send(RollingOutput::Late { raw_text, failed_chunks });
    }
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

    // ---- worker: never discard a finished take (B2/B3) ----
    //
    // These drive the real worker against a scripted stand-in for the model
    // manager, so every failure mode of a live engine (an error, an empty
    // result, a wedge, a late answer) is reproducible on demand.

    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    /// What the fake engine does with one `TranscribeChunk`.
    enum Answer {
        Text(&'static str),
        Fail,
        /// Hold the reply open and never answer (a wedged inference).
        Never,
        /// Answer, but only after this delay.
        After(Duration, &'static str),
    }

    /// Speech-level (RMS ≈ 0.07) deterministic noise, distinct per sample
    /// index so chunks of equal length are still distinguishable.
    fn speech(n_secs: f32, seed: u32) -> Vec<f32> {
        let mut x = seed.wrapping_mul(2_654_435_761).wrapping_add(1);
        (0..secs(n_secs))
            .map(|_| {
                x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                0.12 * ((x >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0)
            })
            .collect()
    }

    /// 40s of speech with a pause at 30s: exactly one silence cut (chunk 1 ≈
    /// 30s) plus a ~10s tail (chunk 2).
    fn forty_seconds_one_cut() -> Vec<f32> {
        let mut s = speech(40.0, 7);
        for x in &mut s[secs(29.9)..secs(30.1)] {
            *x = 0.0;
        }
        s
    }

    /// Fake model manager. `script(call_index, samples)` decides each answer;
    /// every call's sample count is recorded.
    fn fake_engine(
        script: impl Fn(usize, &[f32]) -> Answer + Send + 'static,
    ) -> (Sender<ModelCommand>, Arc<Mutex<Vec<usize>>>) {
        let (tx, rx) = mpsc::channel::<ModelCommand>();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let calls2 = calls.clone();
        thread::spawn(move || {
            let mut parked = Vec::new(); // replies held open for `Never`
            let mut n = 0usize;
            while let Ok(cmd) = rx.recv() {
                let ModelCommand::TranscribeChunk { samples, reply } = cmd else {
                    panic!("the rolling worker must only send bounded chunks");
                };
                calls2.lock().unwrap().push(samples.len());
                let ok = |t: &str| Ok(Transcript { text: t.to_string(), segments: None });
                match script(n, &samples) {
                    Answer::Text(t) => {
                        let _ = reply.send(ok(t));
                    }
                    Answer::Fail => {
                        let _ = reply.send(Err("engine error".to_string()));
                    }
                    Answer::Never => parked.push(reply),
                    Answer::After(d, t) => {
                        // Inline, like the real manager: one inference at a
                        // time, later requests queue behind this one.
                        thread::sleep(d);
                        let _ = reply.send(ok(t));
                    }
                }
                n += 1;
            }
        });
        (tx, calls)
    }

    fn quick_cfg() -> RollingConfig {
        RollingConfig {
            no_progress: Duration::from_millis(600),
            no_progress_per_audio_sec: 0.0,
            load_allowance: Duration::ZERO,
            late_wait_max: Duration::from_secs(10),
            fault: FaultInjection::default(),
        }
    }

    /// Push `audio`, then keep trickling a little silence until the worker has
    /// collected (and previewed) `previews` chunk results, so the test controls
    /// exactly which chunks finished *before* release.
    fn record(
        rin: &Sender<RollingInput>,
        out: &mpsc::Receiver<RollingOutput>,
        audio: &[f32],
        previews: usize,
    ) {
        rin.send(RollingInput::Samples(audio.to_vec())).unwrap();
        let mut seen = 0;
        let deadline = Instant::now() + Duration::from_secs(10);
        while seen < previews {
            assert!(Instant::now() < deadline, "chunk previews never arrived");
            rin.send(RollingInput::Samples(vec![0.0; 16])).unwrap();
            while let Ok(o) = out.try_recv() {
                if matches!(o, RollingOutput::Preview { .. }) {
                    seen += 1;
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    struct FinalMsg {
        raw_text: String,
        chunks: usize,
        failed_chunks: usize,
        partial: bool,
        audio_len: usize,
    }

    fn wait_final(out: &mpsc::Receiver<RollingOutput>, within: Duration) -> FinalMsg {
        let deadline = Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match out.recv_timeout(left) {
                Ok(RollingOutput::Final { raw_text, chunks, failed_chunks, partial, audio, .. }) => {
                    return FinalMsg { raw_text, chunks, failed_chunks, partial, audio_len: audio.len() }
                }
                Ok(_) => continue,
                Err(e) => panic!("no Final within {within:?} ({e}) — the take was stranded"),
            }
        }
    }

    #[test]
    fn a_failed_chunk_is_retried_once_before_it_becomes_a_gap() {
        // Chunk 1 (~30s) errors on its first attempt only; the tail is fine.
        let attempts = Arc::new(Mutex::new(0usize));
        let a2 = attempts.clone();
        let (engine, calls) = fake_engine(move |_, s| {
            if s.len() > secs(20.0) {
                let mut a = a2.lock().unwrap();
                *a += 1;
                if *a == 1 { Answer::Fail } else { Answer::Text("alpha") }
            } else {
                Answer::Text("omega")
            }
        });
        let (out_tx, out) = mpsc::channel();
        let rin = spawn_rolling_worker_with(quick_cfg(), engine, out_tx);
        rin.send(RollingInput::Samples(forty_seconds_one_cut())).unwrap();
        rin.send(RollingInput::Finish).unwrap();
        let f = wait_final(&out, Duration::from_secs(10));
        assert_eq!(f.raw_text, "alpha omega", "the first chunk's words were dropped");
        assert_eq!(f.failed_chunks, 0);
        assert_eq!(calls.lock().unwrap().len(), 3, "one retry, no more");
    }

    #[test]
    fn a_chunk_that_fails_twice_is_counted_and_the_rest_still_delivered() {
        let (engine, _) = fake_engine(|_, s| if s.len() > secs(20.0) { Answer::Fail } else { Answer::Text("omega") });
        let (out_tx, out) = mpsc::channel();
        let rin = spawn_rolling_worker_with(quick_cfg(), engine, out_tx);
        let audio = forty_seconds_one_cut();
        rin.send(RollingInput::Samples(audio.clone())).unwrap();
        rin.send(RollingInput::Finish).unwrap();
        let f = wait_final(&out, Duration::from_secs(10));
        assert_eq!(f.raw_text, "omega");
        assert_eq!((f.chunks, f.failed_chunks), (2, 1), "the lost chunk must be reported");
        assert!(!f.partial);
        assert_eq!(f.audio_len, audio.len(), "the whole take must come back for recovery");
    }

    #[test]
    fn an_empty_result_for_a_long_speech_chunk_is_resplit_and_recovered() {
        // A 12s single-chunk dictation of speech-level audio for which the
        // engine returns nothing (the 11–31s empty entries in history), but
        // which it can transcribe in halves.
        let (engine, calls) = fake_engine(|_, s| {
            if s.len() >= secs(11.0) {
                Answer::Text("")
            } else if s[0] == speech(12.0, 3)[0] {
                Answer::Text("first half")
            } else {
                Answer::Text("second half")
            }
        });
        let (out_tx, out) = mpsc::channel();
        let rin = spawn_rolling_worker_with(quick_cfg(), engine, out_tx);
        rin.send(RollingInput::Samples(speech(12.0, 3))).unwrap();
        rin.send(RollingInput::Finish).unwrap();
        let f = wait_final(&out, Duration::from_secs(10));
        assert_eq!(f.raw_text, "first half second half");
        assert_eq!(f.failed_chunks, 0);
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 3, "whole + two halves: {calls:?}");
        assert!(calls[1] >= secs(3.0) && calls[2] >= secs(3.0), "halves must be real halves: {calls:?}");
        assert_eq!(calls[1] + calls[2], calls[0], "the halves must cover the chunk exactly");
    }

    #[test]
    fn an_empty_result_for_silence_is_accepted_without_a_resplit() {
        let (engine, calls) = fake_engine(|_, _| Answer::Text(""));
        let (out_tx, out) = mpsc::channel();
        let rin = spawn_rolling_worker_with(quick_cfg(), engine, out_tx);
        rin.send(RollingInput::Samples(vec![0.0; secs(12.0)])).unwrap();
        rin.send(RollingInput::Finish).unwrap();
        let f = wait_final(&out, Duration::from_secs(10));
        assert_eq!(f.raw_text, "");
        assert_eq!(f.failed_chunks, 0, "silence transcribing to nothing is not a failure");
        assert_eq!(calls.lock().unwrap().len(), 1, "no re-split for a silent chunk");
    }

    #[test]
    fn a_wedged_chunk_trips_the_progress_watchdog_and_salvages_the_rest() {
        // Chunk 1 finishes during recording; the tail never comes back. The
        // old worker blocked forever here (and the coordinator discarded the
        // take at `duration + 60s`).
        let (engine, _) = fake_engine(|_, s| if s.len() > secs(20.0) { Answer::Text("alpha") } else { Answer::Never });
        let (out_tx, out) = mpsc::channel();
        let rin = spawn_rolling_worker_with(quick_cfg(), engine, out_tx);
        let audio = forty_seconds_one_cut();
        record(&rin, &out, &audio, 1);
        let released = Instant::now();
        rin.send(RollingInput::Finish).unwrap();
        let f = wait_final(&out, Duration::from_secs(5));
        let waited = released.elapsed();
        assert!(f.partial, "must be flagged partial");
        assert_eq!(f.raw_text, "alpha", "the finished chunk must be delivered");
        assert_eq!(f.failed_chunks, 1);
        assert!(f.audio_len >= audio.len(), "the whole take must come back for recovery");
        assert!(waited >= Duration::from_millis(500), "fired early: {waited:?}");
    }

    #[test]
    fn a_result_that_lands_after_the_watchdog_is_delivered_late_not_dropped() {
        let (engine, _) = fake_engine(|_, s| {
            if s.len() > secs(20.0) { Answer::Text("alpha") } else { Answer::After(Duration::from_millis(1500), "omega") }
        });
        let (out_tx, out) = mpsc::channel();
        let rin = spawn_rolling_worker_with(quick_cfg(), engine, out_tx);
        record(&rin, &out, &forty_seconds_one_cut(), 1);
        rin.send(RollingInput::Finish).unwrap();
        let f = wait_final(&out, Duration::from_secs(5));
        assert!(f.partial);
        assert_eq!(f.raw_text, "alpha");
        match out.recv_timeout(Duration::from_secs(10)) {
            Ok(RollingOutput::Late { raw_text, failed_chunks }) => {
                assert_eq!(raw_text, "alpha omega");
                assert_eq!(failed_chunks, 0);
            }
            Ok(_) => panic!("expected Late"),
            Err(e) => panic!("the late result was dropped ({e})"),
        }
    }

    #[test]
    fn progress_restarts_the_watchdog_so_a_slow_but_moving_backlog_is_not_cut_off() {
        // Three queued chunks at release, each taking 400ms — longer in total
        // (1.2s) than the 600ms no-progress window, but never 600ms silent.
        let (engine, _) = fake_engine(|_, _| Answer::After(Duration::from_millis(400), "x"));
        let (out_tx, out) = mpsc::channel();
        let rin = spawn_rolling_worker_with(quick_cfg(), engine, out_tx);
        rin.send(RollingInput::Samples(speech(80.0, 11))).unwrap();
        rin.send(RollingInput::Finish).unwrap();
        let f = wait_final(&out, Duration::from_secs(10));
        assert!(!f.partial, "a moving backlog must not be treated as a wedge");
        assert_eq!(f.failed_chunks, 0);
        assert!(f.chunks >= 3);
    }

    #[test]
    fn a_sliver_of_tail_after_a_cut_is_not_reported_as_lost_audio() {
        // 35.05s of speech with a pause in the last frame of the cut window
        // (34.9–35.0s): the cut lands at 34.95s, leaving a 0.1s tail. The
        // engine errors on it. That must not turn into "some audio couldn't be
        // transcribed".
        let (engine, calls) = fake_engine(|_, s| if s.len() < secs(1.0) { Answer::Fail } else { Answer::Text("alpha") });
        let (out_tx, out) = mpsc::channel();
        let rin = spawn_rolling_worker_with(quick_cfg(), engine, out_tx);
        let mut audio = speech(35.05, 5);
        for x in &mut audio[secs(34.9)..secs(35.0)] {
            *x = 0.0;
        }
        rin.send(RollingInput::Samples(audio)).unwrap();
        let f = wait_final_after_finish(&rin, &out);
        assert_eq!(f.failed_chunks, 0, "{:?}", calls.lock().unwrap());
        assert!(f.raw_text.starts_with("alpha"));
    }

    fn wait_final_after_finish(rin: &Sender<RollingInput>, out: &mpsc::Receiver<RollingOutput>) -> FinalMsg {
        rin.send(RollingInput::Finish).unwrap();
        wait_final(out, Duration::from_secs(10))
    }

    #[test]
    fn fault_injection_fails_the_named_chunk_every_attempt() {
        let (engine, calls) = fake_engine(|_, _| Answer::Text("ok"));
        let mut cfg = quick_cfg();
        cfg.fault.fail_chunk = Some(1);
        let (out_tx, out) = mpsc::channel();
        let rin = spawn_rolling_worker_with(cfg, engine, out_tx);
        rin.send(RollingInput::Samples(forty_seconds_one_cut())).unwrap();
        rin.send(RollingInput::Finish).unwrap();
        let f = wait_final(&out, Duration::from_secs(10));
        assert_eq!((f.raw_text.as_str(), f.failed_chunks), ("ok", 1));
        assert_eq!(calls.lock().unwrap().len(), 1, "an injected failure never reaches the engine");
    }
}
