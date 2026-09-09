//! Meeting mode: live, fully-local transcription of a video call by capturing
//! both the system/app audio (the other participants, via ScreenCaptureKit)
//! and the local microphone (the user, via cpal) concurrently, streaming each
//! to a shared Parakeet engine, and writing a timestamped, speaker-labelled
//! transcript that is summarized on stop.
//!
//! Two entry points, one engine:
//!
//! * [`run`] / [`start`] — the legacy shapes the CLI and the desktop tray have
//!   always used. Thin wrappers over the `_with` forms.
//! * [`run_with`] / [`start_with`] — the full session: live line/state/tip
//!   observers, a typed [`MeetingOutcome`], a notes sidecar merged on stop, a
//!   hierarchical summary and a PDF export. [`start_with`] reserves the
//!   transcript file on the **caller's** thread, so the session id and the
//!   notes path are known before any capture starts.
//!
//! Only the pure sub-modules ([`dedup`], [`transcriber`], [`events`],
//! [`notes`], [`summary`], [`pdf`], [`interview`]), the listing/path helpers
//! and the finalize path below compile on every platform. Live capture
//! depends on ScreenCaptureKit and is macOS-only; off macOS [`run_with`]
//! returns a clear error.

pub mod dedup;
pub mod detect;
pub mod events;
pub mod interview;
pub mod notes;
pub mod pdf;
pub mod pipeline;
pub mod summary;
pub mod transcriber;

pub use events::*;

#[cfg(target_os = "macos")]
mod syscapture;

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use anyhow::{Context, Result};

use crate::llm::TextGenerator;

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
///
/// Only `.md` files count, which is why the live-notes sidecar is
/// `.notes.txt` (see [`notes`]) — a markdown sidecar would show up here as a
/// phantom meeting.
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
            // "<title> — <datetime>" (em dash separator written by
            // reserve_transcript_path).
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
// Session options and observers.
// ---------------------------------------------------------------------------

/// Called once, synchronously, when a session's files are reserved.
pub type StartedObserver = Arc<dyn Fn(&SessionStarted) + Send + Sync>;
/// Called for every recognized transcript line, *after* it is on disk.
pub type LineObserver = Arc<dyn Fn(&TranscriptLine) + Send + Sync>;
/// Called on every lifecycle transition (`Recording` → … → `Completed`).
pub type StateObserver = Arc<dyn Fn(&SessionState) + Send + Sync>;
/// Called for every interview-coaching tip.
pub type TipObserver = Arc<dyn Fn(&InterviewTip) + Send + Sync>;

/// Everything a session needs beyond "where do I write". Every field has a
/// `Default`, so the legacy [`run`]/[`start`] shapes are `MeetingOptions {
/// title, out_dir, ..Default::default() }`.
///
/// **Observers run on session threads**, never the main thread: `on_line`
/// and `on_state` on the transcription worker / session thread, `on_tip` on
/// the coach thread. A UI observer must marshal to the main thread itself
/// (CLAUDE.md gotcha (h)). A panicking observer is contained — it costs its
/// own notification, never the session.
///
/// **Line mirroring is still done by flow-core**: every line is echoed to
/// stderr by the transcript writer exactly as before, so an `on_line` that
/// also prints would double up.
#[derive(Default)]
pub struct MeetingOptions {
    /// Meeting title; seeds the header, the filename slug and the PDF name.
    pub title: Option<String>,
    /// Output directory; defaults to [`default_meetings_dir`].
    pub out_dir: Option<PathBuf>,
    /// Pre-reserved transcript path (set by [`start_with`]). When `None` the
    /// session reserves its own on the session thread.
    pub transcript_path: Option<PathBuf>,
    pub on_started: Option<StartedObserver>,
    pub on_line: Option<LineObserver>,
    pub on_state: Option<StateObserver>,
    /// Set this to enable the interview coach; `None` means no coach thread
    /// is spawned at all.
    pub on_tip: Option<TipObserver>,
    /// The process-wide resident model. `None` makes the session build its
    /// own [`crate::llm::LocalGenerator`], which is what the CLI wants; the
    /// desktop passes its [`crate::llm::ManagerGenerator`] so there is still
    /// exactly one model in the process.
    pub generator: Option<Arc<dyn TextGenerator>>,
    /// Live-togglable mid-meeting: gates coaching and the tighter `Them`
    /// silence hold.
    pub interview: Arc<AtomicBool>,
    /// Candidate context for the coach; `None` reads
    /// [`interview::load_context`].
    pub interview_context: Option<String>,
    /// `None` (or `enabled: false`) means no PDF is exported.
    pub pdf: Option<pdf::PdfOptions>,
    pub summary: summary::SummaryOptions,
}

/// Calls one observer with a panic guard.
///
/// The observer belongs to the caller (a Tauri `emit_to`, a CLI print), so a
/// panic in it must not take the session thread with it — whatever it is
/// being told about is already persisted by the time it runs.
fn notify<T>(observer: Option<&Arc<dyn Fn(&T) + Send + Sync>>, value: &T) {
    if let Some(cb) = observer {
        let cb = Arc::clone(cb);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || cb(value)));
    }
}

/// The session id for a reserved transcript: its file **stem**, e.g.
/// `2026-09-08-zoom-meeting-141203`. Immutable for the life of the session.
fn session_id_for(transcript: &Path) -> String {
    transcript
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "meeting".to_string())
}

/// How many name candidates `reserve_transcript_path` tries before giving up.
const MAX_RESERVE_ATTEMPTS: usize = 1_000;

/// Reserves `<out>/<date>-<slug>.md`, falling back to `-HHMMSS`, then
/// `-HHMMSS-2`, `-3`, … until one name is **created** rather than merely
/// found absent, and writes the `# Meeting:` header into it.
///
/// The creation is `create_new(true)`, so two sessions started in the same
/// second (or in two processes) can never be handed the same path — the
/// exists-then-open shape this replaces could hand both the same name.
///
/// Called on the caller's thread by [`start_with`], which is what makes the
/// session id and the notes path available synchronously, before any capture
/// thread exists.
pub fn reserve_transcript_path(
    out_dir: &Path,
    now: &chrono::DateTime<chrono::Local>,
    title: &str,
) -> Result<PathBuf> {
    fs::create_dir_all(out_dir)
        .with_context(|| format!("failed to create meetings directory {}", out_dir.display()))?;

    let date = now.format("%Y-%m-%d");
    let time = now.format("%H%M%S");
    let slug = slug_title(title);

    for attempt in 0..MAX_RESERVE_ATTEMPTS {
        let name = match attempt {
            0 => format!("{date}-{slug}.md"),
            1 => format!("{date}-{slug}-{time}.md"),
            n => format!("{date}-{slug}-{time}-{n}.md"),
        };
        let path = out_dir.join(name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                writeln!(file, "# Meeting: {} — {}\n", title, now.format("%Y-%m-%d %H:%M"))
                    .with_context(|| format!("failed to write transcript header to {}", path.display()))?;
                file.flush().ok();
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("failed to create transcript file {}", path.display()))
            }
        }
    }
    anyhow::bail!(
        "could not reserve a transcript name in {} after {MAX_RESERVE_ATTEMPTS} attempts",
        out_dir.display()
    )
}

// ---------------------------------------------------------------------------
// Transcript writer (persist first, notify second).
// ---------------------------------------------------------------------------

/// Line-buffered, crash-safe transcript writer. Every line is flushed to disk
/// immediately (so a crash mid-meeting keeps everything written so far),
/// mirrored to stderr (so stdout stays clean for piping), and only *then*
/// fanned out to the live observers.
///
/// That order is the crash-safety property: an observer panic costs the
/// notification, never the line.
#[cfg(any(target_os = "macos", test))]
struct TranscriptWriter {
    file: fs::File,
    /// Plain `Speaker: text` lines accumulated for the summarizer.
    body: Vec<String>,
    /// Sequence numbering + live observers, shared with nothing else.
    fanout: LineFanout,
}

