//! Meeting mode: live, fully-local transcription of a video call by capturing
//! both the system/app audio (the other participants, via ScreenCaptureKit)
//! and the local microphone (the user, via cpal) concurrently, streaming each
//! to a shared Parakeet engine, and writing a timestamped, speaker-labelled
//! transcript that is summarized on stop.
//!
//! Only the pure sub-modules ([`dedup`], [`transcriber`]) and the listing /
//! path helpers below compile on every platform. The live session ([`run`])
//! and system-audio capture depend on ScreenCaptureKit and are macOS-only;
//! off macOS `run` returns a clear error.

pub mod dedup;
pub mod transcriber;

#[cfg(target_os = "macos")]
mod syscapture;

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Default directory meeting transcripts are written to when `--out` isn't
/// given: `~/Documents/vzt-flow/meetings/`.
pub fn default_meetings_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("could not determine home directory")?;
    Ok(home.join("Documents").join("vzt-flow").join("meetings"))
}

/// Turns a meeting title into a filesystem-safe slug: lowercase, spaces and
/// runs of non-alphanumeric characters collapsed to single hyphens, trimmed.
/// Falls back to `"meeting"` when the title has no usable characters.
pub fn slug_title(title: &str) -> String {
    let mut slug = String::new();
    let mut prev_dash = false;
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !slug.is_empty() {
            slug.push('-');
            prev_dash = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        "meeting".to_string()
    } else {
        slug
    }
}

/// Metadata for one transcript file, for `flow meeting list`.
#[derive(Debug, Clone)]
pub struct MeetingSummary {
    pub path: PathBuf,
    /// Title parsed from the `# Meeting: <title> — <datetime>` header, or the
    /// file stem if the header can't be parsed.
    pub title: String,
    /// Datetime string parsed from the header (as written), or empty.
    pub datetime: String,
    /// Meeting duration, taken from the last `[HH:MM:SS]` line, if any.
    pub duration: Option<String>,
    /// File size in bytes.
    pub size_bytes: u64,
}

/// Lists the most recent `limit` meeting transcripts in `dir`, newest first
/// (by file modified time). Returns an empty vec if the directory doesn't
/// exist yet.
pub fn list_meetings(dir: &Path, limit: usize) -> Result<Vec<MeetingSummary>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut entries: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().map(|e| e == "md").unwrap_or(false) {
            let mtime = entry.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
            entries.push((mtime, path));
        }
    }
    entries.sort_by_key(|e| std::cmp::Reverse(e.0));
    entries.truncate(limit);

    let mut out = Vec::with_capacity(entries.len());
    for (_, path) in entries {
        out.push(summarize_file(&path));
    }
    Ok(out)
}

/// Parses one transcript file's header/last-line/size into a [`MeetingSummary`].
fn summarize_file(path: &Path) -> MeetingSummary {
    let size_bytes = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let content = fs::read_to_string(path).unwrap_or_default();

    let mut title = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "meeting".to_string());
    let mut datetime = String::new();
    let mut duration = None;

    for line in content.lines() {
        if let Some(header) = line.strip_prefix("# Meeting: ") {
            // "<title> — <datetime>" (em dash separator written by run()).
            if let Some((t, dt)) = header.split_once(" — ") {
                title = t.trim().to_string();
                datetime = dt.trim().to_string();
            } else {
                title = header.trim().to_string();
            }
        } else if let Some(ts) = parse_leading_timestamp(line) {
            // Keep the last one seen -> meeting length.
            duration = Some(ts);
        }
    }

    MeetingSummary { path: path.to_path_buf(), title, datetime, duration, size_bytes }
}

