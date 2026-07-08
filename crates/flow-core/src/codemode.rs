//! Deterministic "code mode" post-processing: converts spoken-form
//! identifiers and symbol names into the punctuation/casing a terminal
//! expects, with no LLM involved (latency + "don't rewrite my shell
//! command" both argue against running cleanup here).
//!
//! # Rules
//!
//! 1. **Case keywords** (`camel case`, `snake case`, `pascal case`,
//!    `kebab case`) consume every word that follows, up to (and including,
//!    for stop purposes) whichever comes first: a symbol word/phrase, one
//!    of the recognized language keywords (see [`LANGUAGE_KEYWORDS`]), a
//!    word ASR attached trailing punctuation to (`,`/`.`/`;`/`:`/`!`/`?`),
//!    or end of input. The consumed words are joined per the requested
//!    casing.
//! 2. **Symbol words** (`open paren`, `dot`, `equals`, ...) map to their
//!    literal punctuation, longest phrase first (`fat arrow` before
//!    `arrow`, `double equals` before `equals`, etc). See
//!    [`symbol_for`].
//! 3. **Implicit call-name merge**: a run of two or more consecutive bare
//!    words (not a case keyword, symbol, or language keyword) immediately
//!    followed by an opening symbol (`(`, `{`, `[`) is treated as a
//!    function/identifier name and camelCased automatically — this is
//!    what turns `get user open paren close paren` into `getUser()`
//!    without requiring an explicit `camel case` before it. A single bare
//!    word before an opener is left as-is (already one token).
//! 4. **Language keywords** (`const`, `await`, `return`, ...) always stay
//!    literal, single tokens, and act as stops for the rules above — this
//!    is what keeps `const camel case user profile equals await get user
//!    open paren close paren` from folding `await` into the next
//!    identifier.
//! 5. The whole input is lowercased up front (stripping the sentence-case
//!    ASR adds to the first word) and trailing punctuation is stripped
//!    per-token (stripping the trailing period ASR adds to the end of an
//!    utterance, along with any other punctuation dictated words happened
//!    to pick up) rather than preserved — code mode never wants stray
//!    commas or periods.
//! 6. Output spacing: no space is inserted around `(` / `)` / `.` (call
//!    and member-access syntax), everything else gets single-space
//!    separation.

const LANGUAGE_KEYWORDS: &[&str] = &[
    "const", "let", "var", "return", "await", "async", "function", "new", "throw", "typeof",
    "instanceof", "if", "else", "for", "while", "do", "switch", "case", "break", "continue",
    "class", "extends", "import", "export", "from", "default", "static", "public", "private",
    "protected", "true", "false", "null", "undefined", "this", "super", "yield", "in", "of",
    "delete", "void", "try", "catch", "finally",
];

const TWO_WORD_SYMBOLS: &[(&str, &str, &str)] = &[
    ("double", "equals", "=="),
    ("fat", "arrow", "=>"),
    ("open", "paren", "("),
    ("close", "paren", ")"),
    ("open", "brace", "{"),
    ("close", "brace", "}"),
    ("open", "bracket", "["),
    ("close", "bracket", "]"),
    ("at", "sign", "@"),
    ("dollar", "sign", "$"),
];

const ONE_WORD_SYMBOLS: &[(&str, &str)] = &[
    ("equals", "="),
    ("arrow", "->"),
    ("pipe", "|"),
    ("ampersand", "&"),
    ("backtick", "`"),
    ("underscore", "_"),
    ("dot", "."),
    ("colon", ":"),
    ("semicolon", ";"),
    ("slash", "/"),
    ("star", "*"),
    ("plus", "+"),
    ("minus", "-"),
    ("percent", "%"),
    ("hash", "#"),
];

const OPEN_SYMBOLS: &[&str] = &["(", "{", "["];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Case {
    Camel,
    Snake,
    Pascal,
    Kebab,
}

fn case_keyword(w1: &str, w2: &str) -> Option<Case> {
    match (w1, w2) {
        ("camel", "case") => Some(Case::Camel),
        ("snake", "case") => Some(Case::Snake),
        ("pascal", "case") => Some(Case::Pascal),
        ("kebab", "case") => Some(Case::Kebab),
        _ => None,
    }
}

