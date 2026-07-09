//! Microphone capture + resampling to the 16 kHz mono f32 PCM that
//! transcribe-rs engines expect.

use std::io::{self, BufRead};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig};

pub const TARGET_SAMPLE_RATE: u32 = 16_000;

/// Info about the default input device, used by `flow doctor`.
pub struct InputDeviceInfo {
    pub name: String,
    pub sample_rate: u32,
    pub channels: u16,
}

pub fn default_input_device_info() -> Result<InputDeviceInfo> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .context("no default input (microphone) device found")?;
    let name = device.name().unwrap_or_else(|_| "unknown".to_string());
    let config = device
        .default_input_config()
        .context("failed to read default input config")?;
    Ok(InputDeviceInfo {
        name,
        sample_rate: config.sample_rate().0,
        channels: config.channels(),
    })
}

pub struct AudioRecorder;

impl AudioRecorder {
    /// Record from the default input device until Enter is pressed on
    /// stdin, or `max_seconds` elapses (whichever comes first). Returns
    /// mono f32 samples resampled to 16 kHz, plus the recorded duration.
    pub fn record_until_enter(max_seconds: Option<u64>) -> Result<(Vec<f32>, Duration)> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .context("no default input (microphone) device found")?;
        let config = device
            .default_input_config()
            .context("failed to read default input config")?;

        let sample_format = config.sample_format();
        let stream_config: StreamConfig = config.into();
        let in_rate = stream_config.sample_rate.0;
        let in_channels = stream_config.channels as usize;

        let (tx, rx) = mpsc::channel::<Vec<f32>>();
        let err_fn = |err| eprintln!("audio input stream error: {err}");

        let stream = match sample_format {
            SampleFormat::F32 => device.build_input_stream(
                &stream_config,
                move |data: &[f32], _| {
                    let _ = tx.send(data.to_vec());
                },
                err_fn,
                None,
            ),
            SampleFormat::I16 => device.build_input_stream(
                &stream_config,
                move |data: &[i16], _| {
                    let converted: Vec<f32> = data.iter().map(|&s| s as f32 / i16::MAX as f32).collect();
                    let _ = tx.send(converted);
                },
                err_fn,
                None,
            ),
            SampleFormat::U16 => device.build_input_stream(
                &stream_config,
                move |data: &[u16], _| {
                    let converted: Vec<f32> = data
                        .iter()
                        .map(|&s| (s as f32 - u16::MAX as f32 / 2.0) / (u16::MAX as f32 / 2.0))
                        .collect();
                    let _ = tx.send(converted);
                },
                err_fn,
                None,
            ),
            other => anyhow::bail!("unsupported sample format: {other:?}"),
        }
        .context("failed to build input stream")?;

        stream.play().context("failed to start input stream")?;

        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();

        // Wait for Enter on stdin (spawned so we can also honor a timeout),
        // unless max_seconds is set with no interactive wait desired.
        let stdin_thread = std::thread::spawn(move || {
            let mut line = String::new();
            let _ = io::stdin().lock().read_line(&mut line);
            stop_clone.store(true, Ordering::SeqCst);
        });

        eprintln!("Recording... press Enter to stop{}.",
            max_seconds.map(|s| format!(" (auto-stops after {s}s)")).unwrap_or_default());

        let started = Instant::now();
        let mut raw_samples: Vec<f32> = Vec::new();
        loop {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            if let Some(max) = max_seconds {
                if started.elapsed().as_secs() >= max {
                    break;
                }
            }
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(chunk) => raw_samples.extend(chunk),
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        drop(stream);

        // Drain any samples still queued.
        while let Ok(chunk) = rx.try_recv() {
            raw_samples.extend(chunk);
        }

        // The stdin-reading thread will only exit once Enter is pressed;
        // if we stopped due to the timeout instead, detach it rather than
        // block the caller waiting for a keypress that may never come.
        if !stop.load(Ordering::SeqCst) {
            drop(stdin_thread);
        } else {
            let _ = stdin_thread.join();
        }

        let mono = downmix_to_mono(&raw_samples, in_channels);
        let resampled = resample_linear(&mono, in_rate, TARGET_SAMPLE_RATE);
        let duration = Duration::from_secs_f64(mono.len() as f64 / in_rate as f64);

        Ok((resampled, duration))
    }
}