#[cfg(any(target_os = "macos", test))]
impl TranscriptWriter {
    fn new(file: fs::File, fanout: LineFanout) -> Self {
        Self { file, body: Vec::new(), fanout }
    }

    /// Persists one recognized line, then notifies the observers.
    ///
    /// Returns the fanned-out [`TranscriptLine`], or `None` when an observer
    /// panicked — the line is on disk either way, but its sequence number
    /// reached only the observers registered before the one that panicked,
    /// so the caller must not treat it as fully delivered (the interview
    /// coach skips it rather than coaching on a half-delivered line).
    fn append_line(
        &mut self,
        offset_secs: f32,
        source: transcriber::Source,
        text: &str,
    ) -> Option<TranscriptLine> {
        let line = format!(
            "[{}] {}: {}",
            transcriber::format_offset(offset_secs as u64),
            source.label(),
            text
        );
        // Mirror live to stderr.
        eprintln!("{line}");
        // Persist immediately, line-buffered + fsync-lite via flush.
        let _ = writeln!(self.file, "{line}");
        let _ = self.file.flush();
        self.body.push(format!("{}: {}", source.label(), text));

        // Notified only after the line is durable. `LineFanout` calls its
        // observers inline, so this catches a panicking observer here rather
        // than letting it unwind the transcription worker and silently end
        // the meeting (cf. CLAUDE.md gotcha (i)).
        let fanout = &self.fanout;
        let text = text.to_string();
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            fanout.push(source, offset_secs, text)
        }))
        .ok()
    }
}

// ---------------------------------------------------------------------------
// Decision routing (pipeline -> hold buffer -> writer -> coach).
// ---------------------------------------------------------------------------

/// Writes every line the [`pipeline::LineBuffer`] released, in the order it
/// released them (already `start_offset`-ordered), and feeds each one to the
/// interview coach.
///
/// `chunk_closed_at` is the instant the chunk that triggered the release was
/// dequeued, so a tip's measured latency still starts when the speaker
/// stopped talking rather than when the buffer happened to let the line go.
#[cfg(any(target_os = "macos", test))]
fn write_released(
    writer: &std::sync::Mutex<TranscriptWriter>,
    coach: &std::sync::Mutex<Option<interview::Coach>>,
    released: Vec<(f32, transcriber::Source, String)>,
    chunk_closed_at: std::time::Instant,
) {
    for (start, source, text) in released {
        // The writer lock is released before the coach is touched: an
        // observer runs inline inside `append_line`, so holding both would
        // widen the window in which a slow observer blocks the other source's
        // lines (cf. `TranscriptWriter`'s flush-then-fanout contract).
        let line = {
            let mut w = writer.lock().unwrap_or_else(|p| p.into_inner());
            w.append_line(start, source, &text)
        };
        if let Some(line) = line {
            let guard = coach.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(coach) = guard.as_ref() {
                coach.observe(&line, chunk_closed_at);
            }
        }
    }
}

/// Routes one [`pipeline::Decision`] through the hold buffer and writes
/// whatever comes back out.
///
/// Every decision is pushed — including `Skipped`/`Failed`/`DroppedEcho`,
/// which carry no line but do advance meeting time and so can make an
/// already-held line due. An ordinary `Them` line is returned by
/// `push_chunk` on the same call, so it reaches `append_line` (and therefore
/// the coach and the live notepad) with no added latency; only `Me` lines and
/// a `Them` line that ended on a hard cap cut are ever held.
///
/// Factored out of the macOS-only transcription worker so the
/// decision -> buffer -> writer path is unit-testable with no capture engine.
#[cfg(any(target_os = "macos", test))]
fn route_decision(
    buffer: &mut pipeline::LineBuffer,
    writer: &std::sync::Mutex<TranscriptWriter>,
    coach: &std::sync::Mutex<Option<interview::Coach>>,
    decision: pipeline::Decision,
    chunk: &transcriber::Chunk,
    chunk_closed_at: std::time::Instant,
) {
    match &decision {
        pipeline::Decision::DroppedEcho(corrected) => {
            eprintln!("[vzt-flow] dropped echo (Me overlapped Them): {corrected}");
        }
        pipeline::Decision::Failed(e) => {
            eprintln!("[vzt-flow] transcription error: {e}");
        }
        _ => {}
    }
    // A held `Me` line vetoed by a `Them` line that finished transcribing
    // later never reaches the writer, so the buffer's counter is the only
    // place that late drop is visible. Log the delta, not the total.
    let dropped_before = buffer.dropped_echo_count();
    let released = buffer.push_chunk(decision, chunk);
    let late = buffer.dropped_echo_count() - dropped_before;
    if late > 0 {
        eprintln!("[vzt-flow] dropped echo ({late} held Me line(s) vetoed by a late Them line)");
    }
    write_released(writer, coach, released, chunk_closed_at);
}

// ---------------------------------------------------------------------------
// Finalize: stop -> merge notes -> summarize -> PDF.
// ---------------------------------------------------------------------------

/// Idle timeout for a session-owned [`crate::llm::LocalGenerator`]. Long
/// enough that the coach and the end-of-meeting summary share one load, short
/// enough that a CLI session doesn't sit on a resident model afterwards.
#[cfg(any(target_os = "macos", test))]
const GENERATOR_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Resolves the generator for a session: the caller's if it supplied one,
/// else a lazily-loading local one, else nothing at all.
///
/// Returns the note to append to the summary's coverage line when there is no
/// model to summarize with.
#[cfg(any(target_os = "macos", test))]
fn build_generator(opts: &MeetingOptions) -> (Arc<dyn TextGenerator>, Option<String>) {
    if let Some(g) = &opts.generator {
        return (Arc::clone(g), None);
    }
    match crate::models::cleanup_model_path() {
        Ok(p) if p.exists() => (
            Arc::new(crate::llm::LocalGenerator::new(p, GENERATOR_IDLE_TIMEOUT)),
            None,
        ),
        _ => (
            Arc::new(crate::llm::NullGenerator),
            Some("No cleanup model is installed, so no summary was generated.".to_string()),
        ),
    }
}

/// Everything [`finalize`] needs that isn't produced by the teardown itself.
#[cfg(any(target_os = "macos", test))]
struct FinalizeRequest<'a> {
    session_id: &'a str,
    title: &'a str,
    started_at: chrono::DateTime<chrono::Local>,
    transcript: &'a Path,
    notes_path: &'a Path,
    generator: &'a dyn TextGenerator,
    summary: &'a summary::SummaryOptions,
    /// `None` (or `enabled: false`) skips the PDF step entirely.
    pdf: Option<&'a pdf::PdfOptions>,
    on_state: Option<&'a StateObserver>,
    /// Appended to the coverage line (e.g. "no cleanup model installed").
    model_note: Option<&'a str>,
}

