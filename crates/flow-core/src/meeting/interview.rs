//! Local interview coach: watches the `Them` transcript stream for
//! interview-style questions and, on a match, generates a short coaching tip
//! grounded in the candidate's own context (resume/talking points).
//!
//! Stub for U1: public surface only, minus the `Coach` struct and its
//! `TextGenerator`-dependent `spawn`/`observe`/`clear`/`shutdown` (that type
//! doesn't exist until U2). U6 replaces every body here.

use std::path::PathBuf;

use super::events::TranscriptLine;

/// Path to the candidate context file, `~/.config/vzt-flow/interview.md`.
/// Stub (U6): always fails.
pub fn context_path() -> anyhow::Result<PathBuf> {
    anyhow::bail!("interview: not implemented (U6)")
}

/// Reads the candidate context file, or `""` when absent. Stub (U6).
pub fn load_context() -> String {
    // stub (U6)
    String::new()
}

/// Writes the candidate context file atomically (temp + rename). Stub (U6):
/// always fails.
pub fn save_context(_text: &str) -> anyhow::Result<()> {
    anyhow::bail!("interview: not implemented (U6)")
}

/// Truncates `text` to `max_chars`, keeping the head. Stub (U6): returns the
/// input unchanged.
pub fn truncate_context(text: &str, _max_chars: usize) -> String {
    // stub (U6)
    text.to_string()
}

/// Whether `text` reads as an interview question worth coaching on. Stub
/// (U6): always false.
pub fn looks_like_question(_text: &str) -> bool {
    // stub (U6)
    false
}

/// Builds the coach's system prompt from the candidate context. Stub (U6):
/// returns the input unchanged.
pub fn build_coach_system_prompt(context: &str) -> String {
    // stub (U6)
    context.to_string()
}

/// Builds the coach's user turn from recent transcript lines and the
/// triggering question. Stub (U6): returns the question unchanged.
pub fn build_coach_user_turn(_recent: &[TranscriptLine], question: &str) -> String {
    // stub (U6)
    question.to_string()
}

/// Parses a raw model reply into `(headline, bullets)`. Stub (U6): always
/// `None`.
pub fn parse_tip(_raw: &str) -> Option<(String, Vec<String>)> {
    // stub (U6)
    None
}