/// Commands accepted by the audio worker thread spawned by
/// [`spawn_audio_worker`]. The worker owns the cpal `Stream` for the
/// lifetime of a recording (cpal streams are not `Send`, so they can never
/// leave the thread that created them) and is driven entirely by this
/// channel instead.
pub enum AudioCommand {
    /// Open the input stream and start accumulating samples. `max_secs` is a
    /// hard cap: once the recording reaches it the worker auto-stops and
    /// delivers what it captured (see [`AudioReply::Stopped`] `capped`).
    /// `handsfree_silence_secs`, when set, enables energy-based auto-stop:
    /// once at least one loud frame has been seen, this many seconds of
    /// continuous sub-threshold audio afterward auto-stops the recording
    /// (see [`AudioReply::Stopped`] `auto_stopped_silence`). `None` for
    /// hold-to-talk recordings, where releasing the key is the only stop
    /// signal.
    Start { max_secs: u64, handsfree_silence_secs: Option<f64> },
    /// Stop the current recording and reply with the resampled 16kHz mono
    /// audio plus its real-world duration.
    Stop,
    /// Stop the current recording and discard whatever was captured.
    Cancel,
}

/// Messages the audio worker sends back to the coordinator.
pub enum AudioReply {
    Started,
    /// A recording finished. `capped` is true when it ended because it hit
    /// the max-duration cap rather than a user Stop — the coordinator uses
    /// that to reset mode flags and mark the in-flight key press consumed.
    /// `auto_stopped_silence` is true when it ended because the hands-free
    /// VAD detected trailing silence; the coordinator treats it the same as
    /// `capped` (system-initiated stop) but logs a distinct message.
    Stopped {
        samples: Vec<f32>,
        duration: Duration,
        capped: bool,
        auto_stopped_silence: bool,
    },
    Cancelled,
    /// The input device faulted mid-recording (unplugged, format change,
    /// etc.). `samples` carries what was captured when it's long enough to be
    /// worth transcribing (>1s), otherwise it is empty and should be
    /// discarded. Either way the coordinator returns to Idle.
    Disconnected {
        samples: Vec<f32>,
        duration: Duration,
    },
    /// Roughly-15Hz input level updates (peak amplitude in `[0, 1]`) for
    /// driving the overlay's level bars while recording.
    Level(f32),
    Error(String),
}

const LEVEL_UPDATE_INTERVAL: Duration = Duration::from_millis(66); // ~15 Hz

/// Minimum captured duration worth transcribing after a mid-recording device
/// fault; shorter than this we discard as noise.
const MIN_SALVAGE_SECS: f64 = 1.0;

/// Number of raw (interleaved, pre-downmix) input samples that `max_secs` of
/// audio occupies, plus a generous margin. Used as a byte/length backstop so
/// the capture buffer can never grow without bound even if the wall-clock cap
/// check is somehow starved.
pub fn max_raw_samples(max_secs: u64, sample_rate: u32, channels: usize) -> usize {
    // +5s of headroom so the time-based cap is what normally fires first.
    (max_secs.saturating_add(5) as usize)
        .saturating_mul(sample_rate.max(1) as usize)
        .saturating_mul(channels.max(1))
}

/// Spawns a dedicated OS thread that waits for [`AudioCommand`]s and drives
/// microphone capture. Runs until `cmd_rx` disconnects.
pub fn spawn_audio_worker(
    cmd_rx: mpsc::Receiver<AudioCommand>,
    reply_tx: mpsc::Sender<AudioReply>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("vzt-flow-audio-worker".into())
        .spawn(move || {
            while let Ok(cmd) = cmd_rx.recv() {
                match cmd {
                    AudioCommand::Start { max_secs, handsfree_silence_secs } => {
                        if let Err(e) =
                            run_one_recording(&cmd_rx, &reply_tx, max_secs, handsfree_silence_secs)
                        {
                            let _ = reply_tx.send(AudioReply::Error(e.to_string()));
                        }
                    }
                    // Stop/Cancel with no recording in progress: nothing to do.
                    AudioCommand::Stop | AudioCommand::Cancel => {}
                }
            }
        })
        .expect("failed to spawn audio worker thread")
}

