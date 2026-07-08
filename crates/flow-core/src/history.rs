//! Append-only dictation history at `~/.config/vzt-flow/history.jsonl`.

use std::fs::OpenOptions;
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::config_dir;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HistoryEntry {
    /// Unix epoch seconds.
    pub ts: u64,
    /// Frontmost app's bundle id at the time of dictation, if cheaply
    /// available. `None` when we couldn't determine it (or aren't on
    /// macOS).
    pub app: Option<String>,
    pub raw_text: String,
    pub duration_s: f64,
    /// Real-time factor: transcription wall time / audio duration. Lower is
    /// faster than real time.
    pub rtf: f64,
    /// The text that was actually pasted, after dictionary correction plus
    /// code-mode/cleanup. Equal to `raw_text` in `raw` mode. `#[serde(default)]`
    /// so history lines written before Phase 3 still parse.
    pub clean_text: String,
    /// The pipeline mode that produced `clean_text`: "raw", "clean",
    /// "polish", or "code".
    pub mode: String,
}

impl Default for HistoryEntry {
    fn default() -> Self {
        Self {
            ts: 0,
            app: None,
            raw_text: String::new(),
            duration_s: 0.0,
            rtf: 0.0,
            clean_text: String::new(),
            mode: "clean".to_string(),
        }
    }
}

pub fn history_path() -> Result<std::path::PathBuf> {
    Ok(config_dir()?.join("history.jsonl"))
}

pub fn append(entry: &HistoryEntry) -> Result<()> {
    let dir = config_dir()?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create {}", dir.display()))?;
    let path = history_path()?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    let line = serde_json::to_string(entry).context("failed to serialize history entry")?;
    writeln!(file, "{line}").context("failed to append history entry")?;
    Ok(())
}

/// Reads the most recent `n` history entries, newest first. Missing file
/// returns an empty list; malformed individual lines (e.g. hand-edited or
/// from an older/newer schema) are skipped rather than failing the whole
/// read.
pub fn read_recent(n: usize) -> Result<Vec<HistoryEntry>> {
    let path = history_path()?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let mut entries: Vec<HistoryEntry> = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    entries.reverse();
    entries.truncate(n);
    Ok(entries)
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_schema_lines_still_parse_with_defaults() {
        // Pre-Phase-3 history.jsonl lines have no clean_text/mode fields.
        let old_line = r#"{"ts":1,"app":null,"raw_text":"hello","duration_s":1.0,"rtf":0.1}"#;
        let entry: HistoryEntry = serde_json::from_str(old_line).unwrap();
        assert_eq!(entry.raw_text, "hello");
        assert_eq!(entry.clean_text, "");
        assert_eq!(entry.mode, "clean");
    }

    #[test]
    fn round_trips_new_fields() {
        let entry = HistoryEntry {
            ts: 42,
            app: Some("com.apple.Terminal".to_string()),
            raw_text: "get user open paren close paren".to_string(),
            duration_s: 2.0,
            rtf: 0.2,
            clean_text: "getUser()".to_string(),
            mode: "code".to_string(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let parsed: HistoryEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.clean_text, "getUser()");
        assert_eq!(parsed.mode, "code");
    }
}
