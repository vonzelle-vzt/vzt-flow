//! Hierarchical meeting summary: splits a long transcript into bounded
//! windows, summarizes each independently, then folds the partials into one
//! final pass — so a 60-minute meeting's early decisions survive instead of
//! being dropped by the old tail-only truncation.

use crate::cleanup::build_summary_prompt;
use crate::llm::{GenRequest, Priority, TextGenerator};
use crate::meeting::transcriber::split_for_summary;

/// Default window size (chars) for one partial-summary pass.
const DEFAULT_WINDOW_CHARS: usize = 6_000;

/// Default cap on how many windows a transcript is ever split into,
/// regardless of length — past this, windows coarsen instead of multiplying.
const DEFAULT_MAX_PASSES: usize = 12;

/// Placeholder line substituted for a window whose partial pass returned no
/// usable output (empty or timed out).
const DEGRADED_SECTION: &str = "- (a portion of the meeting could not be summarized)";

impl Default for SummaryOptions {
    fn default() -> Self {
        Self {
            window_chars: DEFAULT_WINDOW_CHARS,
            partial_timeout_ms: 12_000,
            final_timeout_ms: 20_000,
            max_passes: DEFAULT_MAX_PASSES,
        }
    }
}

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

/// The system/task prompt for one partial-window summarization pass. Task
/// instruction stays last (CLAUDE.md gotcha (l)): Qwen3 decodes greedily and
/// regurgitates whatever sits at the tail of the prompt when there's little
/// to correct, so the instruction — not glossary/context material — must be
/// the last thing it reads.
pub fn build_partial_prompt() -> String {
    "You compress one portion of a meeting transcript. Lines are labelled \
     \"Them:\" and \"Me:\". Keep names, numbers, decisions and commitments \
     verbatim. Do NOT invent owners, dates or deadlines. Write at most 6 \
     terse bullets, no headings, no preamble. Output only the bullets."
        .to_string()
}

/// The window size to use for a transcript of `total_chars`, coarsening
/// (growing) past `opts.window_chars` so a very long meeting never exceeds
/// `opts.max_passes` windows — coverage stays complete at any length instead
/// of dropping content.
pub fn effective_window_chars(total_chars: usize, opts: &SummaryOptions) -> usize {
    if opts.max_passes == 0 || total_chars == 0 {
        return opts.window_chars.max(1);
    }
    let coarsened = total_chars.div_ceil(opts.max_passes);
    opts.window_chars.max(coarsened)
}

/// Summarizes `body` (a meeting transcript's `Them:`/`Me:` lines), splitting
/// into windows sized by [`effective_window_chars`] when it's too long for a
/// single pass.
///
/// A single window takes the direct path: one call to `gen` with
/// [`build_summary_prompt`] (unchanged behaviour from before hierarchical
/// summarization existed). More than one window runs a partial pass per
/// window (`build_partial_prompt`, `max_new_tokens = 220`,
/// `Priority::Background`, `opts.partial_timeout_ms`), then a single final
/// pass over the concatenated partials using `build_summary_prompt`
/// (`max_new_tokens = 420`, `opts.final_timeout_ms`).
///
/// A partial that comes back empty (failed, timed out, or degraded per the
/// `TextGenerator` empty-string contract) is replaced by
/// [`DEGRADED_SECTION`] and marks the result `complete = false` — the
/// transcript's other windows still contribute rather than the whole summary
/// being abandoned.
///
/// `progress(i, n)` is called before each of the `n` total generator calls
/// (partials, then the final pass) so a caller can drive a
/// `Finalizing{step: "summarizing i/n"}` UI event.
pub fn summarize(
    body: &str,
    gen: &dyn TextGenerator,
    opts: &SummaryOptions,
    progress: &dyn Fn(usize, usize),
) -> SummaryResult {
    let total_chars = body.chars().count();
    let window_chars = effective_window_chars(total_chars, opts);
    let windows = split_for_summary(body, window_chars);
    let sections = windows.len().max(1);

    if windows.len() <= 1 {
        let text = if windows.is_empty() { body } else { &windows[0] };
        progress(1, 1);
        let markdown = run_final_pass(text, gen, opts);
        return SummaryResult {
            markdown,
            sections: 1,
            complete: true,
            coverage_note: "Summary covers the full transcript (1 sections).".to_string(),
        };
    }

    let n = windows.len();
    let total_calls = n + 1;
    let mut complete = true;
    let mut summarized = 0usize;
    let mut partials: Vec<String> = Vec::with_capacity(n);

    for (i, window) in windows.iter().enumerate() {
        progress(i + 1, total_calls);
        let req = GenRequest {
            system: build_partial_prompt(),
            user: format!("{window} /no_think"),
            max_new_tokens: 220,
            timeout_ms: opts.partial_timeout_ms,
            priority: Priority::Background,
        };
        let text = gen.generate(req).unwrap_or_default();
        let trimmed = text.trim();
        if trimmed.is_empty() {
            complete = false;
            partials.push(DEGRADED_SECTION.to_string());
        } else {
            summarized += 1;
            partials.push(trimmed.to_string());
        }
    }

    progress(total_calls, total_calls);
    let combined = partials.join("\n");
    let markdown = run_final_pass(&combined, gen, opts);

    let coverage_note = if complete {
        format!("Summary covers the full transcript ({n} sections).")
    } else {
        format!("Summary coverage is partial: {summarized} of {n} sections were summarized.")
    };

    SummaryResult { markdown, sections, complete, coverage_note }
}