/// Opens the input stream, accumulates samples until a `Stop`/`Cancel`
/// command (or the sender disconnects), and replies accordingly. Lives
/// entirely on the audio worker thread so the non-`Send` cpal `Stream`
/// never has to cross a thread boundary.
fn run_one_recording(
    cmd_rx: &mpsc::Receiver<AudioCommand>,
    reply_tx: &mpsc::Sender<AudioReply>,
    max_secs: u64,
    handsfree_silence_secs: Option<f64>,
) -> Result<()> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .context("no default input (microphone) device found")?;
    let config = device
        .default_input_config()
        .context("failed to read default input config")?;

    let sample_format = config.sample_format();
    let stream_config: StreamConfig = config.into();
    let in_rate = stream_config.sample_rate.0;
    let in_channels = stream_config.channels as usize;

    let (data_tx, data_rx) = mpsc::channel::<Vec<f32>>();

    // Raised by cpal's error callback (runs on cpal's own thread) when the
    // input device faults mid-stream — device unplugged, sample-format change,
    // etc. The capture loop polls it and treats it as a clean fault stop.
    let device_lost = Arc::new(AtomicBool::new(false));
    let err_flag = device_lost.clone();
    let err_fn = move |err| {
        eprintln!("audio input stream error: {err}");
        err_flag.store(true, Ordering::SeqCst);
    };

    let stream = match sample_format {
        SampleFormat::F32 => device.build_input_stream(
            &stream_config,
            move |data: &[f32], _| {
                let _ = data_tx.send(data.to_vec());
            },
            err_fn,
            None,
        ),
        SampleFormat::I16 => device.build_input_stream(
            &stream_config,
            move |data: &[i16], _| {
                let converted: Vec<f32> = data.iter().map(|&s| s as f32 / i16::MAX as f32).collect();
                let _ = data_tx.send(converted);
            },
            err_fn,
            None,
        ),
        SampleFormat::U16 => device.build_input_stream(
            &stream_config,
            move |data: &[u16], _| {
                let converted: Vec<f32> = data
                    .iter()
                    .map(|&s| (s as f32 - u16::MAX as f32 / 2.0) / (u16::MAX as f32 / 2.0))
                    .collect();
                let _ = data_tx.send(converted);
            },
            err_fn,
            None,
        ),
        other => anyhow::bail!("unsupported sample format: {other:?}"),
    }
    .context("failed to build input stream")?;

    stream.play().context("failed to start input stream")?;
    let _ = reply_tx.send(AudioReply::Started);

    let mut raw_samples: Vec<f32> = Vec::new();
    let mut last_level_sent = Instant::now();
    let mut cancelled = false;
    let mut capped = false;
    let mut auto_stopped_silence = false;
    let mut disconnected = false;

    let started = Instant::now();
    let max_elapsed = Duration::from_secs(max_secs);
    let raw_sample_cap = max_raw_samples(max_secs, in_rate, in_channels);

    let mut silence_detector = handsfree_silence_secs.map(SilenceDetector::new);
    let frame_len_samples = (in_rate as usize / 10).max(1) * in_channels; // ~100ms of interleaved samples
    let mut frame_accum: Vec<f32> = Vec::new();

    'capture: loop {
        // A device fault reported by cpal's error callback takes priority:
        // stop cleanly and salvage/discard below.
        if device_lost.load(Ordering::SeqCst) {
            disconnected = true;
            break 'capture;
        }

        // Hard duration cap (and its buffer-length backstop): auto-stop and
        // keep what we captured so a stuck key can't record forever.
        if started.elapsed() >= max_elapsed || raw_samples.len() >= raw_sample_cap {
            capped = true;
            break 'capture;
        }

        // Drain whatever audio has arrived, then check for a control
        // command, with a short timeout so level updates stay responsive
        // even during silence.
        match data_rx.recv_timeout(Duration::from_millis(20)) {
            Ok(chunk) => {
                if last_level_sent.elapsed() >= LEVEL_UPDATE_INTERVAL {
                    let peak = chunk.iter().fold(0.0f32, |m, s| m.max(s.abs())).min(1.0);
                    let _ = reply_tx.send(AudioReply::Level(peak));
                    last_level_sent = Instant::now();
                }
                if let Some(detector) = silence_detector.as_mut() {
                    frame_accum.extend_from_slice(&chunk);
                    while frame_accum.len() >= frame_len_samples {
                        let frame: Vec<f32> = frame_accum.drain(..frame_len_samples).collect();
                        if detector.push_frame(rms(&frame)) {
                            auto_stopped_silence = true;
                        }
                    }
                }
                raw_samples.extend(chunk);
                if auto_stopped_silence {
                    break 'capture;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break 'capture,
        }

        match cmd_rx.try_recv() {
            Ok(AudioCommand::Stop) => break 'capture,
            Ok(AudioCommand::Cancel) => {
                cancelled = true;
                break 'capture;
            }
            Ok(AudioCommand::Start { .. }) => {} // already recording; ignore
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => break 'capture,
        }
    }

    drop(stream);
    // Don't drain further after a device fault — the queue may be stale/torn.
    if !disconnected {
        while let Ok(chunk) = data_rx.try_recv() {
            raw_samples.extend(chunk);
        }
    }

    if cancelled {
        let _ = reply_tx.send(AudioReply::Cancelled);
        return Ok(());
    }

    let mono = downmix_to_mono(&raw_samples, in_channels);
    let duration = Duration::from_secs_f64(mono.len() as f64 / in_rate as f64);

    if disconnected {
        // Salvage the take only if it's long enough to be worth transcribing;
        // otherwise hand back an empty buffer so the coordinator discards it.
        let samples = if duration.as_secs_f64() >= MIN_SALVAGE_SECS {
            resample_linear(&mono, in_rate, TARGET_SAMPLE_RATE)
        } else {
            Vec::new()
        };
        let _ = reply_tx.send(AudioReply::Disconnected { samples, duration });
        return Ok(());
    }

    if auto_stopped_silence {
        eprintln!(
            "[vzt-flow] hands-free recording auto-stopped after {:.1}s of trailing silence",
            duration.as_secs_f64()
        );
    }

    let resampled = resample_linear(&mono, in_rate, TARGET_SAMPLE_RATE);
    let _ = reply_tx.send(AudioReply::Stopped {
        samples: resampled,
        duration,
        capped,
        auto_stopped_silence,
    });
    Ok(())
}

