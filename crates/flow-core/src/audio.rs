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

        println!("Recording... press Enter to stop{}.",
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
    Start { max_secs: u64 },
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
    Stopped {
        samples: Vec<f32>,
        duration: Duration,
        capped: bool,
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
                    AudioCommand::Start { max_secs } => {
                        if let Err(e) = run_one_recording(&cmd_rx, &reply_tx, max_secs) {
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
    let mut disconnected = false;

    let started = Instant::now();
    let max_elapsed = Duration::from_secs(max_secs);
    let raw_sample_cap = max_raw_samples(max_secs, in_rate, in_channels);

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
                raw_samples.extend(chunk);
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

    let resampled = resample_linear(&mono, in_rate, TARGET_SAMPLE_RATE);
    let _ = reply_tx.send(AudioReply::Stopped {
        samples: resampled,
        duration,
        capped,
    });
    Ok(())
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
}