/// The stop → finalize state machine, factored out of the live session so it
/// is testable with no microphone, no ScreenCaptureKit and no model.
///
/// `stop_capture` performs the teardown — flush tails, join the capture and
/// worker threads, shut the coach down — and returns the transcript body it
/// collected. It runs *after* `Stopping` is emitted, so the UI reflects the
/// wait for the final chunk rather than still claiming to be recording.
///
/// Nothing here fails the meeting: the transcript is already on disk, so a
/// missing model, an unusable notes sidecar or an unwritable PDF directory is
/// recorded in the [`MeetingOutcome`] and the session still completes.
/// `Failed` is emitted only when the transcript itself could not be written.
#[cfg(any(target_os = "macos", test))]
fn finalize(req: FinalizeRequest<'_>, stop_capture: impl FnOnce() -> Vec<String>) -> MeetingOutcome {
    let on_state = req.on_state;
    let emit = move |state: SessionState| notify(on_state, &state);

    emit(SessionState::Stopping);
    let body_lines = stop_capture();

    let mut outcome = MeetingOutcome {
        session_id: req.session_id.to_string(),
        transcript: req.transcript.to_path_buf(),
        pdf: None,
        pdf_error: None,
        notes_merged: false,
        // flow-core has no notes revision counter — the notepad owner (the
        // desktop `SessionSlot`) is the only thing that knows which revision
        // it last saved, so it overwrites this with its own `notes_rev`.
        notes_rev_used: 0,
        summary_sections: 0,
        summary_complete: false,
    };

    // --- notes -------------------------------------------------------------
    // Read once, here: this snapshot is both what gets merged into the
    // transcript and what the summarizer sees, so the two can't disagree.
    emit(SessionState::Finalizing { step: "merging notes".to_string() });
    let notes = notes::read(req.notes_path);
    match notes::merge_into_transcript(req.transcript, &notes) {
        Ok(merged) => outcome.notes_merged = merged,
        Err(e) => eprintln!("[vzt-flow] could not merge meeting notes: {e}"),
    }

    // --- summary -----------------------------------------------------------
    // The merge rewrote the transcript via rename, so every write from here
    // on re-opens it by path; any handle taken before the merge points at the
    // replaced inode.
    let mut summary_input = body_lines;
    summary_input.extend(notes::as_summary_lines(&notes));
    let body = summary_input.join("\n");

    let mut summary_md: Option<String> = None;
    let mut coverage: Option<String> = None;
    let mut transcript_error: Option<String> = None;

    if body.trim().is_empty() {
        eprintln!("[vzt-flow] nothing was transcribed; skipping summary");
    } else {
        let progress = |i: usize, n: usize| {
            emit(SessionState::Finalizing { step: format!("summarizing {i}/{n}") })
        };
        let result = summary::summarize(&body, req.generator, req.summary, &progress);

        let markdown = result.markdown.trim().to_string();
        outcome.summary_sections = result.sections;
        outcome.summary_complete = result.complete && !markdown.is_empty();

        let mut note = result.coverage_note.clone();
        if let Some(extra) = req.model_note {
            note.push(' ');
            note.push_str(extra);
        }

        // An empty summary gets a line only when we can say *why* there
        // isn't one — a bare coverage note would claim coverage of a summary
        // that does not exist.
        let appended = if markdown.is_empty() {
            eprintln!("[vzt-flow] summary generation produced no text; leaving transcript as-is");
            match req.model_note {
                Some(reason) => Some(("".to_string(), reason.to_string())),
                None => None,
            }
        } else {
            Some((markdown, note))
        };

        if let Some((markdown, note)) = appended {
            match append_summary(req.transcript, &markdown, &note) {
                Ok(()) => {
                    if !markdown.is_empty() {
                        summary_md = Some(markdown);
                    }
                    coverage = Some(note);
                }
                Err(e) => {
                    eprintln!("[vzt-flow] could not append the summary to the transcript: {e}");
                    outcome.summary_complete = false;
                    transcript_error = Some(e.to_string());
                }
            }
        }
    }

    // --- pdf ---------------------------------------------------------------
    if let Some(pdf_opts) = req.pdf.filter(|p| p.enabled) {
        emit(SessionState::Finalizing { step: "writing pdf".to_string() });
        let (transcript_lines, duration) = transcript_lines_and_duration(req.transcript);
        let doc = pdf::MeetingDoc {
            title: req.title.to_string(),
            date_line: req.started_at.format("%Y-%m-%d %H:%M").to_string(),
            duration,
            summary_md,
            notes: (!notes.trim().is_empty()).then(|| notes.clone()),
            transcript_lines,
            coverage_note: coverage,
            source_path: req.transcript.to_path_buf(),
        };
        let stem = pdf::pdf_file_stem(req.title, &req.started_at);
        let dir = pdf_opts.dir.clone();
        // The renderer is pure but total: a panic in it would otherwise
        // unwind a session whose transcript is already complete on disk.
        let written = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let bytes = pdf::render(&doc);
            pdf::write_atomic(&bytes, &dir, &stem)
        }));
        match written {
            Ok(Ok(path)) => outcome.pdf = Some(path),
            Ok(Err(e)) => {
                eprintln!("[vzt-flow] PDF export failed: {e}");
                outcome.pdf_error = Some(e.to_string());
            }
            Err(_) => {
                eprintln!("[vzt-flow] PDF export panicked");
                outcome.pdf_error = Some("the PDF renderer panicked".to_string());
            }
        }
    }

    match transcript_error {
        Some(msg) => emit(SessionState::Failed(msg)),
        None => emit(SessionState::Completed),
    }
    outcome
}

/// Appends the summary markdown and its coverage line to the transcript.
/// An empty summary still gets its coverage line — that line is how a reader
/// learns *why* there is no summary.
#[cfg(any(target_os = "macos", test))]
fn append_summary(transcript: &Path, markdown: &str, coverage_note: &str) -> Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(transcript)
        .with_context(|| format!("failed to open {}", transcript.display()))?;
    writeln!(file)?;
    if !markdown.is_empty() {
        writeln!(file, "{markdown}")?;
        writeln!(file)?;
    }
    writeln!(file, "_{coverage_note}_")?;
    file.flush()?;
    Ok(())
}

/// Reads back the timestamped transcript lines (for the PDF body) and the
/// meeting length (the last `[HH:MM:SS]` seen), reusing the same parse
/// `list_meetings` uses so the PDF and the listing can't disagree.
#[cfg(any(target_os = "macos", test))]
fn transcript_lines_and_duration(transcript: &Path) -> (Vec<String>, String) {
    let content = fs::read_to_string(transcript).unwrap_or_default();
    let mut lines = Vec::new();
    let mut duration = None;
    for line in content.lines() {
        if let Some(ts) = parse_leading_timestamp(line) {
            duration = Some(ts);
            lines.push(line.to_string());
        }
    }
    (lines, duration.unwrap_or_else(|| "00:00:00".to_string()))
}

// ---------------------------------------------------------------------------
// In-process session handle (used by the desktop app's tray integration).
// ---------------------------------------------------------------------------

/// A meeting session started in-process on a background thread.
///
/// The identity fields ([`Self::session_id`], [`Self::transcript_path`],
/// [`Self::notes_path`]) are known the moment [`start_with`] returns — the
/// transcript file is reserved on the caller's thread — so a notepad window
/// can bind to the session before the first line exists.
pub struct MeetingHandle {
    session_id: String,
    transcript: PathBuf,
    notes: PathBuf,
    interview: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<Result<MeetingOutcome>>>,
    /// Set when [`start`] could not even reserve a transcript. The legacy
    /// `start` has no `Result`, so the error is held here and surfaces from
    /// [`Self::stop`], where the caller already handles one.
    start_error: Option<anyhow::Error>,
}