/// Loads an arbitrary audio file (wav directly via `hound`; anything else
/// shelled out to the system `ffmpeg`) and returns 16kHz mono f32 PCM plus
/// its real-world duration. Shared by the CLI's `transcribe`/`clean-test`
/// commands and the daemon's `transcribe` socket command so both go through
/// identical file-loading logic.
pub fn load_audio_file_as_f32(path: &std::path::Path) -> Result<(Vec<f32>, Duration)> {
    let is_wav = path.extension().map(|e| e.eq_ignore_ascii_case("wav")).unwrap_or(false);

    let wav_path: std::path::PathBuf = if is_wav {
        path.to_path_buf()
    } else {
        let tmp = std::env::temp_dir().join(format!("flow-convert-{}.wav", std::process::id()));
        let status = std::process::Command::new("ffmpeg")
            .args(["-y", "-i"])
            .arg(path)
            .args(["-ar", "16000", "-ac", "1", "-f", "wav"])
            .arg(&tmp)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .context("failed to invoke ffmpeg (is it installed and on PATH?)")?;
        if !status.success() {
            anyhow::bail!("ffmpeg conversion failed for {}", path.display());
        }
        tmp
    };

    let mut reader = hound::WavReader::open(&wav_path)
        .with_context(|| format!("failed to open wav {}", wav_path.display()))?;
    let spec = reader.spec();
    let duration = Duration::from_secs_f64(reader.duration() as f64 / spec.sample_rate as f64);

    let raw_samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .collect::<std::result::Result<Vec<_>, _>>()
            .context("failed to read f32 wav samples")?,
        hound::SampleFormat::Int => {
            let max = (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f32 / max))
                .collect::<std::result::Result<Vec<_>, _>>()
                .context("failed to read int wav samples")?
        }
    };

    let mono = if spec.channels > 1 {
        raw_samples
            .chunks(spec.channels as usize)
            .map(|frame| frame.iter().sum::<f32>() / frame.len() as f32)
            .collect()
    } else {
        raw_samples
    };

    let resampled = resample_linear(&mono, spec.sample_rate, TARGET_SAMPLE_RATE);

    if !is_wav {
        let _ = std::fs::remove_file(&wav_path);
    }

    Ok((resampled, duration))
}

fn downmix_to_mono(samples: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return samples.to_vec();
    }
    samples
        .chunks(channels)
        .map(|frame| frame.iter().sum::<f32>() / frame.len() as f32)
        .collect()
}

