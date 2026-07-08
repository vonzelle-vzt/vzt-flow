//! Voice snippets: dictating a trigger phrase (optionally prefixed with
//! "insert") expands to fixed text instead of being pasted literally.
//!
//! Persisted at `~/.config/vzt-flow/snippets.json` as `{"trigger": "expansion"}`.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::config::config_dir;

pub type Snippets = HashMap<String, String>;

pub fn seed_snippets() -> Snippets {
    let mut map = HashMap::new();
    map.insert("my email".to_string(), "you@example.com".to_string());
    map
}

pub fn snippets_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("snippets.json"))
}

pub fn load_or_seed() -> Result<Snippets> {
    let path = snippets_path()?;
    if !path.exists() {
        let seed = seed_snippets();
        save(&seed)?;
        return Ok(seed);
    }
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))
}

pub fn save(snippets: &Snippets) -> Result<()> {
    let dir = config_dir()?;
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let raw = serde_json::to_string_pretty(snippets).context("failed to serialize snippets")?;
    fs::write(snippets_path()?, raw).context("failed to write snippets.json")?;
    Ok(())
}

/// Case/punctuation-insensitive normalization: lowercase, strip anything
/// that isn't alphanumeric or whitespace, collapse whitespace runs.
fn normalize(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if c.is_alphanumeric() || c.is_whitespace() { c.to_ascii_lowercase() } else { ' ' })
        .collect();
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// If the *entire* cleaned transcript matches a snippet trigger (either the
/// bare trigger, or "insert <trigger>"), returns the expansion text.
/// Case/punctuation-insensitive; the whole transcript must match, not a
/// substring, so ordinary dictation containing the trigger phrase mid
/// sentence is left alone.
pub fn expand(transcript: &str, snippets: &Snippets) -> Option<String> {
    let normalized = normalize(transcript);
    if normalized.is_empty() {
        return None;
    }
    for (trigger, expansion) in snippets {
        let norm_trigger = normalize(trigger);
        if norm_trigger.is_empty() {
            continue;
        }
        if normalized == norm_trigger {
            return Some(expansion.clone());
        }
        let with_insert = format!("insert {norm_trigger}");
        if normalized == with_insert {
            return Some(expansion.clone());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snippets() -> Snippets {
        seed_snippets()
    }

    #[test]
    fn expands_bare_trigger() {
        assert_eq!(expand("my email", &snippets()), Some("you@example.com".to_string()));
    }

    #[test]
    fn expands_with_insert_prefix() {
        assert_eq!(
            expand("insert my email", &snippets()),
            Some("you@example.com".to_string())
        );
    }

    #[test]
    fn is_case_and_punctuation_insensitive() {
        assert_eq!(
            expand("My Email!", &snippets()),
            Some("you@example.com".to_string())
        );
        assert_eq!(
            expand("Insert, My Email.", &snippets()),
            Some("you@example.com".to_string())
        );
    }

    #[test]
    fn does_not_fire_mid_sentence() {
        assert_eq!(expand("please send my email to the team", &snippets()), None);
    }

    #[test]
    fn no_match_returns_none() {
        assert_eq!(expand("what's the weather", &snippets()), None);
    }
}
