//! Personal dictionary: corrects ASR mishearings of proper nouns (product
//! names, brand names, jargon) before the cleanup/codemode pass ever sees
//! the transcript.
//!
//! Persisted at `~/.config/vzt-flow/dictionary.json` as a simple array of
//! `{"term": "...", "hints": ["...", ...]}` entries. `hints` are alternate
//! spellings/mishearings the ASR is known to produce; the term itself
//! (lowercased) is always an implicit candidate too, so a dictionary entry
//! with no hints still fixes casing (e.g. "typescript" -> "TypeScript").

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::config_dir;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DictionaryTerm {
    pub term: String,
    #[serde(default)]
    pub hints: Vec<String>,
}

fn term(term: &str, hints: &[&str]) -> DictionaryTerm {
    DictionaryTerm {
        term: term.to_string(),
        hints: hints.iter().map(|s| s.to_string()).collect(),
    }
}

/// The seed dictionary written on first run.
pub fn seed_dictionary() -> Vec<DictionaryTerm> {
    vec![
        term("Supabase", &["superbase", "super base"]),
        term("Whop", &["whopp", "wop", "wap"]),
        term("VZT", &[]),
        term("Resend", &[]),
        term("Vercel", &["versel", "verscel"]),
        term("Tauri", &["tory", "torii"]),
        term("Parakeet", &[]),
        term("TradeScriptAI", &["trade script ai", "trade script a i"]),
        term("FlagPlay", &["flag play"]),
        term("NextPlay", &["next play"]),
        term("Anthropic", &[]),
        term("Claude", &["clawd"]),
        term("Stripe", &[]),
        term("Expo", &[]),
        term("Postgres", &["postgres sql", "post grass"]),
        term("Next.js", &["next js", "next dot js"]),
        term("TypeScript", &["type script"]),
        term("Whisper", &[]),
        // Explicit term replacement rather than a spelling fix.
        term("VZT Flow", &["wispr flow", "whisper flow"]),
    ]
}

pub fn dictionary_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("dictionary.json"))
}

/// Loads the dictionary, seeding the file on first run.
pub fn load_or_seed() -> Result<Vec<DictionaryTerm>> {
    let path = dictionary_path()?;
    if !path.exists() {
        let seed = seed_dictionary();
        save(&seed)?;
        return Ok(seed);
    }
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))
}

pub fn save(entries: &[DictionaryTerm]) -> Result<()> {
    let dir = config_dir()?;
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let raw = serde_json::to_string_pretty(entries).context("failed to serialize dictionary")?;
    fs::write(dictionary_path()?, raw).context("failed to write dictionary.json")?;
    Ok(())
}

/// Minimum canonical-term length a fuzzy (edit-distance) match may fire on.
/// Below this, only an exact (case-insensitive) match replaces — otherwise
/// short terms like "VZT" would fuzzy-match all sorts of unrelated words.
const MIN_FUZZY_TERM_LEN: usize = 4;

/// Edit-distance budget for a candidate phrase of `len` characters.
fn distance_threshold(len: usize) -> usize {
    (len / 4).max(1)
}

/// Applies dictionary corrections to `text`, replacing recognized
/// mishearings/hints with their canonical spelling. Word-boundary aware and
/// case-insensitive; never fires on terms shorter than
/// [`MIN_FUZZY_TERM_LEN`] unless the match is exact.
pub fn correct(text: &str, dictionary: &[DictionaryTerm]) -> String {
    if dictionary.is_empty() || text.trim().is_empty() {
        return text.to_string();
    }

    // Each candidate is (lowercased phrase words, canonical replacement).
    // Built as owned Strings up front so nothing borrows from a temporary.
    let mut candidates: Vec<(Vec<String>, &str)> = Vec::new();
    for entry in dictionary {
        let words: Vec<String> = entry
            .term
            .to_lowercase()
            .split_whitespace()
            .map(|w| w.to_string())
            .collect();
        if !words.is_empty() {
            candidates.push((words, entry.term.as_str()));
        }
        for hint in &entry.hints {
            let words: Vec<String> = hint
                .to_lowercase()
                .split_whitespace()
                .map(|w| w.to_string())
                .collect();
            if !words.is_empty() {
                candidates.push((words, entry.term.as_str()));
            }
        }
    }

    let max_words = candidates.iter().map(|(w, _)| w.len()).max().unwrap_or(1);

    // Tokenize into words + the separator text that followed each one, so
    // the corrected text can be reassembled exactly.
    let tokens = tokenize(text);
    let mut out = String::new();
    let mut i = 0;
    while i < tokens.len() {
        let mut replaced = false;
        'window: for window in (1..=max_words.min(tokens.len() - i)).rev() {
            let phrase: Vec<String> =
                tokens[i..i + window].iter().map(|t| t.word.to_lowercase()).collect();
            for (cand_words, canonical) in &candidates {
                if cand_words.len() != window {
                    continue;
                }
                if !words_match(&phrase, cand_words) {
                    continue;
                }
                out.push_str(canonical);
                // Preserve the trailing separator of the last consumed token.
                out.push_str(&tokens[i + window - 1].sep);
                i += window;
                replaced = true;
                break 'window;
            }
        }
        if !replaced {
            out.push_str(&tokens[i].word);
            out.push_str(&tokens[i].sep);
            i += 1;
        }
    }
    out
}

