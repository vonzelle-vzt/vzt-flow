//! User-editable settings persisted at `~/.config/vzt-flow/config.toml`.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// macOS virtual keycode for Right Option (kVK_RightOption), the default
/// hold-to-talk key. Chosen because it's rarely bound to anything else and
/// is reachable with the thumb on both hands.
pub const DEFAULT_HOTKEY_KEYCODE: u16 = 61;

/// macOS virtual keycode for Escape (kVK_Escape), used to cancel an
/// in-progress recording.
pub const ESCAPE_KEYCODE: u16 = 53;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// macOS virtual keycode of the hold-to-talk key.
    pub hotkey_keycode: u16,
    /// Human-readable label for the current binding, shown in Settings.
    pub hotkey_label: String,
    /// Minimum hold duration (ms) before a press counts as "hold to talk"
    /// rather than a tap that toggles hands-free recording.
    pub hold_threshold_ms: u64,
    /// Seconds of transcriber inactivity before the model is unloaded.
    pub idle_unload_secs: u64,
    /// Launch the app at login (mirrors tauri-plugin-autostart state; kept
    /// here too so Settings can render it without an async round-trip).
    pub launch_at_login: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            hotkey_keycode: DEFAULT_HOTKEY_KEYCODE,
            hotkey_label: "Right Option".to_string(),
            hold_threshold_ms: 300,
            idle_unload_secs: 300,
            launch_at_login: false,
        }
    }
}

pub fn config_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("could not determine home directory")?;
    Ok(home.join(".config").join("vzt-flow"))
}

pub fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_path()?;
        if !path.exists() {
            let cfg = Self::default();
            cfg.save()?;
            return Ok(cfg);
        }
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let cfg: Self = toml::from_str(&raw)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        let dir = config_dir()?;
        fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
        let raw = toml::to_string_pretty(self).context("failed to serialize config")?;
        fs::write(config_path()?, raw).context("failed to write config.toml")?;
        Ok(())
    }
}