/// Starts a meeting session with full options and returns a handle whose
/// identity is already resolved.
///
/// The transcript is reserved (and its header written) **on this thread**
/// before the session thread is spawned, and `on_started` fires here — so the
/// caller can open a notepad bound to `session_id` with no race against the
/// first transcript line.
pub fn start_with(mut opts: MeetingOptions) -> Result<MeetingHandle> {
    let title = opts.title.clone().unwrap_or_else(|| "meeting".to_string());
    let out_dir = match opts.out_dir.clone() {
        Some(d) => d,
        None => default_meetings_dir()?,
    };
    let now = chrono::Local::now();
    let transcript = match opts.transcript_path.clone() {
        Some(p) => p,
        None => reserve_transcript_path(&out_dir, &now, &title)?,
    };

    let session_id = session_id_for(&transcript);
    let notes = notes::notes_path_for(&transcript);

    opts.title = Some(title.clone());
    opts.out_dir = Some(out_dir);
    opts.transcript_path = Some(transcript.clone());

    notify(
        opts.on_started.as_ref(),
        &SessionStarted {
            session_id: session_id.clone(),
            title,
            started_at_ms: now.timestamp_millis(),
            transcript: transcript.clone(),
            notes: notes.clone(),
        },
    );

    let interview = Arc::clone(&opts.interview);
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let join = std::thread::Builder::new()
        .name("vzt-flow-meeting-session".into())
        .spawn(move || run_with(opts, thread_stop))
        .context("failed to spawn meeting session thread")?;

    Ok(MeetingHandle {
        session_id,
        transcript,
        notes,
        interview,
        stop,
        join: Some(join),
        start_error: None,
    })
}

/// Starts a meeting session on a background thread and returns a handle. The
/// session captures + transcribes until [`MeetingHandle::stop`] is called,
/// which also triggers the on-stop summary. `title`/`out_dir` mirror [`run`]'s
/// arguments (default title `"meeting"`, default dir
/// [`default_meetings_dir`]).
///
/// Thin wrapper over [`start_with`]: no observers, no PDF, no coach.
pub fn start(title: Option<String>, out_dir: Option<PathBuf>) -> MeetingHandle {
    let opts = MeetingOptions { title, out_dir, ..Default::default() };
    match start_with(opts) {
        Ok(handle) => handle,
        // The legacy signature can't return an error here; hand back a handle
        // that is already finished and reports it from `stop`.
        Err(e) => MeetingHandle {
            session_id: String::new(),
            transcript: PathBuf::new(),
            notes: PathBuf::new(),
            interview: Arc::new(AtomicBool::new(false)),
            stop: Arc::new(AtomicBool::new(true)),
            join: None,
            start_error: Some(e),
        },
    }
}

impl MeetingHandle {
    /// The session id: the transcript file's stem, immutable for the life of
    /// the session. Empty only for a handle whose start failed.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Where the transcript is being written.
    pub fn transcript_path(&self) -> &Path {
        &self.transcript
    }

    /// The `.notes.txt` sidecar for this session. It does not exist until
    /// something saves notes into it.
    pub fn notes_path(&self) -> &Path {
        &self.notes
    }

    /// The live interview-mode flag. Flipping it takes effect mid-meeting:
    /// it gates coaching and the tighter `Them` silence hold.
    pub fn interview_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.interview)
    }

    /// Signals the session to stop, blocks until it has flushed its tails,
    /// merged notes, summarized and exported, and returns the full outcome.
    ///
    /// **Blocking**: finalize can take 10-60s, so call this from a background
    /// thread, never a UI/coordinator thread.
    pub fn stop_detailed(mut self) -> Result<MeetingOutcome> {
        if let Some(e) = self.start_error.take() {
            return Err(e);
        }
        self.stop.store(true, Ordering::SeqCst);
        match self.join.take() {
            Some(j) => j
                .join()
                .map_err(|_| anyhow::anyhow!("meeting session thread panicked"))?,
            None => anyhow::bail!("meeting session already stopped"),
        }
    }

    /// Legacy shape: stops the session and returns just the transcript path.
    pub fn stop(self) -> Result<PathBuf> {
        self.stop_detailed().map(|outcome| outcome.transcript)
    }

    /// Whether the session thread is still running (has not returned/panicked).
    pub fn is_running(&self) -> bool {
        self.join.as_ref().map(|j| !j.is_finished()).unwrap_or(false)
    }
}

// ---------------------------------------------------------------------------
// Live session — macOS only (ScreenCaptureKit).
// ---------------------------------------------------------------------------

/// Runs a meeting session until `stop` is set (the CLI wires this to SIGINT).
/// Returns the path of the transcript file written.
///
/// Thin wrapper over [`run_with`] with no observers, no PDF and no coach.
pub fn run(title: Option<String>, out_dir: Option<PathBuf>, stop: Arc<AtomicBool>) -> Result<PathBuf> {
    let opts = MeetingOptions { title, out_dir, ..Default::default() };
    run_with(opts, stop).map(|outcome| outcome.transcript)
}

