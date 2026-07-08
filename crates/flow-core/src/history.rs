//! Append-only dictation history at `~/.config/vzt-flow/history.jsonl`.

use std::fs::OpenOptions;
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::config_dir;

#[derive(Debug, Clone, Serialize, Deserialize)]
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

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