/// Runs the final consolidation pass over `text` (either the sole window's
/// raw text, or the concatenated partials) with the unchanged
/// `build_summary_prompt`.
fn run_final_pass(text: &str, gen: &dyn TextGenerator, opts: &SummaryOptions) -> String {
    let req = GenRequest {
        system: build_summary_prompt(),
        user: format!("{text} /no_think"),
        max_new_tokens: 420,
        timeout_ms: opts.final_timeout_ms,
        priority: Priority::Background,
    };
    gen.generate(req).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// Fake generator: counts calls, and returns canned/empty output per a
    /// caller-supplied rule.
    struct FakeGen {
        calls: AtomicUsize,
        seen: Mutex<Vec<String>>,
        empty_on_call: Vec<usize>, // 1-indexed call numbers to return "" for
    }

    impl FakeGen {
        fn new() -> Self {
            Self { calls: AtomicUsize::new(0), seen: Mutex::new(Vec::new()), empty_on_call: Vec::new() }
        }

        fn with_empty_on(calls: Vec<usize>) -> Self {
            Self { calls: AtomicUsize::new(0), seen: Mutex::new(Vec::new()), empty_on_call: calls }
        }

        fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl TextGenerator for FakeGen {
        fn generate(&self, req: GenRequest) -> Result<String> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            self.seen.lock().unwrap().push(req.user.clone());
            if self.empty_on_call.contains(&n) {
                Ok(String::new())
            } else {
                Ok(format!("summary-{n}"))
            }
        }
    }

    fn opts() -> SummaryOptions {
        SummaryOptions { window_chars: 20, partial_timeout_ms: 1_000, final_timeout_ms: 1_000, max_passes: 12 }
    }

    fn no_progress(_i: usize, _n: usize) {}

    #[test]
    fn effective_window_chars_grows_to_cap_the_pass_count() {
        let opts = SummaryOptions { window_chars: 6_000, partial_timeout_ms: 0, final_timeout_ms: 0, max_passes: 12 };
        let total = 200_000usize;
        let window = effective_window_chars(total, &opts);
        // ceil(200_000 / 12) = 16_667
        assert_eq!(window, 16_667);
        let expected_windows = total.div_ceil(window);
        assert_eq!(expected_windows, 12);
    }

    #[test]
    fn hierarchical_summary_calls_the_generator_once_per_window_plus_a_final_pass() {
        // Build a body that splits into exactly 3 windows at window_chars=20.
        let line = "Them: 0123456789"; // 16 chars
        let body = vec![line; 3].join("\n");
        let o = opts();
        let windows = split_for_summary(&body, effective_window_chars(body.chars().count(), &o));
        assert_eq!(windows.len(), 3, "test setup: expected 3 windows, got {}", windows.len());

        let gen = FakeGen::new();
        let result = summarize(&body, &gen, &o, &no_progress);
        assert_eq!(gen.call_count(), 4); // 3 partials + 1 final
        assert!(result.complete);
        assert_eq!(result.sections, 3);
    }

    #[test]
    fn a_failed_partial_degrades_that_section_and_marks_incomplete() {
        let line = "Them: 0123456789";
        let body = vec![line; 3].join("\n");
        let o = opts();
        // Window 2 (the 2nd generator call) returns empty.
        let gen = FakeGen::with_empty_on(vec![2]);
        let result = summarize(&body, &gen, &o, &no_progress);
        assert!(!result.complete);
        assert_eq!(result.coverage_note, "Summary coverage is partial: 2 of 3 sections were summarized.");
    }

    #[test]
    fn single_window_takes_the_direct_path() {
        let body = "Them: hello\nMe: hi";
        let o = SummaryOptions { window_chars: 6_000, ..opts() };
        let gen = FakeGen::new();
        let result = summarize(body, &gen, &o, &no_progress);
        assert_eq!(gen.call_count(), 1);
        assert!(result.complete);
        assert_eq!(result.sections, 1);
    }

    #[test]
    fn coverage_note_names_the_section_count() {
        let body = "Them: hello\nMe: hi";
        let o = SummaryOptions { window_chars: 6_000, ..opts() };
        let gen = FakeGen::new();
        let result = summarize(body, &gen, &o, &no_progress);
        assert_eq!(result.coverage_note, "Summary covers the full transcript (1 sections).");
    }

    #[test]
    fn progress_reports_every_call_including_the_final_pass() {
        let line = "Them: 0123456789";
        let body = vec![line; 3].join("\n");
        let o = opts();
        let gen = FakeGen::new();
        let seen = Mutex::new(Vec::new());
        let progress = |i: usize, n: usize| seen.lock().unwrap().push((i, n));
        summarize(&body, &gen, &o, &progress);
        let seen = seen.into_inner().unwrap();
        assert_eq!(seen, vec![(1, 4), (2, 4), (3, 4), (4, 4)]);
    }
}
