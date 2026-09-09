//! Transcript accuracy metrics. Word-level edit distance plus an alignment
//! diff, so a change can be reported as "WER moved from X to Y, and it turned
//! N previously-correct words wrong" rather than as an aggregate that hides
//! regressions inside an average.

/// Lowercased, punctuation-stripped word tokens (same normalization as
/// `meeting::dedup::normalize_tokens`, but order-preserving).
pub fn tokenize(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(|w| w.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase())
        .filter(|w| !w.is_empty())
        .collect()
}

/// Result of aligning a hypothesis against a reference transcript via
/// Levenshtein edit distance over word tokens.
#[derive(Debug, Clone, PartialEq)]
pub struct WerReport {
    pub reference_words: usize,
    pub substitutions: usize,
    pub deletions: usize,
    pub insertions: usize,
    /// Indices into the reference of words the hypothesis matched exactly.
    pub matched: Vec<usize>,
}

impl WerReport {
    /// Word error rate: (S + D + I) / reference_words. A reference with zero
    /// words is defined as 0.0 if the hypothesis is also empty (nothing to
    /// get wrong), else 1.0 (all insertions against nothing to divide by).
    pub fn wer(&self) -> f64 {
        if self.reference_words == 0 {
            return if self.insertions == 0 { 0.0 } else { 1.0 };
        }
        (self.substitutions + self.deletions + self.insertions) as f64 / self.reference_words as f64
    }
}

/// Levenshtein alignment of `hypothesis` against `reference`, with backtrace,
/// operating on [`tokenize`]d words.
pub fn wer(reference: &str, hypothesis: &str) -> WerReport {
    let r = tokenize(reference);
    let h = tokenize(hypothesis);
    align(&r, &h)
}

/// Standard DP edit-distance alignment with backtrace, shared by [`wer`] and
/// [`harmful_helpful`] (both align a hypothesis against the same reference
/// tokens).
fn align(r: &[String], h: &[String]) -> WerReport {
    let n = r.len();
    let m = h.len();
    // dp[i][j] = min edit distance between r[..i] and h[..j].
    let mut dp = vec![vec![0usize; m + 1]; n + 1];
    for i in 0..=n {
        dp[i][0] = i;
    }
    for j in 0..=m {
        dp[0][j] = j;
    }
    for i in 1..=n {
        for j in 1..=m {
            if r[i - 1] == h[j - 1] {
                dp[i][j] = dp[i - 1][j - 1];
            } else {
                dp[i][j] = 1 + dp[i - 1][j - 1].min(dp[i - 1][j]).min(dp[i][j - 1]);
            }
        }
    }

    // Backtrace from (n, m) to (0, 0), preferring a match/substitution
    // (diagonal) over deletion/insertion when costs tie, so the reported
    // alignment is the "natural" one word processors expect.
    let mut substitutions = 0;
    let mut deletions = 0;
    let mut insertions = 0;
    let mut matched = Vec::new();
    let (mut i, mut j) = (n, m);
    while i > 0 || j > 0 {
        if i > 0 && j > 0 && r[i - 1] == h[j - 1] && dp[i][j] == dp[i - 1][j - 1] {
            matched.push(i - 1);
            i -= 1;
            j -= 1;
        } else if i > 0 && j > 0 && dp[i][j] == dp[i - 1][j - 1] + 1 {
            substitutions += 1;
            i -= 1;
            j -= 1;
        } else if i > 0 && dp[i][j] == dp[i - 1][j] + 1 {
            deletions += 1;
            i -= 1;
        } else {
            insertions += 1;
            j -= 1;
        }
    }
    matched.reverse();

    WerReport { reference_words: n, substitutions, deletions, insertions, matched }
}

/// Words the baseline got right that the candidate got wrong (`harmful`) and
/// vice versa (`helpful`). This is the number that decides whether a change
/// ships — a WER tie with 12 harmful and 12 helpful changes is not a tie.
pub fn harmful_helpful(reference: &str, baseline: &str, candidate: &str) -> (usize, usize) {
    let r = tokenize(reference);
    let base_h = tokenize(baseline);
    let cand_h = tokenize(candidate);

    let base_report = align(&r, &base_h);
    let cand_report = align(&r, &cand_h);

    let base_matched: std::collections::HashSet<usize> = base_report.matched.into_iter().collect();
    let cand_matched: std::collections::HashSet<usize> = cand_report.matched.into_iter().collect();

    let harmful = base_matched.difference(&cand_matched).count();
    let helpful = cand_matched.difference(&base_matched).count();
    (harmful, helpful)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wer_of_identical_text_is_zero() {
        let report = wer("the cat sat on the mat", "the cat sat on the mat");
        assert_eq!(report.wer(), 0.0);
        assert_eq!(report.substitutions, 0);
        assert_eq!(report.deletions, 0);
        assert_eq!(report.insertions, 0);
    }

    #[test]
    fn wer_counts_a_substitution() {
        let report = wer("the cat sat", "the bat sat");
        assert_eq!(report.substitutions, 1);
        assert_eq!(report.deletions, 0);
        assert_eq!(report.insertions, 0);
        assert_eq!(report.wer(), 1.0 / 3.0);
    }

    #[test]
    fn wer_counts_a_deletion() {
        let report = wer("the cat sat on the mat", "the cat on the mat");
        assert_eq!(report.deletions, 1);
        assert_eq!(report.substitutions, 0);
        assert_eq!(report.insertions, 0);
        assert_eq!(report.wer(), 1.0 / 6.0);
    }

    #[test]
    fn wer_counts_an_insertion() {
        let report = wer("the cat sat", "the cat really sat");
        assert_eq!(report.insertions, 1);
        assert_eq!(report.substitutions, 0);
        assert_eq!(report.deletions, 0);
        assert_eq!(report.wer(), 1.0 / 3.0);
    }

    #[test]
    fn wer_is_case_and_punctuation_insensitive() {
        let report = wer("The Cat, sat!", "the cat sat");
        assert_eq!(report.wer(), 0.0);
    }

    #[test]
    fn wer_of_an_empty_hypothesis_is_one() {
        let report = wer("the cat sat", "");
        assert_eq!(report.wer(), 1.0);
        assert_eq!(report.deletions, 3);
    }

    #[test]
    fn harmful_helpful_flags_a_word_the_candidate_broke() {
        let (harmful, helpful) = harmful_helpful("the cat sat", "the cat sat", "the bat sat");
        assert_eq!((harmful, helpful), (1, 0));
    }

    #[test]
    fn harmful_helpful_flags_a_word_the_candidate_fixed() {
        let (harmful, helpful) = harmful_helpful("the cat sat", "the bat sat", "the cat sat");
        assert_eq!((harmful, helpful), (0, 1));
    }

    #[test]
    fn harmful_helpful_is_zero_when_both_match() {
        let (harmful, helpful) = harmful_helpful("the cat sat", "the cat sat", "the cat sat");
        assert_eq!((harmful, helpful), (0, 0));
    }
}
