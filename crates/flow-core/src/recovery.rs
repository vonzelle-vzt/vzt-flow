//! Never-lose-a-take support for long dictations: the audio statistics that
//! tell "the mic heard nothing" apart from "the recognizer returned nothing",
//! the single-slot recovery recording (`~/.config/vzt-flow/recovery/last.wav`),
//! and re-transcribing that recording through the model manager.
//!
//! Background: every failure after the hotkey is released (a rolling watchdog
//! trip, an empty result, a failed chunk) used to end the dictation with
//! nothing pasted *and nothing kept* — the audio lived only in memory and the
//! history file is written on success only. So a multi-minute dictation that
//! failed left no trace at all. The coordinator now writes the audio here
//! whenever the transcript is incomplete or missing, and the tray's "Recover
//! last recording" item turns it back into text.
//!
//! Only one recording is kept, and it stays on this machine; each save
//! overwrites the previous one.

use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::chunking::{self, ChunkPlan, SAMPLE_RATE};
use crate::engine::Transcript;
use crate::meeting::transcriber::{rms, FRAME_SECS, SPEECH_RMS_THRESHOLD};
use crate::model_manager::ModelCommand;

/// Peak amplitude below which a recording is treated as "the microphone
/// delivered silence" (-46 dBFS). A muted mic, a revoked Microphone grant and a
/// headset that dropped its input link all deliver exact or near-exact zeros;
/// real room noise on any working mic peaks well above this.
pub const MIC_SILENT_PEAK: f32 = 0.005;

/// Minimum speech-level audio (100ms frames at or above
/// [`SPEECH_RMS_THRESHOLD`]) for a span to count as "someone was talking".
pub const MIN_SPEECH_SECS: f32 = 2.0;

/// ...and the minimum fraction of the span those frames must make up, so a
/// long recording of a quiet room with one cough doesn't qualify.
pub const MIN_SPEECH_FRACTION: f32 = 0.2;

/// Cheap signal statistics for a 16 kHz mono span.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AudioStats {
    /// Largest absolute sample value, in `[0, 1]` for sane input.
    pub peak: f32,
    /// Seconds of 100ms frames whose RMS reaches the speech threshold.
    pub speech_secs: f32,
    pub duration_secs: f32,
}

impl AudioStats {
    pub fn from_samples(samples: &[f32], sample_rate: u32) -> Self {
        let sr = sample_rate.max(1);
        let frame = ((FRAME_SECS * sr as f32) as usize).max(1);
        let peak = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        let speech_frames = samples
            .chunks(frame)
            .filter(|f| f.len() == frame && rms(f) >= SPEECH_RMS_THRESHOLD)
            .count();
        Self {
            peak,
            speech_secs: speech_frames as f32 * frame as f32 / sr as f32,
            duration_secs: samples.len() as f32 / sr as f32,
        }
    }

    /// The microphone produced (near-)digital silence.
    pub fn mic_silent(&self) -> bool {
        self.peak < MIC_SILENT_PEAK
    }

    /// Enough speech-level energy that an empty transcript means words were
    /// lost, not that nobody spoke.
    pub fn has_speech(&self) -> bool {
        self.speech_secs >= MIN_SPEECH_SECS
            && self.duration_secs > 0.0
            && self.speech_secs / self.duration_secs >= MIN_SPEECH_FRACTION
    }
}

/// `~/.config/vzt-flow/recovery` (honours `VZT_FLOW_CONFIG_DIR`).
pub fn recovery_dir() -> Result<PathBuf> {
    Ok(crate::config::config_dir()?.join("recovery"))
}

/// The one saved recording.
pub fn last_recording_path() -> Result<PathBuf> {
    Ok(recovery_dir()?.join("last.wav"))
}

/// Whether a recording is available to recover.
pub fn has_last_recording() -> bool {
    last_recording_path().map(|p| p.is_file()).unwrap_or(false)
}