fn is_case_keyword_start(words: &[String], i: usize) -> bool {
    i + 1 < words.len() && case_keyword(&words[i], &words[i + 1]).is_some()
}

/// Returns `(symbol, words_consumed)` if a symbol word/phrase starts at
/// `words[i]`, checking two-word phrases before one-word ones.
fn match_symbol(words: &[String], i: usize) -> Option<(&'static str, usize)> {
    if i >= words.len() {
        return None;
    }
    if i + 1 < words.len() {
        for (a, b, sym) in TWO_WORD_SYMBOLS {
            if words[i] == *a && words[i + 1] == *b {
                return Some((sym, 2));
            }
        }
    }
    for (w, sym) in ONE_WORD_SYMBOLS {
        if words[i] == *w {
            return Some((sym, 1));
        }
    }
    None
}

fn is_symbol_start(words: &[String], i: usize) -> bool {
    match_symbol(words, i).is_some()
}

fn is_open_symbol_start(words: &[String], i: usize) -> bool {
    match_symbol(words, i).is_some_and(|(sym, _)| OPEN_SYMBOLS.contains(&sym))
}

fn is_language_keyword(w: &str) -> bool {
    LANGUAGE_KEYWORDS.contains(&w)
}

fn capitalize(w: &str) -> String {
    let mut chars = w.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn apply_case(case: Case, words: &[String]) -> String {
    if words.is_empty() {
        return String::new();
    }
    match case {
        Case::Camel => {
            let mut out = words[0].clone();
            for w in &words[1..] {
                out.push_str(&capitalize(w));
            }
            out
        }
        Case::Pascal => words.iter().map(|w| capitalize(w)).collect::<Vec<_>>().join(""),
        Case::Snake => words.join("_"),
        Case::Kebab => words.join("-"),
    }
}

/// Splits `input` into (lowercased, punctuation-stripped) word tokens plus
/// a parallel `stops` vector marking which tokens had trailing punctuation
/// stripped from them (a natural stop boundary for case-groups).
fn tokenize(input: &str) -> (Vec<String>, Vec<bool>) {
    let mut words = Vec::new();
    let mut stops = Vec::new();
    let mut pending_stop = false;
    for raw in input.split_whitespace() {
        let lower = raw.to_lowercase();
        let trimmed = lower.trim_end_matches([',', '.', ';', ':', '!', '?']);
        let had_stop = trimmed.len() != lower.len();
        if trimmed.is_empty() {
            pending_stop = true;
            continue;
        }
        words.push(trimmed.to_string());
        stops.push(had_stop || pending_stop);
        pending_stop = false;
    }
    (words, stops)
}

/// No space before `)`, before/after `(`, or around `.` — call and
/// member-access syntax read wrong with spaces in them.
fn is_tight_before(a: &str) -> bool {
    a == ")" || a == "(" || a == "."
}
fn is_tight_after(a: &str) -> bool {
    a == "(" || a == "."
}

fn join_atoms(atoms: &[String]) -> String {
    let mut out = String::new();
    for (i, a) in atoms.iter().enumerate() {
        if i > 0 {
            let prev = &atoms[i - 1];
            if !is_tight_after(prev) && !is_tight_before(a) {
                out.push(' ');
            }
        }
        out.push_str(a);
    }
    out
}

/// Transforms spoken-form code dictation into symbol-laden text. Pure
/// function: no I/O, no LLM, safe to call inline on the coordinator thread.
pub fn transform(input: &str) -> String {
    let (words, stops) = tokenize(input);
    let mut atoms: Vec<String> = Vec::new();
    let mut i = 0;

    while i < words.len() {
        if is_case_keyword_start(&words, i) {
            let case = case_keyword(&words[i], &words[i + 1]).unwrap();
            let mut j = i + 2;
            let mut group: Vec<String> = Vec::new();
            loop {
                if j >= words.len() {
                    break;
                }
                if is_symbol_start(&words, j) || is_case_keyword_start(&words, j) || is_language_keyword(&words[j]) {
                    break;
                }
                group.push(words[j].clone());
                let had_stop = stops[j];
                j += 1;
                if had_stop {
                    break;
                }
            }
            let rendered = apply_case(case, &group);
            if !rendered.is_empty() {
                atoms.push(rendered);
            }
            i = j;
            continue;
        }

        if let Some((sym, consumed)) = match_symbol(&words, i) {
            atoms.push(sym.to_string());
            i += consumed;
            continue;
        }

        if is_language_keyword(&words[i]) {
            atoms.push(words[i].clone());
            i += 1;
            continue;
        }

        // Bare word: gather the maximal run of bare (non-symbol,
        // non-case-keyword, non-language-keyword) words.
        let mut j = i;
        let mut run: Vec<String> = Vec::new();
        loop {
            if j >= words.len() {
                break;
            }
            if is_symbol_start(&words, j) || is_case_keyword_start(&words, j) || is_language_keyword(&words[j]) {
                break;
            }
            run.push(words[j].clone());
            let had_stop = stops[j];
            j += 1;
            if had_stop {
                break;
            }
        }
        let next_is_open = is_open_symbol_start(&words, j);
        if run.len() >= 2 && next_is_open {
            atoms.push(apply_case(Case::Camel, &run));
        } else {
            atoms.extend(run);
        }
        i = j;
    }

    join_atoms(&atoms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camel_case_basic() {
        assert_eq!(transform("camel case user id"), "userId");
    }

    #[test]
    fn snake_case_basic() {
        assert_eq!(transform("snake case api key"), "api_key");
    }

    #[test]
    fn pascal_case_basic() {
        assert_eq!(transform("pascal case flow core"), "FlowCore");
    }

    #[test]
    fn kebab_case_basic() {
        assert_eq!(transform("kebab case my app"), "my-app");
    }

    #[test]
    fn parens_are_tight() {
        assert_eq!(transform("open paren close paren"), "()");
    }

    #[test]
    fn equals_symbol() {
        assert_eq!(transform("equals"), "=");
    }

    #[test]
    fn double_equals_beats_equals() {
        assert_eq!(transform("double equals"), "==");
    }

    #[test]
    fn arrow_symbol() {
        assert_eq!(transform("arrow"), "->");
    }

    #[test]
    fn fat_arrow_beats_arrow() {
        assert_eq!(transform("fat arrow"), "=>");
    }

    #[test]
    fn dot_call_reads_like_member_access() {
        assert_eq!(transform("console dot log open paren close paren"), "console.log()");
    }

    #[test]
    fn full_worked_example_from_the_brief() {
        assert_eq!(
            transform("const camel case user profile equals await get user open paren close paren"),
            "const userProfile = await getUser()"
        );
    }

    #[test]
    fn strips_sentence_casing_and_trailing_period() {
        // ASR capitalizes the first word and appends a period; both must
        // vanish, and the implicit call-name merge still fires.
        assert_eq!(transform("Get user open paren close paren."), "getUser()");
    }

    #[test]
    fn case_group_stops_at_language_keyword() {
        assert_eq!(transform("snake case api key const x"), "api_key const x");
    }

    #[test]
    fn case_group_stops_at_dictated_punctuation() {
        // The comma is a stop boundary and is dropped, not preserved.
        assert_eq!(transform("kebab case my app, then const y"), "my-app then const y");
    }

    #[test]
    fn brace_and_bracket_symbols() {
        assert_eq!(transform("open brace close brace open bracket close bracket"), "{ } [ ]");
    }

    #[test]
    fn misc_symbol_words() {
        assert_eq!(
            transform("pipe ampersand backtick colon semicolon slash star plus minus percent hash"),
            "| & ` : ; / * + - % #"
        );
    }

    #[test]
    fn dollar_sign_and_at_sign_and_underscore() {
        assert_eq!(transform("dollar sign underscore at sign"), "$ _ @");
    }

    #[test]
    fn single_bare_word_before_paren_is_not_merged_oddly() {
        assert_eq!(transform("log open paren close paren"), "log()");
    }

    #[test]
    fn implicit_merge_only_applies_directly_before_an_opener() {
        // No opener follows, so "get user" stays two separate words rather
        // than being guessed into an identifier.
        assert_eq!(transform("get user"), "get user");
    }
}