/// Simple linear-interpolation resampler. transcribe-rs models are trained
/// on speech, not music, and linear resampling is transparent enough at
/// this ratio (typically 48kHz -> 16kHz, a clean 3:1) that it doesn't
/// measurably hurt recognition accuracy — chosen over `rubato` to avoid an
/// extra dependency/API surface for what is a straightforward decimation.
pub fn resample_linear(input: &[f32], in_rate: u32, out_rate: u32) -> Vec<f32> {
    if in_rate == out_rate || input.is_empty() {
        return input.to_vec();
    }
    let ratio = in_rate as f64 / out_rate as f64;
    let out_len = (input.len() as f64 / ratio).ceil() as usize;
    let mut output = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src_pos = i as f64 * ratio;
        let idx = src_pos.floor() as usize;
        let frac = (src_pos - idx as f64) as f32;
        let a = input.get(idx).copied().unwrap_or(0.0);
        let b = input.get(idx + 1).copied().unwrap_or(a);
        output.push(a + (b - a) * frac);
    }
    output
}

/// Root-mean-square energy of a frame of samples, used by the hands-free
/// silence detector. Independent of `downmix_to_mono`/resampling — this
/// runs on raw (possibly multi-channel, native sample rate) frames.
fn rms(frame: &[f32]) -> f32 {
    if frame.is_empty() {
        return 0.0;
    }
    let sum_sq: f32 = frame.iter().map(|s| s * s).sum();
    (sum_sq / frame.len() as f32).sqrt()
}

/// Number of ~100ms frames the initial noise-floor calibration spans.
const CALIBRATION_FRAMES: usize = 3; // ~300ms

/// How far above the calibrated noise floor a frame's RMS must be to count
/// as "loud" (speech), and the absolute floor under which we never treat a
/// frame as loud regardless of a near-zero noise floor.
const LOUD_MULTIPLIER: f32 = 4.0;
const MIN_LOUD_THRESHOLD: f32 = 0.01;

/// Energy-based voice activity detector for hands-free auto-stop: after
/// calibrating a noise floor from the first [`CALIBRATION_FRAMES`] frames,
/// it watches for at least one "loud" (speech) frame, then counts
/// consecutive quiet frames afterward. Once that trailing-silence run
/// reaches the configured duration, [`Self::push_frame`] returns `true`.
///
/// Deliberately simple (energy threshold, no spectral/ML VAD) per the
/// brief — good enough for "did the user stop talking", and avoids an ONNX
/// VAD dependency in this phase.
pub struct SilenceDetector {
    calibration_sum: f32,
    calibration_count: usize,
    noise_floor: Option<f32>,
    loud_threshold: f32,
    had_loud_frame: bool,
    quiet_frames: usize,
    stop_after_quiet_frames: usize,
}

impl SilenceDetector {
    /// `silence_secs` is the trailing-silence duration required to trigger
    /// a stop; frames are assumed to be ~100ms each (matching the caller's
    /// `frame_len_samples` = sample_rate/10 * channels).
    pub fn new(silence_secs: f64) -> Self {
        Self {
            calibration_sum: 0.0,
            calibration_count: 0,
            noise_floor: None,
            loud_threshold: MIN_LOUD_THRESHOLD,
            had_loud_frame: false,
            quiet_frames: 0,
            stop_after_quiet_frames: ((silence_secs * 10.0).round() as usize).max(1),
        }
    }