/// Extracts the `HH:MM:SS` from a `[HH:MM:SS] Speaker: ...` transcript line.
fn parse_leading_timestamp(line: &str) -> Option<String> {
    let rest = line.strip_prefix('[')?;
    let close = rest.find(']')?;
    let ts = &rest[..close];
    // Must look like HH:MM:SS.
    let parts: Vec<&str> = ts.split(':').collect();
    if parts.len() == 3 && parts.iter().all(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_digit())) {
        Some(ts.to_string())
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Live session — macOS only (ScreenCaptureKit).
// ---------------------------------------------------------------------------

/// Off macOS there is no ScreenCaptureKit, so live capture is unavailable.
/// The listing/MCP paths above still work everywhere.
#[cfg(not(target_os = "macos"))]
pub fn run(
    _title: Option<String>,
    _out_dir: Option<PathBuf>,
    _stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<PathBuf> {
    anyhow::bail!("meeting mode requires macOS (ScreenCaptureKit system-audio capture is macOS-only)")
}

#[cfg(target_os = "macos")]
pub use session::run;

#[cfg(target_os = "macos")]
mod session {
    //! The live meeting session: wires the two capture sources, the shared
    //! Parakeet engine, the transcript writer, echo dedup, and the on-stop
    //! summarizer together.

    use std::collections::VecDeque;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::Duration;

    use anyhow::{Context, Result};

    use crate::cleanup::LlamaCleanupProvider;
    use crate::dictionary;
    use crate::engine::{ParakeetTranscriber, Transcriber};
    use crate::models::{cleanup_model_path, parakeet_model_dir};

    use super::dedup::{is_echo, time_overlaps, DEFAULT_ECHO_THRESHOLD};
    use super::syscapture;
    use super::transcriber::{format_offset, Chunk, Source, StreamingChunker};
    use super::{default_meetings_dir, slug_title};

    /// Max characters of transcript fed to the summarizer. Beyond this, only
    /// the final portion is summarized (see `transcriber::truncate_for_summary`)
    /// so a marathon meeting never overruns the model's context. Sized well
    /// under the cleanup model's 8192-token context budget.
    const SUMMARY_MAX_CHARS: usize = 6_000;

    /// How long a transcribed segment stays eligible for echo comparison. A
    /// `Me:` chunk is only ever dropped as an echo of a `Them:` chunk it
    /// overlaps in time with, so we only need to retain very recent history.
    const DEDUP_RETAIN_SECS: f32 = 30.0;

    /// One transcribed, dictionary-corrected line, retained briefly for the
    /// echo-dedup time/word comparison.
    struct RecordedSeg {
        source: Source,
        start: f32,
        end: f32,
        text: String,
    }

    /// Line-buffered, crash-safe transcript writer. Every line is flushed to
    /// disk immediately (so a crash mid-meeting keeps everything written so
    /// far) and mirrored to stderr (so stdout stays clean for piping).
    struct TranscriptWriter {
        file: std::fs::File,
        /// Plain `Speaker: text` lines accumulated for the summarizer.
        body: Vec<String>,
    }

    impl TranscriptWriter {
        fn append_line(&mut self, offset_secs: f32, source: Source, text: &str) {
            let line = format!("[{}] {}: {}", format_offset(offset_secs as u64), source.label(), text);
            // Mirror live to stderr.
            eprintln!("{line}");
            // Persist immediately, line-buffered + fsync-lite via flush.
            let _ = writeln!(self.file, "{line}");
            let _ = self.file.flush();
            self.body.push(format!("{}: {}", source.label(), text));
        }

        fn append_raw(&mut self, text: &str) {
            let _ = writeln!(self.file, "{text}");
            let _ = self.file.flush();
        }
    }

    /// Runs a meeting session until `stop` is set (the CLI wires this to
    /// SIGINT). Returns the path of the transcript file written.
    pub fn run(title: Option<String>, out_dir: Option<PathBuf>, stop: Arc<AtomicBool>) -> Result<PathBuf> {
        let title = title.unwrap_or_else(|| "meeting".to_string());
        let out_dir = match out_dir {
            Some(d) => d,
            None => default_meetings_dir()?,
        };
        std::fs::create_dir_all(&out_dir)
            .with_context(|| format!("failed to create meetings directory {}", out_dir.display()))?;

        // Screen Recording (TCC) permission is required for system-audio
        // capture. Detect + prompt before we do anything else.
        syscapture::ensure_screen_permission();

        let now = chrono::Local::now();
        let path = unique_transcript_path(&out_dir, &now, &title);

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("failed to open transcript file {}", path.display()))?;
        writeln!(file, "# Meeting: {} — {}\n", title, now.format("%Y-%m-%d %H:%M"))
            .context("failed to write transcript header")?;
        file.flush().ok();

        eprintln!("[vzt-flow] meeting started -> {}", path.display());
        eprintln!("[vzt-flow] press Ctrl+C to stop and summarize. (Wear headphones for best speaker separation.)");

        // Shared Parakeet engine behind a mutex, serving both sources.
        let model_dir = parakeet_model_dir()?;
        let engine = Arc::new(Mutex::new(
            ParakeetTranscriber::load(&model_dir).context("failed to load Parakeet model")?,
        ));
        let dict = Arc::new(dictionary::load_or_seed().unwrap_or_default());
        let writer = Arc::new(Mutex::new(TranscriptWriter { file, body: Vec::new() }));

        // One transcription worker consumes chunks from both sources in FIFO
        // order; a single worker naturally serializes engine access and keeps
        // the echo-dedup history single-threaded (no lock needed for it).
        let (flush_tx, flush_rx) = mpsc::channel::<Chunk>();
        let worker = {
            let engine = engine.clone();
            let dict = dict.clone();
            let writer = writer.clone();
            std::thread::Builder::new()
                .name("vzt-flow-meeting-worker".into())
                .spawn(move || transcription_worker(flush_rx, engine, dict, writer))
                .context("failed to spawn transcription worker")?
        };

        // Microphone source on its own thread (cpal streams are !Send).
        let mic_flush = flush_tx.clone();
        let mic_stop = stop.clone();
        let mic = std::thread::Builder::new()
            .name("vzt-flow-meeting-mic".into())
            .spawn(move || {
                if let Err(e) = run_mic_source(mic_flush, mic_stop) {
                    eprintln!("[vzt-flow] microphone capture stopped: {e}");
                }
            })
            .context("failed to spawn microphone source")?;

        // System audio source on THIS thread: start the SCK stream (kept
        // alive locally) and drive its chunker until stop.
        let sys_result = run_system_source(&flush_tx, &stop);

        // Stop everything: signal, then join in dependency order.
        stop.store(true, Ordering::SeqCst);
        let _ = mic.join();
        // Dropping every sender lets the worker see the channel disconnect and
        // drain the last queued chunks before exiting.
        drop(flush_tx);
        let _ = worker.join();

        if let Err(e) = sys_result {
            eprintln!(
                "[vzt-flow] system-audio capture error: {e}\n\
                 If this is a permission error, grant Screen Recording to your terminal in\n\
                 System Settings › Privacy & Security › Screen Recording, then re-run."
            );
        }

        // Summarize (post-meeting latency is fine; no deadline race).
        summarize_into_file(&writer, &path);

        eprintln!("[vzt-flow] meeting saved -> {}", path.display());
        Ok(path)
    }

    /// Builds `<out>/<date>-<slug>.md`, adding a `-HHMMSS` suffix if that path
    /// already exists so a same-day, same-title meeting never clobbers an
    /// earlier one.
    fn unique_transcript_path(out_dir: &std::path::Path, now: &chrono::DateTime<chrono::Local>, title: &str) -> PathBuf {
        let date = now.format("%Y-%m-%d");
        let slug = slug_title(title);
        let base = out_dir.join(format!("{date}-{slug}.md"));
        if !base.exists() {
            return base;
        }
        out_dir.join(format!("{date}-{slug}-{}.md", now.format("%H%M%S")))
    }

    /// The single transcription worker: for each chunk, resample to 16 kHz,
    /// transcribe on the shared engine, dictionary-correct, apply echo dedup,
    /// and append the surviving line.
    fn transcription_worker(
        flush_rx: mpsc::Receiver<Chunk>,
        engine: Arc<Mutex<ParakeetTranscriber>>,
        dict: Arc<Vec<dictionary::DictionaryTerm>>,
        writer: Arc<Mutex<TranscriptWriter>>,
    ) {
        let mut recent: VecDeque<RecordedSeg> = VecDeque::new();

        while let Ok(chunk) = flush_rx.recv() {
            if !chunk.has_speech || chunk.samples.is_empty() {
                continue; // dead-air chunk flushed by the 30s cap — skip it
            }
            let start = chunk.start_offset;
            let end = chunk.end_offset();

            // Resample native -> 16 kHz for the engine.
            let samples = crate::audio::resample_linear(
                &chunk.samples,
                chunk.sample_rate,
                crate::audio::TARGET_SAMPLE_RATE,
            );

            let text = {
                let mut guard = match engine.lock() {
                    Ok(g) => g,
                    Err(p) => p.into_inner(),
                };
                match guard.transcribe(&samples) {
                    Ok(t) => t.text.trim().to_string(),
                    Err(e) => {
                        eprintln!("[vzt-flow] transcription error: {e}");
                        continue;
                    }
                }
            };
            if text.is_empty() {
                continue;
            }
            let corrected = dictionary::correct(&text, &dict);

            // Prune history older than the dedup window relative to this chunk.
            while let Some(front) = recent.front() {
                if front.end < start - DEDUP_RETAIN_SECS {
                    recent.pop_front();
                } else {
                    break;
                }
            }

            // Echo dedup: drop a Me line that time-overlaps a recent Them line
            // and is textually near-identical (the no-headphones case).
            if chunk.source == Source::Me {
                let echo = recent.iter().any(|seg| {
                    seg.source == Source::Them
                        && time_overlaps(start, end, seg.start, seg.end)
                        && is_echo(&corrected, &seg.text, DEFAULT_ECHO_THRESHOLD)
                });
                if echo {
                    eprintln!("[vzt-flow] dropped echo (Me overlapped Them): {corrected}");
                    continue;
                }
            }

            if let Ok(mut w) = writer.lock() {
                w.append_line(start, chunk.source, &corrected);
            }
            recent.push_back(RecordedSeg { source: chunk.source, start, end, text: corrected });
        }
    }

    /// Microphone capture loop: opens a cpal input stream, feeds a chunker at
    /// the device's native rate, and forwards `Me` chunks. Runs until `stop`.
    fn run_mic_source(flush_tx: mpsc::Sender<Chunk>, stop: Arc<AtomicBool>) -> Result<()> {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
        use cpal::{SampleFormat, StreamConfig};

        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .context("no default input (microphone) device found")?;
        let config = device.default_input_config().context("failed to read default input config")?;
        let sample_format = config.sample_format();
        let stream_config: StreamConfig = config.into();
        let in_rate = stream_config.sample_rate.0;
        let in_channels = stream_config.channels as usize;

        let (data_tx, data_rx) = mpsc::channel::<Vec<f32>>();
        let err_fn = |err| eprintln!("[vzt-flow] mic stream error: {err}");
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
                    let _ = data_tx.send(data.iter().map(|&s| s as f32 / i16::MAX as f32).collect());
                },
                err_fn,
                None,
            ),
            SampleFormat::U16 => device.build_input_stream(
                &stream_config,
                move |data: &[u16], _| {
                    let _ = data_tx.send(
                        data.iter()
                            .map(|&s| (s as f32 - u16::MAX as f32 / 2.0) / (u16::MAX as f32 / 2.0))
                            .collect(),
                    );
                },
                err_fn,
                None,
            ),
            other => anyhow::bail!("unsupported mic sample format: {other:?}"),
        }
        .context("failed to build mic input stream")?;
        stream.play().context("failed to start mic stream")?;

        let mut chunker = StreamingChunker::new(Source::Me, in_rate);
        let mut stats = SourceStats::new("mic");
        while !stop.load(Ordering::SeqCst) {
            match data_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(block) => {
                    let mono = downmix(&block, in_channels);
                    stats.observe(&mono);
                    for chunk in chunker.push(&mono) {
                        if flush_tx.send(chunk).is_err() {
                            return Ok(()); // worker gone
                        }
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        drop(stream);
        if let Some(tail) = chunker.flush() {
            let _ = flush_tx.send(tail);
        }
        stats.report(in_rate);
        Ok(())
    }

    /// System-audio capture loop: starts a ScreenCaptureKit audio-only stream,
    /// feeds a chunker at the capture rate, and forwards `Them` chunks. Runs on
    /// the calling thread until `stop` (the SCK stream is kept alive here and
    /// never crosses a thread boundary).
    fn run_system_source(flush_tx: &mpsc::Sender<Chunk>, stop: &Arc<AtomicBool>) -> Result<()> {
        let capture = syscapture::SystemAudioCapture::start()
            .context("failed to start ScreenCaptureKit system-audio capture")?;
        let rate = capture.sample_rate();
        let mut chunker = StreamingChunker::new(Source::Them, rate);
        let mut stats = SourceStats::new("system (SCK)");

        while !stop.load(Ordering::SeqCst) {
            match capture.recv_timeout(Duration::from_millis(100)) {
                Ok(mono) => {
                    stats.observe(&mono);
                    for chunk in chunker.push(&mono) {
                        if flush_tx.send(chunk).is_err() {
                            break;
                        }
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        // Flush the tail before the stream is torn down on drop.
        if let Some(tail) = chunker.flush() {
            let _ = flush_tx.send(tail);
        }
        stats.report(rate);
        capture.stop();
        Ok(())
    }

    /// Per-source capture diagnostics: how much audio actually arrived and how
    /// loud it was. Printed once when a source stops so a silent/blocked
    /// capture (e.g. Screen Recording permission denied but not errored) is
    /// visible rather than mistaken for a quiet meeting.
    struct SourceStats {
        label: &'static str,
        blocks: u64,
        samples: u64,
        peak: f32,
    }

    impl SourceStats {
        fn new(label: &'static str) -> Self {
            Self { label, blocks: 0, samples: 0, peak: 0.0 }
        }
        fn observe(&mut self, mono: &[f32]) {
            self.blocks += 1;
            self.samples += mono.len() as u64;
            let peak = mono.iter().fold(0.0f32, |m, s| m.max(s.abs()));
            if peak > self.peak {
                self.peak = peak;
            }
        }
        fn report(&self, rate: u32) {
            let secs = self.samples as f64 / rate.max(1) as f64;
            eprintln!(
                "[vzt-flow] {} source: {} blocks, {:.1}s audio, peak amplitude {:.4}{}",
                self.label,
                self.blocks,
                secs,
                self.peak,
                if self.peak < 0.001 { " (SILENT — nothing usable captured)" } else { "" }
            );
        }
    }

    /// Downmixes interleaved `channels`-channel audio to mono by averaging.
    fn downmix(samples: &[f32], channels: usize) -> Vec<f32> {
        if channels <= 1 {
            return samples.to_vec();
        }
        samples.chunks(channels).map(|f| f.iter().sum::<f32>() / f.len() as f32).collect()
    }

    /// Loads the cleanup model and appends `## Summary` + `## Action items` to
    /// the transcript. Never crashes the meeting: any failure (missing model,
    /// generation error) is logged and the transcript is left as-is.
    fn summarize_into_file(writer: &Arc<Mutex<TranscriptWriter>>, path: &std::path::Path) {
        let body = {
            let guard = match writer.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            guard.body.join("\n")
        };
        if body.trim().is_empty() {
            eprintln!("[vzt-flow] nothing was transcribed; skipping summary");
            return;
        }

        let (input, truncated) = super::transcriber::truncate_for_summary(&body, SUMMARY_MAX_CHARS);
        eprintln!("[vzt-flow] generating summary (this can take 10-60s)...");

        let model_path = match cleanup_model_path() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[vzt-flow] summary skipped (cleanup model path error: {e})");
                return;
            }
        };
        let provider = match LlamaCleanupProvider::load(&model_path) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[vzt-flow] summary skipped (cleanup model unavailable: {e})");
                return;
            }
        };

        let cancel = AtomicBool::new(false);
        let summary = match provider.summarize(&input, &cancel) {
            Ok(s) if !s.trim().is_empty() => s,
            Ok(_) => {
                eprintln!("[vzt-flow] summary generation produced no text; leaving transcript as-is");
                return;
            }
            Err(e) => {
                eprintln!("[vzt-flow] summary generation failed: {e}");
                return;
            }
        };

        if let Ok(mut w) = writer.lock() {
            w.append_raw("");
            if truncated {
                w.append_raw("_(summary of final portion)_");
                w.append_raw("");
            }
            w.append_raw(summary.trim());
        }
        let _ = path; // path retained for symmetry / future use
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_is_filesystem_safe() {
        assert_eq!(slug_title("Weekly Sync"), "weekly-sync");
        assert_eq!(slug_title("Q3 Planning: Roadmap!!"), "q3-planning-roadmap");
        assert_eq!(slug_title("  trailing/leading  "), "trailing-leading");
        assert_eq!(slug_title("***"), "meeting");
        assert_eq!(slug_title(""), "meeting");
    }

    #[test]
    fn parse_timestamp_accepts_valid_and_rejects_garbage() {
        assert_eq!(parse_leading_timestamp("[00:03:12] Them: hi"), Some("00:03:12".to_string()));
        assert_eq!(parse_leading_timestamp("[01:02:03] Me: yo"), Some("01:02:03".to_string()));
        assert_eq!(parse_leading_timestamp("no timestamp here"), None);
        assert_eq!(parse_leading_timestamp("[3:2:1] bad"), None);
        assert_eq!(parse_leading_timestamp("# Meeting: X — 2026"), None);
    }

    #[test]
    fn summarize_file_parses_header_and_duration() {
        let dir = std::env::temp_dir().join(format!("vzt-meeting-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("2026-07-08-demo.md");
        fs::write(
            &path,
            "# Meeting: Demo Call — 2026-07-08 20:15\n\n[00:00:03] Them: hello\n[00:04:20] Me: bye\n",
        )
        .unwrap();

        let s = summarize_file(&path);
        assert_eq!(s.title, "Demo Call");
        assert_eq!(s.datetime, "2026-07-08 20:15");
        assert_eq!(s.duration, Some("00:04:20".to_string()));
        assert!(s.size_bytes > 0);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn list_meetings_on_missing_dir_is_empty() {
        let dir = std::env::temp_dir().join("vzt-meeting-does-not-exist-xyz");
        assert!(list_meetings(&dir, 10).unwrap().is_empty());
    }
}
