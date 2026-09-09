//! Echo dedup for the no-headphones case.
//!
//! When the user isn't wearing headphones, their microphone picks up the
//! *other* participants' voices coming out of the speakers, a beat after
//! ScreenCaptureKit already captured the same audio from the system mix. That
//! produces a spurious `Me:` line that is really an echo of a `Them:` line.
//!
//! The guard is deliberately conservative: a `Me:` chunk is dropped only when
//! it (a) overlaps in time with a `Them:` chunk and (b) is textually near-
//! identical to it (normalized-token Jaccard similarity above a threshold),
//! or contains nearly all of the mic tokens despite a different chunk boundary.
//! Real back-channel interjections ("yeah", "right", "makes sense") are short
//! and rarely word-for-word matches, so they survive.
//!
//! Wearing headphones eliminates the echo at the source and is the
//! recommended setup — see `docs/MEETINGS.md`.

use std::collections::HashSet;

/// Default Jaccard-similarity threshold above which an overlapping `Me:`
/// chunk is treated as an echo of a `Them:` chunk and dropped. Chosen per the
/// feature spec (> 0.7): high enough that a genuinely different sentence that
/// merely shares a few common words ("the", "we", "so") is kept, low enough
/// that ASR jitter between the two captures of the same speech (a dropped
/// filler word, a mis-heard token) doesn't push a true echo under the bar.
pub const DEFAULT_ECHO_THRESHOLD: f64 = 0.7;

/// Maximum gap between spans considered for echo comparison. Independent
/// chunk boundaries and delayed speaker bleed can make echo spans abut.
pub const ECHO_TIME_TOLERANCE_SECS: f32 = 1.0;

/// Fraction of mic tokens also present in the system-audio chunk. Unlike
/// Jaccard, this catches a short mic fragment of a longer system utterance.
pub const ECHO_CONTAINMENT_THRESHOLD: f64 = 0.8;

/// Overlap allowing a gap of less than `tol` seconds between the spans.
/// At zero tolerance this is the existing strict half-open overlap test.
pub fn overlaps_within(a_start: f32, a_end: f32, b_start: f32, b_end: f32, tol: f32) -> bool {
    a_start - tol < b_end && b_start - tol < a_end
}

/// `|A ∩ B| / |A|`: the share of `a` tokens also found in `b`.
/// An empty `a` has no evidence of containment and returns zero.
pub fn containment(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() { return 0.0; }
    a.intersection(b).count() as f64 / a.len() as f64
}

/// A `Me:` utterance with fewer than this many tokens is never treated as an
/// echo, regardless of similarity: short acknowledgements ("yeah", "right",
/// "for sure") are legitimate speech and a 1-2 word overlap with a long
/// `Them:` line can score deceptively high on Jaccard.
const MIN_TOKENS_FOR_ECHO: usize = 3;

/// Lowercases, strips punctuation, and splits `text` into a set of word
/// tokens for similarity comparison. Punctuation and casing differ freely
/// between the two ASR passes over the same audio, so both are normalized
/// away. Returns a *set* (not a bag) because echo detection cares about
/// which words appear, not how many times.
pub fn normalize_tokens(text: &str) -> HashSet<String> {
    text.split_whitespace()
        .map(|w| {
            w.chars()
                .filter(|c| c.is_alphanumeric())
                .flat_map(|c| c.to_lowercase())
                .collect::<String>()
        })
        .filter(|w| !w.is_empty())
        .collect()
}

/// Jaccard similarity `|A ∩ B| / |A ∪ B|` of two token sets, in `[0, 1]`.
/// Two empty sets are defined as identical (1.0); one empty and one not is
/// 0.0.
pub fn jaccard_similarity(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let intersection = a.intersection(b).count();
    let union = a.union(b).count();
    if union == 0 {
        return 0.0;
    }
    intersection as f64 / union as f64
}

/// Whether `me_text` (a candidate `Me:` line) is a textual echo of
/// `them_text` (a time-overlapping `Them:` line) at or above `threshold`.
/// Short `Me:` utterances (< [`MIN_TOKENS_FOR_ECHO`] tokens) are never
/// echoes — see that constant. Delegates to [`is_echo_with`] with
/// containment always enabled; see that function to toggle it (used by the
/// meeting pipeline's `PipelineOptions::echo_containment` for attribution).
pub fn is_echo(me_text: &str, them_text: &str, threshold: f64) -> bool {
    is_echo_with(me_text, them_text, threshold, true)
}

/// As [`is_echo`], but the containment arm ([`ECHO_CONTAINMENT_THRESHOLD`])
/// can be switched off via `containment_enabled` — used by the meeting
/// pipeline's `PipelineOptions` so an offline replay harness can attribute a
/// WER change to the containment check specifically.
pub fn is_echo_with(me_text: &str, them_text: &str, threshold: f64, containment_enabled: bool) -> bool {
    let me = normalize_tokens(me_text);
    if me.len() < MIN_TOKENS_FOR_ECHO {
        return false;
    }
    let them = normalize_tokens(them_text);
    jaccard_similarity(&me, &them) > threshold
        || (containment_enabled && containment(&me, &them) >= ECHO_CONTAINMENT_THRESHOLD)
}

