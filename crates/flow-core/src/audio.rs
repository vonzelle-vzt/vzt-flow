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
