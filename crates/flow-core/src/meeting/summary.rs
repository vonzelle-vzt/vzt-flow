//! Hierarchical meeting summary: splits a long transcript into bounded
//! windows, summarizes each independently, then folds the partials into one
//! final pass — so a 60-minute meeting's early decisions survive instead of
//! being dropped by the old tail-only truncation.
//!
//! Stub for U1: public surface only, minus the `TextGenerator`-dependent
//! `summarize` entry point (that type doesn't exist until U2). U5 replaces
//! every body here.

/// Tuning knobs for the hierarchical summary pass.
pub struct SummaryOptions {
    pub window_chars: usize,
    pub partial_timeout_ms: u64,
    pub final_timeout_ms: u64,
    pub max_passes: usize,
}

/// Result of summarizing a transcript body.
pub struct SummaryResult {
    pub markdown: String,
    pub sections: usize,
    pub complete: bool,
    pub coverage_note: String,
}

/// The system/task prompt for one partial-window summarization pass. Stub
/// (U5): returns an empty string.
pub fn build_partial_prompt() -> String {
    // stub (U5)
    String::new()
}

/// The window size to use for a transcript of `total_chars`, coarsening
/// (growing) past `opts.window_chars` so a very long meeting never exceeds
/// `opts.max_passes` windows. Stub (U5): returns `opts.window_chars`
/// unchanged.
pub fn effective_window_chars(_total_chars: usize, opts: &SummaryOptions) -> usize {
    // stub (U5)
    opts.window_chars
}