/// Writes `samples` (16 kHz mono f32) as 16-bit PCM to `path`, atomically: the
/// data goes to a sibling temp file that is renamed over `path`, so a reader
/// (the tray's recover action) never sees a half-written file.
pub fn write_wav_atomic(path: &std::path::Path, samples: &[f32], sample_rate: u32) -> Result<()> {
    let dir = path.parent().context("recovery path has no parent directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let tmp = path.with_extension("wav.tmp");
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    {
        let mut w = hound::WavWriter::create(&tmp, spec)
            .with_context(|| format!("failed to create {}", tmp.display()))?;
        for &s in samples {
            w.write_sample((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)?;
        }
        w.finalize().context("failed to finalize recovery wav")?;
    }
    std::fs::rename(&tmp, path)
        .with_context(|| format!("failed to move {} into place", tmp.display()))?;
    Ok(())
}

/// Saves a dictation's audio as the recovery recording, replacing any
/// previous one. Returns the path written.
pub fn save_last_recording(samples: &[f32]) -> Result<PathBuf> {
    let path = last_recording_path()?;
    write_wav_atomic(&path, samples, SAMPLE_RATE)?;
    Ok(path)
}

/// Result of re-transcribing the recovery recording.
#[derive(Debug, Clone)]
pub struct Recovered {
    pub text: String,
    pub duration: Duration,
    pub chunks: usize,
    /// Chunks that failed twice and are missing from `text`.
    pub failed_chunks: usize,
}

/// How long one recovery chunk may take before it is given up on. Chunks are
/// ≤35s of audio; the first may also pay a model load.
const RECOVERY_CHUNK_TIMEOUT: Duration = Duration::from_secs(180);

/// Transcribes `samples` through the shared model manager **one ≤35s chunk at
/// a time** — the same silence-aware plan and seam de-dup as
/// [`chunking::transcribe_long`], never a single >35s engine call.
///
/// Chunks are sent individually and each is awaited before the next is sent,
/// deliberately, instead of one `ModelCommand::Transcribe` for the whole file:
/// the model manager is one queue shared with live dictation, and a
/// 10-minute recovery submitted in one go would park a new dictation's
/// rolling chunks behind minutes of work (enough to trip their watchdog).
/// Pacing keeps at most one recovery chunk ahead of them.
pub fn transcribe_paced(model_cmd_tx: &Sender<ModelCommand>, samples: &[f32]) -> Result<Recovered> {
    transcribe_paced_with(model_cmd_tx, samples, RECOVERY_CHUNK_TIMEOUT)
}

fn transcribe_paced_with(
    model_cmd_tx: &Sender<ModelCommand>,
    samples: &[f32],
    chunk_timeout: Duration,
) -> Result<Recovered> {
    let plans: Vec<ChunkPlan> = if samples.is_empty() {
        Vec::new()
    } else {
        chunking::plan_chunks(samples, SAMPLE_RATE)
    };
    let mut transcripts = Vec::with_capacity(plans.len());
    let mut failed = 0usize;
    for (i, plan) in plans.iter().enumerate() {
        let chunk = &samples[plan.start..plan.start + plan.len];
        let mut text = None;
        for attempt in 0..2 {
            let (tx, rx) = mpsc::channel();
            model_cmd_tx
                .send(ModelCommand::TranscribeChunk { samples: chunk.to_vec(), reply: tx })
                .map_err(|_| anyhow::anyhow!("transcriber is not running"))?;
            match rx.recv_timeout(chunk_timeout) {
                Ok(Ok(t)) => {
                    text = Some(t);
                    break;
                }
                Ok(Err(e)) => eprintln!(
                    "[vzt-flow] recovery chunk {}/{} failed (attempt {}): {e}",
                    i + 1,
                    plans.len(),
                    attempt + 1
                ),
                Err(_) => eprintln!(
                    "[vzt-flow] recovery chunk {}/{} timed out (attempt {})",
                    i + 1,
                    plans.len(),
                    attempt + 1
                ),
            }
        }
        transcripts.push(text.unwrap_or_else(|| {
            failed += 1;
            Transcript { text: String::new(), segments: None }
        }));
    }
    let assembled = chunking::assemble(&plans, &transcripts, SAMPLE_RATE);
    Ok(Recovered {
        text: assembled.text,
        duration: Duration::from_secs_f64(samples.len() as f64 / SAMPLE_RATE as f64),
        chunks: plans.len(),
        failed_chunks: failed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(secs: f32, amp: f32) -> Vec<f32> {
        (0..(secs * SAMPLE_RATE as f32) as usize)
            .map(|i| amp * (i as f32 * 2.0 * std::f32::consts::PI * 220.0 / SAMPLE_RATE as f32).sin())
            .collect()
    }

    #[test]
    fn digital_silence_reads_as_a_silent_mic_with_no_speech() {
        let s = AudioStats::from_samples(&vec![0.0; SAMPLE_RATE as usize * 20], SAMPLE_RATE);
        assert!(s.mic_silent());
        assert!(!s.has_speech());
        assert!((s.duration_secs - 20.0).abs() < 1e-3);
    }

    #[test]
    fn a_quiet_room_is_not_silent_but_is_not_speech() {
        // Noise floor ~-50 dBFS peaks: a working mic nobody talked into.
        let s = AudioStats::from_samples(&tone(20.0, 0.006), SAMPLE_RATE);
        assert!(!s.mic_silent());
        assert!(!s.has_speech(), "{s:?}");
    }

    #[test]
    fn sustained_speech_level_audio_counts_as_speech() {
        let s = AudioStats::from_samples(&tone(12.0, 0.1), SAMPLE_RATE);
        assert!(s.has_speech(), "{s:?}");
        assert!((s.speech_secs - 12.0).abs() < 0.2);
    }

    #[test]
    fn one_short_burst_in_a_long_quiet_take_is_not_speech() {
        // 1s of loud audio in 30s: below both the absolute and the fraction bar.
        let mut samples = vec![0.0f32; SAMPLE_RATE as usize * 30];
        samples.splice(0..SAMPLE_RATE as usize, tone(1.0, 0.2));
        assert!(!AudioStats::from_samples(&samples, SAMPLE_RATE).has_speech());
    }

    #[test]
    fn recovery_wav_round_trips_through_the_normal_loader() {
        let dir = std::env::temp_dir().join(format!("vzt-recovery-test-{}", std::process::id()));
        let path = dir.join("last.wav");
        let samples = tone(2.0, 0.5);
        write_wav_atomic(&path, &samples, SAMPLE_RATE).unwrap();
        assert!(!path.with_extension("wav.tmp").exists(), "temp file must be renamed away");
        let (back, dur) = crate::audio::load_audio_file_as_f32(&path).unwrap();
        assert_eq!(back.len(), samples.len());
        assert!((dur.as_secs_f64() - 2.0).abs() < 1e-3);
        let max_err = samples.iter().zip(&back).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
        assert!(max_err < 1e-3, "16-bit quantization error too large: {max_err}");
        // A second save replaces the first (single slot).
        write_wav_atomic(&path, &samples[..SAMPLE_RATE as usize], SAMPLE_RATE).unwrap();
        let (again, _) = crate::audio::load_audio_file_as_f32(&path).unwrap();
        assert_eq!(again.len(), SAMPLE_RATE as usize);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Deterministic speech-level noise. Unlike a pure tone (220 Hz divides the
    /// 33.5s hard-cut offset exactly), every chunk's content differs, so the
    /// fake manager can tell a retry from a new chunk of the same length.
    fn noise(secs: f32, amp: f32) -> Vec<f32> {
        let mut x: u32 = 0x1234_5678;
        (0..(secs * SAMPLE_RATE as f32) as usize)
            .map(|_| {
                x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                amp * ((x >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0)
            })
            .collect()
    }

    /// Fake model manager: answers every chunk with `c<n>` (n = order of first
    /// sight) and fails the first attempt of chunk index `fail_first`.
    fn fake_manager(fail_first: Option<usize>) -> (Sender<ModelCommand>, std::thread::JoinHandle<Vec<usize>>) {
        let (tx, rx) = mpsc::channel::<ModelCommand>();
        let h = std::thread::spawn(move || {
            let mut lens = Vec::new();
            let mut seen: Vec<(usize, u32, u32)> = Vec::new();
            while let Ok(cmd) = rx.recv() {
                let ModelCommand::TranscribeChunk { samples, reply } = cmd else {
                    panic!("recovery must never send a whole-file Transcribe");
                };
                assert!(samples.len() <= (35.0 * SAMPLE_RATE as f32) as usize + 1, "chunk over 35s");
                lens.push(samples.len());
                let key = (samples.len(), samples[7].to_bits(), samples[samples.len() / 2].to_bits());
                let (idx, is_retry) = match seen.iter().position(|k| *k == key) {
                    Some(i) => (i, true),
                    None => {
                        seen.push(key);
                        (seen.len() - 1, false)
                    }
                };
                if fail_first == Some(idx) && !is_retry {
                    let _ = reply.send(Err("boom".into()));
                } else {
                    let _ = reply.send(Ok(Transcript { text: format!("c{}", idx + 1), segments: None }));
                }
            }
            lens
        });
        (tx, h)
    }

    #[test]
    fn recovery_paces_bounded_chunks_and_retries_a_failure() {
        // 100s of continuous speech-level noise → hard cuts → 3+ chunks, each ≤35s.
        let samples = noise(100.0, 0.2);
        let (tx, h) = fake_manager(Some(1));
        let r = transcribe_paced_with(&tx, &samples, Duration::from_secs(5)).unwrap();
        drop(tx);
        let calls = h.join().unwrap();
        assert!(r.chunks >= 3);
        assert_eq!(r.failed_chunks, 0, "the retry must recover chunk 2");
        assert_eq!(calls.len(), r.chunks + 1, "exactly one extra call: the retry");
        assert!(r.text.contains("c1") && r.text.contains("c2") && r.text.contains("c3"), "{}", r.text);
        assert!((r.duration.as_secs_f64() - 100.0).abs() < 1e-3);
    }
}