/// Off macOS there is no ScreenCaptureKit, so live capture is unavailable.
/// The listing/notes/summary/PDF paths above still work everywhere.
#[cfg(target_os = "linux")]
pub fn run_with(_opts: MeetingOptions, _stop: Arc<AtomicBool>) -> Result<MeetingOutcome> {
    anyhow::bail!(
        "meeting mode is not yet available on Linux (needs a PipeWire system-audio \
         capture backend — on the roadmap; macOS uses ScreenCaptureKit today)"
    )
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn run_with(_opts: MeetingOptions, _stop: Arc<AtomicBool>) -> Result<MeetingOutcome> {
    anyhow::bail!("meeting mode requires macOS (ScreenCaptureKit system-audio capture is macOS-only)")
}

#[cfg(target_os = "macos")]
pub use session::run_with;

#[cfg(target_os = "macos")]
mod session {
    //! The live meeting session: wires the two capture sources, the shared
    //! Parakeet engine, the transcript writer, echo dedup, the interview
    //! coach and the finalize path together.

    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result};

    use crate::dictionary;
    use crate::engine::{ParakeetTranscriber, Transcriber};
    use crate::models::parakeet_model_dir;

    use super::interview::{Coach, CoachConfig};
    use super::pipeline::{ChunkPipeline, LineBuffer, PipelineOptions};
    use super::syscapture;
    use super::transcriber::{Chunk, Source, StreamingChunker, SILENCE_HOLD_SECS};
    use super::{
        build_generator, default_meetings_dir, finalize, notes, notify, reserve_transcript_path,
        route_decision, session_id_for, write_released, FinalizeRequest, LineFanout,
        MeetingOptions, MeetingOutcome, SessionStarted, SessionState, TranscriptWriter,
    };

    /// Runs a full meeting session until `stop` is set.
    pub fn run_with(opts: MeetingOptions, stop: Arc<AtomicBool>) -> Result<MeetingOutcome> {
        let title = opts.title.clone().unwrap_or_else(|| "meeting".to_string());
        let out_dir = match opts.out_dir.clone() {
            Some(d) => d,
            None => default_meetings_dir()?,
        };

        // Screen Recording (TCC) permission is required for system-audio
        // capture. Detect + prompt before we do anything else.
        syscapture::ensure_screen_permission();

        let now = chrono::Local::now();
        // `start_with` reserves on the caller's thread and fires `on_started`
        // there; only a session that reserves its own path fires it here.
        let (path, reserved_here) = match opts.transcript_path.clone() {
            Some(p) => (p, false),
            None => (reserve_transcript_path(&out_dir, &now, &title)?, true),
        };
        let session_id = session_id_for(&path);
        let notes_path = notes::notes_path_for(&path);
        if reserved_here {
            notify(
                opts.on_started.as_ref(),
                &SessionStarted {
                    session_id: session_id.clone(),
                    title: title.clone(),
                    started_at_ms: now.timestamp_millis(),
                    transcript: path.clone(),
                    notes: notes_path.clone(),
                },
            );
        }

        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("failed to open transcript file {}", path.display()))?;

        eprintln!("[vzt-flow] meeting started -> {}", path.display());
        eprintln!("[vzt-flow] press Ctrl+C to stop and summarize. (Wear headphones for best speaker separation.)");

        let cfg = crate::config::Config::load().unwrap_or_default();

        // Shared Parakeet engine behind a mutex, serving both sources.
        let model_dir = parakeet_model_dir()?;
        let engine = Arc::new(Mutex::new(
            ParakeetTranscriber::load(&model_dir).context("failed to load Parakeet model")?,
        ));
        let dict = Arc::new(dictionary::load_or_seed().unwrap_or_default());

        let mut fanout = LineFanout::new(session_id.clone());
        if let Some(on_line) = opts.on_line.clone() {
            fanout.add_observer(on_line);
        }
        let writer = Arc::new(Mutex::new(TranscriptWriter::new(file, fanout)));

        // One generator for the whole session: the coach and the end-of-
        // meeting summary share it, so the process holds exactly one resident
        // model. Dropping the last clone (at the end of this function) joins
        // its manager thread — never detached (gotcha (e)).
        let (generator, model_note) = build_generator(&opts);

        // The coach lives behind a mutex rather than an `Arc<Coach>` so
        // ownership can be taken back for `shutdown` (which consumes it)
        // without depending on a refcount reaching one.
        let coach: Arc<Mutex<Option<Coach>>> = Arc::new(Mutex::new(None));
        if let Some(on_tip) = opts.on_tip.clone() {
            let context = match opts.interview_context.clone() {
                Some(c) => c,
                None => super::interview::load_context(),
            };
            let coach_cfg = CoachConfig {
                timeout_ms: cfg.interview_tip_timeout_ms,
                context_max_chars: cfg.interview_context_max_chars,
                ..CoachConfig::default()
            };
            let spawned = Coach::spawn(
                session_id.clone(),
                Arc::clone(&generator),
                context,
                Arc::clone(&opts.interview),
                coach_cfg,
                on_tip,
            );
            *coach.lock().unwrap_or_else(|p| p.into_inner()) = Some(spawned);
        }

        notify(opts.on_state.as_ref(), &SessionState::Recording);

        // One transcription worker consumes chunks from both sources in FIFO
        // order; a single worker naturally serializes engine access and keeps
        // the echo-dedup history single-threaded (no lock needed for it).
        let (flush_tx, flush_rx) = mpsc::channel::<Chunk>();
        let worker = {
            let engine = Arc::clone(&engine);
            let dict = Arc::clone(&dict);
            let writer = Arc::clone(&writer);
            let coach = Arc::clone(&coach);
            std::thread::Builder::new()
                .name("vzt-flow-meeting-worker".into())
                .spawn(move || transcription_worker(flush_rx, engine, dict, writer, coach))
                .context("failed to spawn transcription worker")?
        };

        // Microphone source on its own thread (cpal streams are !Send).
        let mic_flush = flush_tx.clone();
        let mic_stop = Arc::clone(&stop);
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
        let sys_result = run_system_source(
            &flush_tx,
            &stop,
            &opts.interview,
            cfg.interview_silence_hold_secs as f32,
        );
        if let Err(e) = sys_result {
            eprintln!(
                "[vzt-flow] system-audio capture error: {e}\n\
                 If this is a permission error, grant Screen Recording to your terminal in\n\
                 System Settings › Privacy & Security › Screen Recording, then re-run."
            );
        }

        let outcome = finalize(
            FinalizeRequest {
                session_id: &session_id,
                title: &title,
                started_at: now,
                transcript: &path,
                notes_path: &notes_path,
                generator: generator.as_ref(),
                summary: &opts.summary,
                pdf: opts.pdf.as_ref(),
                on_state: opts.on_state.as_ref(),
                model_note: model_note.as_deref(),
            },
            // Teardown, in dependency order, after `Stopping` is emitted.
            move || {
                stop.store(true, Ordering::SeqCst);
                let _ = mic.join();
                // Dropping every sender lets the worker see the channel
                // disconnect and drain the last queued chunks before exiting.
                drop(flush_tx);
                let _ = worker.join();
                // The worker is gone, so nothing can feed the coach any more;
                // cancel + join it before the summary competes for the model.
                let coach = coach.lock().unwrap_or_else(|p| p.into_inner()).take();
                if let Some(coach) = coach {
                    coach.shutdown();
                }
                let body = {
                    let guard = writer.lock().unwrap_or_else(|p| p.into_inner());
                    guard.body.clone()
                };
                // Close the append handle before finalize merges notes: the
                // merge replaces the file by rename, so this handle would
                // otherwise write into the replaced inode.
                drop(writer);
                body
            },
        );

        eprintln!("[vzt-flow] meeting saved -> {}", path.display());
        if let Some(pdf) = &outcome.pdf {
            eprintln!("[vzt-flow] meeting PDF -> {}", pdf.display());
        }
        Ok(outcome)
    }

    /// The single transcription worker: hands each chunk to the shared
    /// [`ChunkPipeline`] (resample, transcribe on the shared engine,
    /// dictionary-correct, echo dedup), routes the decision through the
    /// [`LineBuffer`] hold and writes whatever the buffer releases, feeding
    /// each written line to the interview coach.
    ///
    /// Both halves run the production configuration
    /// ([`PipelineOptions::default`]) and must be given the *same* options —
    /// the buffer re-runs the echo veto and the seam repair the pipeline
    /// already applies, so a mismatch would silently change one of them.
    fn transcription_worker(
        flush_rx: mpsc::Receiver<Chunk>,
        engine: Arc<Mutex<ParakeetTranscriber>>,
        dict: Arc<Vec<dictionary::DictionaryTerm>>,
        writer: Arc<Mutex<TranscriptWriter>>,
        coach: Arc<Mutex<Option<Coach>>>,
    ) {
        let opts = PipelineOptions::default();
        let mut pipeline = ChunkPipeline::with_options(dict, opts);
        let mut buffer = LineBuffer::new(opts);

        while let Ok(chunk) = flush_rx.recv() {
            // Stamped at dequeue, not after transcription: a coaching tip's
            // latency is what the candidate experiences, which starts when
            // the interviewer stopped talking, and transcription is part of
            // that wait.
            let chunk_closed_at = Instant::now();
            let decision = pipeline.process(&chunk, |samples| {
                let mut guard = engine.lock().unwrap_or_else(|p| p.into_inner());
                Ok(guard.transcribe(samples)?.text)
            });
            route_decision(&mut buffer, &writer, &coach, decision, &chunk, chunk_closed_at);
        }

        // Every sender is gone (teardown dropped them after joining the
        // capture threads), so no later `Them` chunk can arrive to veto or
        // repair what is still held: release the tail rather than lose it.
        write_released(&writer, &coach, buffer.drain_all(), Instant::now());
    }

    /// Microphone capture loop: opens a cpal input stream, feeds a chunker at
    /// the device's native rate, and forwards `Me` chunks. Runs until `stop`.
    ///
    /// The mic chunker keeps the documented [`SILENCE_HOLD_SECS`] hold in
    /// every mode — the interview-mode hold is a `Them`-only policy, not a
    /// global threshold change.
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
                    stats.maybe_report(in_rate);
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
    ///
    /// The chunker's silence hold is re-applied every iteration rather than
    /// once at the top, so toggling interview mode mid-meeting tightens (or
    /// relaxes) the `Them` cut on the very next block.
    fn run_system_source(
        flush_tx: &mpsc::Sender<Chunk>,
        stop: &Arc<AtomicBool>,
        interview: &Arc<AtomicBool>,
        interview_hold_secs: f32,
    ) -> Result<()> {
        let capture = syscapture::SystemAudioCapture::start()
            .context("failed to start ScreenCaptureKit system-audio capture")?;
        let rate = capture.sample_rate();
        let mut chunker = StreamingChunker::new(Source::Them, rate);
        let mut stats = SourceStats::new("system (SCK)");

        while !stop.load(Ordering::SeqCst) {
            chunker.set_silence_hold_secs(if interview.load(Ordering::Relaxed) {
                interview_hold_secs
            } else {
                SILENCE_HOLD_SECS
            });
            match capture.recv_timeout(Duration::from_millis(100)) {
                Ok(mono) => {
                    stats.observe(&mono);
                    stats.maybe_report(rate);
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

    /// How much *captured audio* (not wall time) passes between periodic
    /// [`SourceStats`] reports. Measured in samples so a stalled source stops
    /// reporting instead of printing an unchanging line every 30 seconds.
    const STATS_REPORT_SECS: f32 = 30.0;

    /// Peak below which a source is called out as too quiet to detect speech
    /// in reliably. Half [`super::pipeline::NORMALIZE_PEAK_THRESHOLD`]: a
    /// chunk under that is merely gain-scaled before inference, but a source
    /// that never gets above this over 30 seconds is misconfigured, not soft.
    const QUIET_PEAK_THRESHOLD: f32 = 0.05;

    /// Per-source capture diagnostics: how much audio actually arrived and how
    /// loud it was. Printed every [`STATS_REPORT_SECS`] of captured audio and
    /// once more when the source stops, so a silent/blocked capture (e.g.
    /// Screen Recording permission denied but not errored) is visible during
    /// the meeting rather than mistaken for a quiet meeting and only noticed
    /// at the end.
    struct SourceStats {
        label: &'static str,
        blocks: u64,
        samples: u64,
        peak: f32,
        /// `samples` as of the last periodic report, so the next one is due a
        /// further `STATS_REPORT_SECS` of captured audio later.
        reported_samples: u64,
        /// The quiet warning is one-shot: it is advice about the setup, and
        /// repeating it every 30s would bury the per-source lines.
        quiet_warned: bool,
    }

    impl SourceStats {
        fn new(label: &'static str) -> Self {
            Self {
                label,
                blocks: 0,
                samples: 0,
                peak: 0.0,
                reported_samples: 0,
                quiet_warned: false,
            }
        }
        fn observe(&mut self, mono: &[f32]) {
            self.blocks += 1;
            self.samples += mono.len() as u64;
            let peak = mono.iter().fold(0.0f32, |m, s| m.max(s.abs()));
            if peak > self.peak {
                self.peak = peak;
            }
        }
        /// Emits the periodic report (and, the first time it applies, the
        /// quiet-source warning) once another `STATS_REPORT_SECS` of audio
        /// has been captured. Cheap enough to call per block.
        fn maybe_report(&mut self, rate: u32) {
            let interval = (STATS_REPORT_SECS * rate.max(1) as f32) as u64;
            if self.samples < self.reported_samples.saturating_add(interval) {
                return;
            }
            self.reported_samples = self.samples;
            self.report(rate);
            if !self.quiet_warned && self.peak < QUIET_PEAK_THRESHOLD {
                self.quiet_warned = true;
                eprintln!(
                    "[vzt-flow] {} source is very quiet (peak {:.4}); speech detection may \
                     miss — check the app's output volume / input gain",
                    self.label, self.peak
                );
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

    #[cfg(test)]
    mod tests {
        use super::{SourceStats, STATS_REPORT_SECS};

        /// One second of 16 kHz mono at a constant amplitude.
        fn one_second(level: f32) -> Vec<f32> {
            vec![level; 16_000]
        }

        #[test]
        fn stats_report_per_30s_of_audio_and_warn_once_about_a_quiet_source() {
            let interval = (STATS_REPORT_SECS * 16_000.0) as u64;
            let mut stats = SourceStats::new("mic");
            let block = one_second(0.01); // well under QUIET_PEAK_THRESHOLD

            for _ in 0..29 {
                stats.observe(&block);
                stats.maybe_report(16_000);
            }
            assert_eq!(stats.reported_samples, 0, "under 30s of audio, nothing is due yet");
            assert!(!stats.quiet_warned, "the warning needs 30s of evidence first");

            stats.observe(&block);
            stats.maybe_report(16_000);
            assert_eq!(stats.reported_samples, interval);
            assert!(stats.quiet_warned);

            // Reporting continues on the same cadence after the one-shot
            // warning has fired.
            for _ in 0..30 {
                stats.observe(&block);
                stats.maybe_report(16_000);
            }
            assert_eq!(stats.reported_samples, 2 * interval);
        }

        #[test]
        fn a_source_at_normal_level_is_never_called_quiet() {
            let interval = (STATS_REPORT_SECS * 16_000.0) as u64;
            let mut stats = SourceStats::new("system (SCK)");
            let block = one_second(0.4);
            for _ in 0..90 {
                stats.observe(&block);
                stats.maybe_report(16_000);
            }
            assert!(!stats.quiet_warned);
            assert_eq!(stats.reported_samples, 3 * interval);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;
    use std::sync::Mutex;

    use crate::llm::{GenRequest, NullGenerator, TextGenerator};

    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let n = TEMP_SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir()
            .join(format!("vzt-meeting-mod-{}-{tag}-{n}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Returns a fixed body of markdown for every request.
    struct CannedGen(String);

    impl TextGenerator for CannedGen {
        fn generate(&self, _req: GenRequest) -> Result<String> {
            Ok(self.0.clone())
        }
    }

    fn state_label(state: &SessionState) -> String {
        match state {
            SessionState::Recording => "recording".to_string(),
            SessionState::Stopping => "stopping".to_string(),
            SessionState::Finalizing { step } => format!("finalizing:{step}"),
            SessionState::Completed => "completed".to_string(),
            SessionState::Failed(msg) => format!("failed:{msg}"),
        }
    }

    /// A state observer plus the log it appends to.
    fn state_recorder() -> (Arc<Mutex<Vec<String>>>, StateObserver) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&log);
        let observer: StateObserver = Arc::new(move |state: &SessionState| {
            sink.lock().unwrap().push(state_label(state));
        });
        (log, observer)
    }

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
        let dir = temp_dir("summarize-file");
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

    /// The no-permission integration oracle: a synthetic line goes to disk
    /// first and reaches every observer second, and a panicking observer
    /// costs its notification rather than the line or the session.
    #[test]
    fn a_synthetic_line_reaches_every_observer() {
        let dir = temp_dir("fanout");
        let path = dir.join("2026-09-09-fanout.md");
        let file = OpenOptions::new().create(true).append(true).open(&path).unwrap();

        let mut fanout = LineFanout::new("sess-fanout");

        // Registered first: it reads the transcript back from disk while the
        // notification is in flight, so this fails against any writer that
        // notifies before it flushes.
        let durable = Arc::new(Mutex::new(Vec::<bool>::new()));
        {
            let durable = Arc::clone(&durable);
            let path = path.clone();
            fanout.add_observer(Arc::new(move |line: &TranscriptLine| {
                let on_disk = fs::read_to_string(&path).unwrap_or_default();
                durable.lock().unwrap().push(on_disk.contains(&line.text));
            }));
        }

        let seen: Vec<Arc<Mutex<Vec<(u64, String)>>>> =
            (0..3).map(|_| Arc::new(Mutex::new(Vec::new()))).collect();
        for sink in &seen {
            let sink = Arc::clone(sink);
            fanout.add_observer(Arc::new(move |line: &TranscriptLine| {
                sink.lock().unwrap().push((line.seq, line.text.clone()));
            }));
        }
        // Registered last, so the three above still see every line.
        fanout.add_observer(Arc::new(|line: &TranscriptLine| {
            assert!(line.seq != 2, "observer panics on the second line (deliberate)");
        }));

        let mut writer = TranscriptWriter::new(file, fanout);
        let texts = ["first line", "second line", "third line"];
        for (i, text) in texts.iter().enumerate() {
            let emitted = writer.append_line(i as f32 * 10.0, transcriber::Source::Them, text);
            if i == 1 {
                assert!(emitted.is_none(), "a panicking observer yields no delivered line");
            } else {
                let emitted = emitted.expect("line delivered");
                assert_eq!(emitted.seq as usize, i + 1);
                assert_eq!(emitted.session_id, "sess-fanout");
                assert_eq!(emitted.text, *text);
            }
        }
        assert_eq!(writer.body.len(), 3);
        drop(writer);

        // Every observer saw a line that was already on disk.
        assert_eq!(*durable.lock().unwrap(), vec![true, true, true]);

        // Every line is on disk — including the one whose observer panicked.
        let content = fs::read_to_string(&path).unwrap();
        for text in texts {
            assert!(content.contains(text), "{text} missing from {content}");
        }
        assert!(content.contains("[00:00:20] Them: third line"));

        for sink in &seen {
            let got = sink.lock().unwrap().clone();
            assert_eq!(got.iter().map(|(seq, _)| *seq).collect::<Vec<_>>(), vec![1, 2, 3]);
        }

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reserve_transcript_path_is_unique_for_same_second_starts() {
        let dir = temp_dir("reserve");
        // One timestamp for all five: the same date *and* the same second,
        // which is exactly what the old exists-then-open shape collided on.
        let now = chrono::Local::now();
        let mut paths = Vec::new();
        for _ in 0..5 {
            paths.push(reserve_transcript_path(&dir, &now, "Weekly Sync").unwrap());
        }

        let unique: std::collections::HashSet<&PathBuf> = paths.iter().collect();
        assert_eq!(unique.len(), 5, "collided: {paths:?}");
        for path in &paths {
            assert!(path.exists(), "{} was not created", path.display());
            let content = fs::read_to_string(path).unwrap();
            assert!(content.starts_with("# Meeting: Weekly Sync — "), "bad header: {content}");
        }
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 5);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn list_meetings_ignores_the_notes_sidecar() {
        let dir = temp_dir("sidecar");
        let transcript = dir.join("2026-09-08-standup.md");
        fs::write(&transcript, "# Meeting: Standup — 2026-09-08 09:00\n\n[00:00:01] Them: hi\n").unwrap();
        fs::write(notes::notes_path_for(&transcript), "my typed notes\n").unwrap();

        let found = list_meetings(&dir, 10).unwrap();
        assert_eq!(found.len(), 1, "sidecar leaked into the listing: {found:?}");
        assert_eq!(found[0].title, "Standup");

        fs::remove_dir_all(&dir).ok();
    }

    /// Compile-level: the exact shapes `flow-cli` (`commands/meeting.rs:22`)
    /// and the desktop (`meeting_ctl.rs:78/110`) call must still exist, and
    /// the defaults must fill in everything they don't pass. Coercing to
    /// `fn` pointers is the assertion — actually running a session would need
    /// a microphone, Screen Recording and the Parakeet model.
    #[test]
    fn run_with_defaults_matches_the_legacy_run_signature() {
        let legacy_run: fn(Option<String>, Option<PathBuf>, Arc<AtomicBool>) -> Result<PathBuf> = run;
        let legacy_start: fn(Option<String>, Option<PathBuf>) -> MeetingHandle = start;
        let legacy_stop: fn(MeetingHandle) -> Result<PathBuf> = MeetingHandle::stop;
        let legacy_is_running: fn(&MeetingHandle) -> bool = MeetingHandle::is_running;
        let full: fn(MeetingOptions, Arc<AtomicBool>) -> Result<MeetingOutcome> = run_with;
        let full_start: fn(MeetingOptions) -> Result<MeetingHandle> = start_with;
        let full_stop: fn(MeetingHandle) -> Result<MeetingOutcome> = MeetingHandle::stop_detailed;

        // What `run`/`start` hand to `run_with` for everything else.
        let opts = MeetingOptions {
            title: Some("Weekly Sync".to_string()),
            out_dir: Some(PathBuf::from("/tmp")),
            ..Default::default()
        };
        assert!(opts.transcript_path.is_none());
        assert!(opts.on_started.is_none() && opts.on_line.is_none());
        assert!(opts.on_state.is_none() && opts.on_tip.is_none());
        assert!(opts.generator.is_none() && opts.interview_context.is_none());
        assert!(opts.pdf.is_none(), "no PDF unless the caller asks for one");
        assert!(!opts.interview.load(Ordering::SeqCst), "interview mode is off by default");

        let _ = (
            legacy_run,
            legacy_start,
            legacy_stop,
            legacy_is_running,
            full,
            full_start,
            full_stop,
        );
    }

    #[test]
    fn finalize_records_a_pdf_error_without_failing_the_meeting() {
        let dir = temp_dir("pdf-error");
        let now = chrono::Local::now();
        let transcript = reserve_transcript_path(&dir, &now, "Broken Export").unwrap();
        let notes_path = notes::notes_path_for(&transcript);

        // A regular file where the export directory should be: `create_dir_all`
        // fails against it, which is the portable stand-in for an unwritable
        // destination (and unlike a chmod, it also blocks a root test runner).
        let blocked = dir.join("blocked");
        fs::write(&blocked, b"not a directory").unwrap();

        let (log, on_state) = state_recorder();
        let generator = NullGenerator;
        let summary_opts = summary::SummaryOptions::default();
        let pdf_opts = pdf::PdfOptions { dir: blocked.join("exports"), enabled: true };

        let outcome = finalize(
            FinalizeRequest {
                session_id: &session_id_for(&transcript),
                title: "Broken Export",
                started_at: now,
                transcript: &transcript,
                notes_path: &notes_path,
                generator: &generator,
                summary: &summary_opts,
                pdf: Some(&pdf_opts),
                on_state: Some(&on_state),
                model_note: None,
            },
            || vec!["Them: are we shipping".to_string(), "Me: yes".to_string()],
        );

        assert!(outcome.transcript.exists(), "the transcript survives a failed export");
        assert!(outcome.pdf.is_none());
        let err = outcome.pdf_error.expect("pdf error recorded");
        assert!(!err.is_empty());

        let states = log.lock().unwrap().clone();
        assert_eq!(states.last().map(String::as_str), Some("completed"), "states: {states:?}");
        assert!(states.iter().any(|s| s == "finalizing:writing pdf"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn finalize_emits_states_in_order() {
        let dir = temp_dir("states");
        let now = chrono::Local::now();
        let transcript = reserve_transcript_path(&dir, &now, "Ordered").unwrap();
        let notes_path = notes::notes_path_for(&transcript);

        let (log, on_state) = state_recorder();
        let generator = CannedGen("## Summary\n- shipped\n".to_string());
        let summary_opts = summary::SummaryOptions::default();
        let pdf_opts = pdf::PdfOptions { dir: dir.join("exports"), enabled: true };

        let _ = finalize(
            FinalizeRequest {
                session_id: &session_id_for(&transcript),
                title: "Ordered",
                started_at: now,
                transcript: &transcript,
                notes_path: &notes_path,
                generator: &generator,
                summary: &summary_opts,
                pdf: Some(&pdf_opts),
                on_state: Some(&on_state),
                model_note: None,
            },
            || vec!["Them: shall we ship".to_string()],
        );

        let states = log.lock().unwrap().clone();
        assert_eq!(states.first().map(String::as_str), Some("stopping"), "states: {states:?}");
        assert_eq!(states.last().map(String::as_str), Some("completed"), "states: {states:?}");

        let stopping = states.iter().position(|s| s == "stopping").unwrap();
        let first_finalizing = states.iter().position(|s| s.starts_with("finalizing:")).unwrap();
        assert!(stopping < first_finalizing, "states: {states:?}");
        assert_eq!(states[first_finalizing], "finalizing:merging notes");
        assert!(states.iter().any(|s| s.starts_with("finalizing:summarizing ")), "states: {states:?}");
        assert!(states.iter().any(|s| s == "finalizing:writing pdf"), "states: {states:?}");
        assert!(!states.iter().any(|s| s.starts_with("failed")), "states: {states:?}");

        fs::remove_dir_all(&dir).ok();
    }

    /// With no cleanup model the session degrades to a `NullGenerator`, and
    /// the transcript says so — but it must not carry a coverage line
    /// claiming to cover a summary that was never written.
    #[test]
    fn a_missing_cleanup_model_is_explained_not_claimed_as_coverage() {
        let dir = temp_dir("no-model");
        let now = chrono::Local::now();
        let transcript = reserve_transcript_path(&dir, &now, "No Model").unwrap();
        let notes_path = notes::notes_path_for(&transcript);
        let generator = NullGenerator;
        let summary_opts = summary::SummaryOptions::default();

        let outcome = finalize(
            FinalizeRequest {
                session_id: &session_id_for(&transcript),
                title: "No Model",
                started_at: now,
                transcript: &transcript,
                notes_path: &notes_path,
                generator: &generator,
                summary: &summary_opts,
                pdf: None,
                on_state: None,
                model_note: Some("No cleanup model is installed, so no summary was generated."),
            },
            || vec!["Them: hello".to_string()],
        );

        assert!(!outcome.summary_complete);
        let content = fs::read_to_string(&transcript).unwrap();
        assert!(content.contains("No cleanup model is installed"), "{content}");
        assert!(!content.contains("Summary covers the full transcript"), "{content}");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn notes_are_merged_before_the_summary_is_appended() {
        let dir = temp_dir("notes-order");
        let now = chrono::Local::now();
        let transcript = reserve_transcript_path(&dir, &now, "Notes First").unwrap();
        let notes_path = notes::notes_path_for(&transcript);
        fs::write(&notes_path, "ask about pricing\n").unwrap();
        // A real capture line, so the transcript has a body before finalize.
        {
            let mut f = OpenOptions::new().append(true).open(&transcript).unwrap();
            writeln!(f, "[00:00:05] Them: what does it cost").unwrap();
        }

        let (log, on_state) = state_recorder();
        let generator = CannedGen(
            "## Summary\n- pricing was discussed\n\n## Action items\n- send the quote".to_string(),
        );
        let summary_opts = summary::SummaryOptions::default();

        let outcome = finalize(
            FinalizeRequest {
                session_id: &session_id_for(&transcript),
                title: "Notes First",
                started_at: now,
                transcript: &transcript,
                notes_path: &notes_path,
                generator: &generator,
                summary: &summary_opts,
                pdf: None,
                on_state: Some(&on_state),
                model_note: None,
            },
            || vec!["Them: what does it cost".to_string()],
        );

        assert!(outcome.notes_merged, "the sidecar should have been merged");
        assert_eq!(outcome.summary_sections, 1);
        assert!(outcome.summary_complete);

        let content = fs::read_to_string(&transcript).unwrap();
        let notes_at = content.find(notes::NOTES_HEADING).expect("## My notes heading");
        let summary_at = content.find("## Summary").expect("## Summary heading");
        assert!(notes_at < summary_at, "notes must precede the summary:\n{content}");
        assert!(content.contains("ask about pricing"));
        assert!(content.contains("_Summary covers the full transcript"));

        let states = log.lock().unwrap().clone();
        let merging = states.iter().position(|s| s == "finalizing:merging notes").unwrap();
        let summarizing = states.iter().position(|s| s.starts_with("finalizing:summarizing ")).unwrap();
        assert!(merging < summarizing, "states: {states:?}");

        fs::remove_dir_all(&dir).ok();
    }

    // ---- B7: decision -> LineBuffer -> writer routing ----

    /// Builds a chunk carrying `len` seconds of (unused) 16 kHz audio, so
    /// `end_offset()` — the meeting time `push_chunk` advances the buffer to —
    /// is `start + len`.
    fn routed_chunk(source: transcriber::Source, start: f32, len: f32) -> transcriber::Chunk {
        transcriber::Chunk {
            source,
            samples: vec![0.0; (len * 16_000.0) as usize],
            sample_rate: 16_000,
            start_offset: start,
            has_speech: true,
            speech_secs: len,
            ..Default::default()
        }
    }

    #[test]
    fn routing_writes_them_at_once_holds_me_and_drains_the_tail() {
        use transcriber::Source;

        let dir = temp_dir("route");
        let path = dir.join("2026-09-09-route.md");
        let file = OpenOptions::new().create(true).append(true).open(&path).unwrap();
        let writer = Mutex::new(TranscriptWriter::new(file, LineFanout::new("sess-route")));
        // No coach: routing must not depend on one being installed.
        let coach: Mutex<Option<interview::Coach>> = Mutex::new(None);
        let opts = pipeline::PipelineOptions::default();
        let mut buffer = pipeline::LineBuffer::new(opts);
        let closed_at = std::time::Instant::now();
        let on_disk = || fs::read_to_string(&path).unwrap();

        // (a) An ordinary (not hard-cut) Them line is written by the very
        // call that produced it — no hold, so the coach and the live notepad
        // see it with no added latency.
        let them = routed_chunk(Source::Them, 0.0, 2.0);
        route_decision(
            &mut buffer,
            &writer,
            &coach,
            pipeline::Decision::Emit {
                start: 0.0,
                source: Source::Them,
                text: "them speaks first".to_string(),
            },
            &them,
            closed_at,
        );
        assert!(
            on_disk().contains("[00:00:00] Them: them speaks first"),
            "an ordinary Them line must reach the transcript immediately:\n{}",
            on_disk()
        );

        // (b) A Me line the per-chunk echo check could not catch (its Them
        // counterpart had not been transcribed yet) is held, then vetoed by
        // that overlapping Them line when it finally arrives.
        let echo = "so the migration lands on thursday";
        let me = routed_chunk(Source::Me, 10.5, 2.0);
        route_decision(
            &mut buffer,
            &writer,
            &coach,
            pipeline::Decision::Emit { start: 10.5, source: Source::Me, text: echo.to_string() },
            &me,
            closed_at,
        );
        assert!(!on_disk().contains("Me: so the migration"), "a Me line is held, not written");

        let late_them = routed_chunk(Source::Them, 10.0, 3.0);
        route_decision(
            &mut buffer,
            &writer,
            &coach,
            pipeline::Decision::Emit { start: 10.0, source: Source::Them, text: echo.to_string() },
            &late_them,
            closed_at,
        );
        assert_eq!(buffer.dropped_echo_count(), 1, "the late Them line vetoes the held Me line");
        let content = on_disk();
        assert!(content.contains("[00:00:10] Them: so the migration lands on thursday"), "{content}");
        assert!(!content.contains("Me: so the migration"), "the vetoed echo must never be written:\n{content}");

        // (c) The last Me line is still inside its hold window when the
        // channel disconnects; `drain_all` is what keeps it.
        let tail = routed_chunk(Source::Me, 20.0, 1.0);
        route_decision(
            &mut buffer,
            &writer,
            &coach,
            pipeline::Decision::Emit {
                start: 20.0,
                source: Source::Me,
                text: "my closing thought".to_string(),
            },
            &tail,
            closed_at,
        );
        assert!(!on_disk().contains("my closing thought"), "still held while the hold window runs");

        write_released(&writer, &coach, buffer.drain_all(), std::time::Instant::now());
        let content = on_disk();
        assert!(
            content.contains("[00:00:20] Me: my closing thought"),
            "the tail must survive the channel disconnect:\n{content}"
        );

        // Everything written, in start_offset order, and the writer's body
        // (what the summarizer reads) agrees with the file.
        let body = writer.lock().unwrap().body.clone();
        assert_eq!(
            body,
            vec![
                "Them: them speaks first".to_string(),
                "Them: so the migration lands on thursday".to_string(),
                "Me: my closing thought".to_string(),
            ]
        );

        fs::remove_dir_all(&dir).ok();
    }
}
