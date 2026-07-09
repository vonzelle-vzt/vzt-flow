//! System / application audio capture via ScreenCaptureKit (macOS 13+).
//!
//! Uses the `screencapturekit` crate (v8, current as of 2026-06) for safe
//! bindings to Apple's `ScreenCaptureKit` framework. We build an audio-only
//! `SCStream`: a display content filter (SCK requires a content filter even
//! for audio), `capturesAudio = true`, and `excludesCurrentProcessAudio =
//! true` so VZT Flow's own output (there is none, but belt-and-suspenders) is
//! never fed back in. Only an *audio* output handler is registered; video
//! frames are never delivered to us.
//!
//! ScreenCaptureKit delivers deinterleaved Float32 PCM at the configured rate
//! (48 kHz stereo here); [`SystemAudioCapture`] downmixes each buffer to mono
//! and forwards it over a channel. The caller resamples to 16 kHz on flush.
//!
//! Requires the Screen Recording TCC permission. When `flow` is launched from
//! a terminal, the grant belongs to the *terminal app*, not `flow` — see
//! [`ensure_screen_permission`].

use std::sync::mpsc;
use std::time::Duration;

use anyhow::{anyhow, Result};

use screencapturekit::cm::{AudioBufferList, CMSampleBufferExt};
use screencapturekit::prelude::*;

/// Sample rate we request from ScreenCaptureKit. 48 kHz is SCK's native audio
/// rate; we downmix to mono here and resample to 16 kHz at flush time via
/// `flow_core::audio::resample_linear`.
const SYSTEM_SAMPLE_RATE: u32 = 48_000;
/// Channel count we request (stereo); downmixed to mono on the way out.
const SYSTEM_CHANNELS: i32 = 2;

/// Receives deinterleaved audio sample buffers from ScreenCaptureKit's
/// background delivery queue and forwards each as a mono `f32` block.
struct AudioHandler {
    tx: mpsc::Sender<Vec<f32>>,
    debug: bool,
    debug_count: std::sync::atomic::AtomicU32,
}

impl SCStreamOutputTrait for AudioHandler {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, output_type: SCStreamOutputType) {
        if output_type != SCStreamOutputType::Audio {
            return;
        }
        // Materialize the sample's data before reading it — an audio
        // CMSampleBuffer's AudioBufferList can be allocated-but-empty (all
        // zeros) until its backing block buffer is made ready.
        let _ = sample.make_data_ready();
        if let Some(list) = sample.audio_buffer_list() {
            if self.debug {
                let n = self.debug_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if n < 3 {
                    let nb = list.num_buffers();
                    let mut desc = format!("[vzt-flow] SCK audio buffer #{n}: num_buffers={nb}");
                    for i in 0..nb {
                        if let Some(b) = list.get(i) {
                            let f = bytes_to_f32(b.data());
                            let head: Vec<f32> = f.iter().take(4).copied().collect();
                            desc.push_str(&format!(
                                " | buf{i}: channels={} bytes={} first={head:?}",
                                b.number_channels, b.data_bytes_size
                            ));
                        }
                    }
                    eprintln!("{desc}");
                }
            }
            let mono = buffer_list_to_mono_f32(&list);
            if !mono.is_empty() {
                let _ = self.tx.send(mono);
            }
        }
    }
}

/// No-op screen consumer. ScreenCaptureKit only runs its capture pipeline
/// (and therefore delivers *audio*) when the stream has an active output; with
/// an audio-only handler some macOS versions deliver silent audio buffers.
/// Registering a screen handler that immediately drops every frame keeps the
/// pipeline running without our doing any video work. (The upstream crate's
/// own audio example registers both a screen and an audio handler for this
/// reason.)
struct ScreenSink;

impl SCStreamOutputTrait for ScreenSink {
    fn did_output_sample_buffer(&self, _sample: CMSampleBuffer, _output_type: SCStreamOutputType) {}
}

/// A live ScreenCaptureKit audio capture. Holds the `SCStream` alive for the
/// capture's duration. `SCStream` wraps non-`Send` Objective-C objects, so a
/// value of this type must stay on the thread that created it (the meeting
/// session drives it from a single thread).
pub struct SystemAudioCapture {
    stream: SCStream,
    rx: mpsc::Receiver<Vec<f32>>,
    stopped: bool,
}

impl SystemAudioCapture {
    /// Starts an audio-only ScreenCaptureKit stream over the main display.
    /// Fails if Screen Recording permission is denied (surfaced by
    /// `SCShareableContent::get`) or no display is available.
    pub fn start() -> Result<Self> {
        let content = SCShareableContent::get()
            .map_err(|e| anyhow!("SCShareableContent::get failed (Screen Recording permission?): {e:?}"))?;
        let display = content
            .displays()
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("no displays available for ScreenCaptureKit"))?;

        let filter = SCContentFilter::create()
            .with_display(&display)
            .with_excluding_windows(&[])
            .build();

