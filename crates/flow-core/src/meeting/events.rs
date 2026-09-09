//! Meeting companion event types and the transcript-line fanout.
//!
//! This module is the pure, platform-independent event vocabulary shared by
//! the session engine (`meeting/mod.rs`), the desktop notepad window, the
//! interview coach, and the summary/PDF finalize path. Nothing here touches
//! macOS-only APIs, audio, or the model — it's plain data plus a small
//! observer-list helper, so it's fully testable off macOS.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::{Serialize, Serializer};

use super::transcriber::Source;

/// Identifies one meeting session. Equal to the transcript file's stem
/// (e.g. `2026-09-08-zoom-meeting-141203`) and immutable for the life of the
/// session.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(pub String);

/// `Source` re-exported from [`super::transcriber`] plus a lowercase
/// `Serialize` impl, added here rather than on the original type so
/// `transcriber.rs` stays untouched (out of scope for this unit).
impl Serialize for Source {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let s = match self {
            Source::Them => "them",
            Source::Me => "me",
        };
        serializer.serialize_str(s)
    }
}

/// One recognized line of transcript, as it is emitted to every observer
/// (transcript file, notepad window, interview coach).
#[derive(Debug, Clone, Serialize)]
pub struct TranscriptLine {
    pub session_id: String,
    pub seq: u64,
    pub source: Source,
    pub offset_secs: f32,
    pub text: String,
}

/// Coarse lifecycle of a meeting session, mirroring `OverlayEvent`
/// (`apps/desktop/src-tauri/src/overlay.rs:95-120`) in shape: a `kind` tag so
/// the frontend can switch on it directly.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionState {
    Recording,
    Stopping,
    Finalizing { step: String },
    Completed,
    Failed(String),
}

/// Emitted once, synchronously, when a session is reserved and its files are
/// known — before the capture threads start.
#[derive(Debug, Clone, Serialize)]
pub struct SessionStarted {
    pub session_id: String,
    pub title: String,
    pub started_at_ms: i64,
    pub transcript: std::path::PathBuf,
    pub notes: std::path::PathBuf,
}

/// One interview-coaching suggestion, keyed to the question that triggered it.
#[derive(Debug, Clone, Serialize)]
pub struct InterviewTip {
    pub session_id: String,
    pub seq: u64,
    pub question: String,
    pub question_offset_secs: f32,
    pub headline: String,
    pub bullets: Vec<String>,
    pub latency_ms: u64,
}

/// Result of finalizing a session: what got written, and what didn't.
#[derive(Debug, Clone, Serialize)]
pub struct MeetingOutcome {
    pub session_id: String,
    pub transcript: std::path::PathBuf,
    pub pdf: Option<std::path::PathBuf>,
    pub pdf_error: Option<String>,
    pub notes_merged: bool,
    pub notes_rev_used: u64,
    pub summary_sections: usize,
    pub summary_complete: bool,
}

/// Assigns a monotonic per-session sequence number to each recognized line
/// and fans it out to every registered observer, in registration order.
///
/// Pure and platform-independent: no file I/O, no threads of its own. The
/// caller (`TranscriptWriter` in `meeting/mod.rs`) is expected to persist the
/// line *before* calling [`LineFanout::push`], not after — an observer panic
/// must not cost a transcript line.
pub struct LineFanout {
    session_id: String,
    seq: AtomicU64,
    observers: Vec<Arc<dyn Fn(&TranscriptLine) + Send + Sync>>,
}

impl LineFanout {
    /// Creates a fanout for `session_id` with no observers registered.
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            seq: AtomicU64::new(0),
            observers: Vec::new(),
        }
    }

    /// Registers an additional observer. Call before any [`Self::push`] —
    /// there's no synchronization between registration and delivery.
    pub fn add_observer(&mut self, observer: Arc<dyn Fn(&TranscriptLine) + Send + Sync>) {
        self.observers.push(observer);
    }

    /// Builds a [`TranscriptLine`] with the next sequence number, calls every
    /// observer with it in registration order, then returns it.
    pub fn push(&self, source: Source, offset_secs: f32, text: String) -> TranscriptLine {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        let line = TranscriptLine {
            session_id: self.session_id.clone(),
            seq,
            source,
            offset_secs,
            text,
        };
        for observer in &self.observers {
            observer(&line);
        }
        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn fanout_assigns_monotonic_seq_and_calls_every_observer() {
        let mut fanout = LineFanout::new("sess-1");
        let seen: Vec<Arc<Mutex<Vec<u64>>>> = (0..3).map(|_| Arc::new(Mutex::new(Vec::new()))).collect();
        for s in &seen {
            let s = Arc::clone(s);
            fanout.add_observer(Arc::new(move |line: &TranscriptLine| {
                s.lock().unwrap().push(line.seq);
            }));
        }

        for i in 0..5 {
            fanout.push(Source::Them, i as f32, format!("line {i}"));
        }

        for s in &seen {
            assert_eq!(*s.lock().unwrap(), vec![1, 2, 3, 4, 5]);
        }
    }

    #[test]
    fn fanout_line_carries_session_id_and_source() {
        let fanout = LineFanout::new("sess-42");
        let line = fanout.push(Source::Me, 12.5, "hello".to_string());
        assert_eq!(line.session_id, "sess-42");
        assert_eq!(line.seq, 1);
        assert_eq!(line.source, Source::Me);
        assert_eq!(line.offset_secs, 12.5);
        assert_eq!(line.text, "hello");
    }

    #[test]
    fn session_state_serializes_with_a_kind_tag() {
        let state = SessionState::Finalizing { step: "summarizing 3/9".to_string() };
        let json = serde_json::to_value(&state).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"kind": "finalizing", "step": "summarizing 3/9"})
        );
    }
}