/// Compares a spoken phrase to a candidate phrase **word by word** (never as
/// one joined string) so an edit "budget" on one word can't leak across a
/// word boundary and change how many words match — e.g. without this,
/// "vzt now" vs. the two-word candidate "vzt flow" would pass at a
/// whole-phrase edit distance of 2, even though "now" and "flow" aren't a
/// sane fuzzy match on their own.
fn words_match(phrase_words: &[String], candidate_words: &[String]) -> bool {
    if phrase_words.len() != candidate_words.len() {
        return false;
    }
    phrase_words.iter().zip(candidate_words.iter()).all(|(p, c)| {
        if p == c {
            return true;
        }
        if c.chars().count() < MIN_FUZZY_TERM_LEN {
            return false; // exact match only for short words
        }
        let threshold = distance_threshold(c.chars().count());
        strsim::levenshtein(p, c) <= threshold
    })
}

struct Token {
    word: String,
    /// Non-word text (spaces/punctuation) immediately following this word,
    /// up to (but not including) the next word.
    sep: String,
}

/// Splits `text` into alphanumeric (plus `'`/`-` interior) word tokens and
/// the separator text between them, preserving enough information to
/// reconstruct the original string byte-for-byte when no replacement fires.
fn tokenize(text: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut chars = text.char_indices().peekable();
    let mut cur_word = String::new();
    let mut cur_sep = String::new();
    let is_word_char = |c: char| c.is_alphanumeric() || c == '\'' || c == '-';

    // Leading separator (rare, e.g. leading whitespace) gets attached to a
    // synthetic empty-word token so it's never lost.
    let mut pending_leading_sep = String::new();
    let mut started_word = false;

    while let Some((_, c)) = chars.next() {
        if is_word_char(c) {
            if !cur_word.is_empty() && !cur_sep.is_empty() {
                // Flush the previous word+sep pair.
                tokens.push(Token { word: std::mem::take(&mut cur_word), sep: std::mem::take(&mut cur_sep) });
            }
            if !started_word && !pending_leading_sep.is_empty() {
                tokens.push(Token { word: String::new(), sep: std::mem::take(&mut pending_leading_sep) });
            }
            started_word = true;
            cur_word.push(c);
        } else {
            if cur_word.is_empty() {
                pending_leading_sep.push(c);
            } else {
                cur_sep.push(c);
            }
        }
    }
    if !cur_word.is_empty() || !cur_sep.is_empty() {
        tokens.push(Token { word: cur_word, sep: cur_sep });
    } else if !pending_leading_sep.is_empty() {
        tokens.push(Token { word: String::new(), sep: pending_leading_sep });
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dict() -> Vec<DictionaryTerm> {
        seed_dictionary()
    }

    #[test]
    fn corrects_superbase_to_supabase() {
        let out = correct("I use superbase for the backend", &dict());
        assert_eq!(out, "I use Supabase for the backend");
    }

    #[test]
    fn corrects_wap_and_whopp_to_whop() {
        assert_eq!(correct("check wap for the plan", &dict()), "check Whop for the plan");
        assert_eq!(correct("check whopp for the plan", &dict()), "check Whop for the plan");
    }

    #[test]
    fn does_not_corrupt_wrap() {
        // "wrap" must never be mistaken for "Whop" (edit distance 2 > threshold 1).
        assert_eq!(correct("please wrap this up", &dict()), "please wrap this up");
    }

    #[test]
    fn fixes_casing_with_no_explicit_hints() {
        assert_eq!(correct("we use typescript everywhere", &dict()), "we use TypeScript everywhere");
        assert_eq!(correct("built with postgres sql", &dict()), "built with Postgres");
    }

    #[test]
    fn short_terms_require_exact_match() {
        // "VZT" (3 chars) must not fuzzy-fire on nearby short words.
        assert_eq!(correct("the vet clinic called", &dict()), "the vet clinic called");
        assert_eq!(correct("ping vzt now", &dict()), "ping VZT now");
    }

    #[test]
    fn replaces_wispr_flow_with_vzt_flow() {
        assert_eq!(correct("open wispr flow please", &dict()), "open VZT Flow please");
    }

    #[test]
    fn empty_dictionary_is_noop() {
        assert_eq!(correct("hello world", &[]), "hello world");
    }

    #[test]
    fn preserves_punctuation_and_spacing() {
        let out = correct("Deploying to vercel, then verscel again.", &dict());
        assert_eq!(out, "Deploying to Vercel, then Vercel again.");
    }
}