        // A minimal-but-valid video config (SCK requires nonzero dimensions);
        // we register no screen handler, so no frames are delivered to us.
        let config = SCStreamConfiguration::new()
            .with_width(128)
            .with_height(128)
            .with_captures_audio(true)
            .with_excludes_current_process_audio(true)
            .with_sample_rate(SYSTEM_SAMPLE_RATE as i32)
            .with_channel_count(SYSTEM_CHANNELS);

        let (tx, rx) = mpsc::channel::<Vec<f32>>();
        let debug = std::env::var("FLOW_MEETING_DEBUG").is_ok();
        let mut stream = SCStream::new(&filter, &config);
        // A screen consumer must be registered for the pipeline to run and
        // deliver non-silent audio (see `ScreenSink`); frames are discarded.
        stream.add_output_handler(ScreenSink, SCStreamOutputType::Screen);
        stream.add_output_handler(
            AudioHandler { tx, debug, debug_count: std::sync::atomic::AtomicU32::new(0) },
            SCStreamOutputType::Audio,
        );
        stream
            .start_capture()
            .map_err(|e| anyhow!("SCStream::start_capture failed: {e:?}"))?;

        Ok(Self { stream, rx, stopped: false })
    }

    /// Native sample rate of the mono blocks this capture produces.
    pub fn sample_rate(&self) -> u32 {
        SYSTEM_SAMPLE_RATE
    }

    /// Waits up to `timeout` for the next mono audio block.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Vec<f32>, mpsc::RecvTimeoutError> {
        self.rx.recv_timeout(timeout)
    }

    /// Stops the underlying capture stream. Idempotent.
    pub fn stop(mut self) {
        self.stop_inner();
    }

    fn stop_inner(&mut self) {
        if !self.stopped {
            let _ = self.stream.stop_capture();
            self.stopped = true;
        }
    }
}

impl Drop for SystemAudioCapture {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

/// Downmixes a ScreenCaptureKit `AudioBufferList` to a single mono `f32`
/// stream. SCK delivers Float32 PCM; stereo audio arrives *deinterleaved*
/// (one `AudioBuffer` per channel), so we average across buffers element-wise.
/// The rare interleaved single-buffer case is handled too.
fn buffer_list_to_mono_f32(list: &AudioBufferList) -> Vec<f32> {
    let n = list.num_buffers();
    if n == 0 {
        return Vec::new();
    }

    // Reinterpret each buffer's bytes as f32.
    let planes: Vec<Vec<f32>> = (0..n)
        .filter_map(|i| list.get(i))
        .map(|buf| bytes_to_f32(buf.data()))
        .collect();
    if planes.is_empty() {
        return Vec::new();
    }

    if planes.len() == 1 {
        // Single buffer: either mono, or interleaved multi-channel.
        let channels = list.get(0).map(|b| b.number_channels).unwrap_or(1).max(1) as usize;
        if channels <= 1 {
            return planes.into_iter().next().unwrap();
        }
        let plane = &planes[0];
        return plane
            .chunks(channels)
            .map(|frame| frame.iter().sum::<f32>() / frame.len() as f32)
            .collect();
    }

    // Deinterleaved (planar): one buffer per channel — average element-wise.
    let len = planes.iter().map(|p| p.len()).min().unwrap_or(0);
    let planes_n = planes.len() as f32;
    (0..len)
        .map(|i| planes.iter().map(|p| p[i]).sum::<f32>() / planes_n)
        .collect()
}

/// Reinterprets a little-endian `f32` byte slice as `f32` samples, ignoring a
/// trailing partial sample if the length isn't a multiple of 4.
fn bytes_to_f32(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    /// Reports whether the calling process already has Screen Recording
    /// permission, without prompting.
    fn CGPreflightScreenCaptureAccess() -> bool;
    /// Prompts for Screen Recording permission if not already granted. Returns
    /// whether access is granted. The prompt is shown at most once per app;
    /// after the first denial the user must grant it manually in System
    /// Settings.
    fn CGRequestScreenCaptureAccess() -> bool;
}

/// Ensures (best effort) that Screen Recording permission is granted, printing
/// the exact System Settings path if it isn't. When `flow` runs from a
/// terminal, the grant belongs to the terminal application, not `flow`.
pub fn ensure_screen_permission() {
    let granted = unsafe { CGPreflightScreenCaptureAccess() };
    if granted {
        return;
    }
    eprintln!(
        "[vzt-flow] Screen Recording permission is required to capture meeting (system) audio.\n\
         Grant it here: System Settings › Privacy & Security › Screen Recording\n\
         When running from a terminal, enable the checkbox for YOUR TERMINAL APP\n\
         (e.g. Terminal, iTerm, Ghostty) — not \"flow\" — then re-run this command."
    );
    let requested = unsafe { CGRequestScreenCaptureAccess() };
    if !requested {
        eprintln!(
            "[vzt-flow] Permission still not granted. If you just enabled it, you may need to\n\
             quit and reopen the terminal app for the grant to take effect."
        );
    }
}