    /// Feeds one ~100ms frame's RMS energy. Returns `true` once enough
    /// trailing silence has elapsed following at least one loud frame.
    pub fn push_frame(&mut self, frame_rms: f32) -> bool {
        if self.noise_floor.is_none() {
            self.calibration_sum += frame_rms;
            self.calibration_count += 1;
            if self.calibration_count >= CALIBRATION_FRAMES {
                let floor = self.calibration_sum / self.calibration_count as f32;
                self.noise_floor = Some(floor);
                self.loud_threshold = (floor * LOUD_MULTIPLIER).max(MIN_LOUD_THRESHOLD);
            }
            return false; // never trigger mid-calibration
        }

        if frame_rms > self.loud_threshold {
            self.had_loud_frame = true;
            self.quiet_frames = 0;
            false
        } else {
            if !self.had_loud_frame {
                return false; // haven't heard speech yet; nothing to end
            }
            self.quiet_frames += 1;
            self.quiet_frames >= self.stop_after_quiet_frames
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_sample_cap_matches_duration_plus_margin() {
        // 120s hold cap at 48kHz stereo = 120*48000*2 plus a 5s margin.
        let cap = max_raw_samples(120, 48_000, 2);
        assert_eq!(cap, (120 + 5) * 48_000 * 2);

        // 300s hands-free cap at 44.1kHz mono.
        let cap = max_raw_samples(300, 44_100, 1);
        assert_eq!(cap, (300 + 5) * 44_100 * 1);
    }

    #[test]
    fn raw_sample_cap_at_new_max_hold_is_reasonable_memory() {
        // 600s (the new max_hold_secs/max_handsfree_secs default) at 48kHz
        // stereo f32 -- the worst-case native mic format, and this is a
        // backstop only (the wall-clock check normally fires first). Should
        // land well under a few hundred MB, not gigabytes.
        let cap = max_raw_samples(600, 48_000, 2);
        let bytes = cap * std::mem::size_of::<f32>();
        assert!(bytes < 300 * 1024 * 1024, "raw capture backstop grew unexpectedly large: {bytes} bytes");
        // Sanity: still comfortably larger than the old 120s cap's backstop.
        let old_cap_bytes = max_raw_samples(120, 48_000, 2) * std::mem::size_of::<f32>();
        assert!(bytes > old_cap_bytes);
    }

    #[test]
    fn raw_sample_cap_is_saturating_and_nonzero() {
        // Degenerate inputs must not panic or produce a zero budget that
        // would instantly cap every recording.
        assert!(max_raw_samples(0, 0, 0) > 0);
        assert_eq!(max_raw_samples(u64::MAX, u32::MAX, usize::MAX), usize::MAX);
    }

    #[test]
    fn min_salvage_threshold_is_one_second() {
        // Guards the >1s salvage decision used on device disconnect.
        assert!(0.5 < MIN_SALVAGE_SECS);
        assert!(1.5 >= MIN_SALVAGE_SECS);
    }

    #[test]
    fn rms_of_silence_is_zero() {
        assert_eq!(rms(&[0.0; 100]), 0.0);
    }

    #[test]
    fn rms_of_constant_amplitude_equals_that_amplitude() {
        let frame = vec![0.5f32; 100];
        assert!((rms(&frame) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn silence_detector_never_fires_during_calibration() {
        let mut d = SilenceDetector::new(2.5);
        // Even loud frames during the first CALIBRATION_FRAMES must not
        // trigger a stop — they're establishing the noise floor, not
        // counted as "heard speech yet".
        assert!(!d.push_frame(0.9));
        assert!(!d.push_frame(0.9));
        assert!(!d.push_frame(0.9));
    }

    #[test]
    fn silence_detector_requires_a_loud_frame_before_it_can_fire() {
        let mut d = SilenceDetector::new(0.3); // 3 quiet frames to trigger
        // Calibrate on near-silence.
        for _ in 0..CALIBRATION_FRAMES {
            d.push_frame(0.001);
        }
        // Continuing quiet frames with no loud frame ever seen must never
        // trigger — there's no speech to end yet.
        for _ in 0..20 {
            assert!(!d.push_frame(0.001));
        }
    }

    #[test]
    fn silence_detector_fires_after_configured_trailing_silence() {
        let mut d = SilenceDetector::new(0.3); // 3 quiet (100ms) frames
        for _ in 0..CALIBRATION_FRAMES {
            d.push_frame(0.001); // calibrate on near-silence
        }
        assert!(!d.push_frame(0.5)); // loud (speech) frame
        assert!(!d.push_frame(0.001)); // quiet frame 1
        assert!(!d.push_frame(0.001)); // quiet frame 2
        assert!(d.push_frame(0.001)); // quiet frame 3 -> trailing silence complete
    }

    #[test]
    fn silence_detector_resets_the_quiet_run_on_renewed_speech() {
        let mut d = SilenceDetector::new(0.3);
        for _ in 0..CALIBRATION_FRAMES {
            d.push_frame(0.001);
        }
        assert!(!d.push_frame(0.5));
        assert!(!d.push_frame(0.001));
        assert!(!d.push_frame(0.001));
        // Speech resumes right before the silence run would have fired —
        // the quiet counter must reset instead of firing on the next frame.
        assert!(!d.push_frame(0.5));
        assert!(!d.push_frame(0.001));
        assert!(!d.push_frame(0.001));
        assert!(d.push_frame(0.001)); // now 3 quiet frames since the last loud one
    }
}