/// Whether two half-open time intervals `[a_start, a_end)` and
/// `[b_start, b_end)` overlap. Used to restrict echo comparison to `Them:`
/// chunks that actually coincide with the `Me:` chunk in wall-clock time, so
/// an unrelated sentence spoken minutes apart can never be mistaken for an
/// echo no matter how similar the words.
pub fn time_overlaps(a_start: f32, a_end: f32, b_start: f32, b_end: f32) -> bool {
    a_start < b_end && b_start < a_end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_case_and_punctuation() {
        let toks = normalize_tokens("Hello, WORLD! it's a test.");
        let expected: HashSet<String> =
            ["hello", "world", "its", "a", "test"].iter().map(|s| s.to_string()).collect();
        assert_eq!(toks, expected);
    }

    #[test]
    fn jaccard_of_identical_sets_is_one() {
        let a = normalize_tokens("the quick brown fox");
        let b = normalize_tokens("the quick brown fox");
        assert_eq!(jaccard_similarity(&a, &b), 1.0);
    }

    #[test]
    fn jaccard_of_disjoint_sets_is_zero() {
        let a = normalize_tokens("alpha beta gamma");
        let b = normalize_tokens("delta epsilon zeta");
        assert_eq!(jaccard_similarity(&a, &b), 0.0);
    }

    #[test]
    fn jaccard_of_partial_overlap_is_the_ratio() {
        // {a,b,c} vs {b,c,d}: intersection {b,c}=2, union {a,b,c,d}=4 -> 0.5.
        let a = normalize_tokens("a b c");
        let b = normalize_tokens("b c d");
        assert!((jaccard_similarity(&a, &b) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn jaccard_two_empty_sets_are_identical() {
        let empty = HashSet::new();
        assert_eq!(jaccard_similarity(&empty, &empty), 1.0);
    }

    #[test]
    fn echo_fires_on_near_identical_overlapping_speech() {
        // The same sentence captured twice, with the mic pass dropping one
        // filler word and mangling casing/punctuation — a real echo.
        let them = "So the deadline for the launch is next Friday.";
        let me = "so the deadline for launch is next friday";
        assert!(is_echo(me, them, DEFAULT_ECHO_THRESHOLD));
    }

    #[test]
    fn echo_does_not_fire_on_a_different_sentence() {
        let them = "The deadline for the launch is next Friday.";
        let me = "Can you also send me the design mockups later today?";
        assert!(!is_echo(me, them, DEFAULT_ECHO_THRESHOLD));
    }

    #[test]
    fn short_backchannel_is_never_an_echo() {
        // "yeah" is a legit interjection even though it appears verbatim in
        // the other speaker's line — too few tokens to be a confident echo.
        let them = "yeah that sounds good to me";
        assert!(!is_echo("yeah", them, DEFAULT_ECHO_THRESHOLD));
        assert!(!is_echo("for sure", them, DEFAULT_ECHO_THRESHOLD));
    }

    #[test]
    fn time_overlap_detects_coincident_and_rejects_disjoint() {
        assert!(time_overlaps(0.0, 5.0, 3.0, 8.0)); // overlap [3,5)
        assert!(time_overlaps(3.0, 8.0, 0.0, 5.0)); // symmetric
        assert!(!time_overlaps(0.0, 3.0, 3.0, 6.0)); // touch at boundary, no overlap
        assert!(!time_overlaps(0.0, 2.0, 10.0, 12.0)); // far apart
    }

    #[test]
    fn containment_of_a_subset_is_one() {
        let a = normalize_tokens("the next release");
        let b = normalize_tokens("please review the next release tomorrow");
        assert_eq!(containment(&a, &b), 1.0);
    }

    #[test]
    fn containment_of_an_empty_set_is_zero() {
        assert_eq!(containment(&HashSet::new(), &normalize_tokens("some words")), 0.0);
        assert_eq!(containment(&HashSet::new(), &HashSet::new()), 0.0);
    }

    #[test]
    fn echo_fires_when_me_captured_only_half_of_what_them_said() {
        let them = "Please review our updated project timeline before sending the final report tomorrow";
        let me = "before sending the final report tomorrow";
        assert_eq!(them.split_whitespace().count(), 12);
        assert_eq!(me.split_whitespace().count(), 6);
        assert!(jaccard_similarity(&normalize_tokens(me), &normalize_tokens(them)) <= 0.7);
        assert!(is_echo(me, them, DEFAULT_ECHO_THRESHOLD));
    }

    #[test]
    fn containment_does_not_fire_on_a_genuine_agreement() {
        let them = "the deadline is next Friday";
        let me = "yes I think Friday works but let's confirm with Priya first";
        assert!(!is_echo(me, them, DEFAULT_ECHO_THRESHOLD));
    }

    #[test]
    fn overlaps_within_catches_an_abutting_span() {
        assert!(overlaps_within(0.0, 9.0, 9.3, 13.0, ECHO_TIME_TOLERANCE_SECS));
        assert!(!overlaps_within(0.0, 9.0, 9.3, 13.0, 0.0));
    }

    #[test]
    fn overlaps_within_still_rejects_distant_spans() {
        assert!(!overlaps_within(0.0, 2.0, 10.0, 12.0, ECHO_TIME_TOLERANCE_SECS));
    }

}
