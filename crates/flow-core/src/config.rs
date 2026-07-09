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

/// Environment variable that overrides the config directory. Additive and
/// useful forever (scripted/isolated runs, tests) — lets a second app
/// instance point at its own `config.toml` / `daemon.sock` / meeting output
/// without touching the user's real `~/.config/vzt-flow`.
pub const CONFIG_DIR_ENV: &str = "VZT_FLOW_CONFIG_DIR";

/// Auto-detect behavior for meetings (see `meeting::detect`). Serialized as
/// the lowercase string stored in [`Config::meeting_auto`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeetingAuto {
    /// Detect a call, then ask (via notification) before transcribing.
    Ask,
    /// Detect a call and start transcribing immediately.
    Auto,
    /// Never auto-detect; the tray's manual Start/Stop is the only path.
    Off,
}

impl MeetingAuto {
    /// Parses the config string, defaulting to [`MeetingAuto::Ask`] for any
    /// unrecognized value so a typo never silently disables detection.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => MeetingAuto::Auto,
            "off" => MeetingAuto::Off,
            _ => MeetingAuto::Ask,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            MeetingAuto::Ask => "ask",
            MeetingAuto::Auto => "auto",
            MeetingAuto::Off => "off",
        }
    }
}

/// Default value for [`Config::meeting_auto`] — kept as a free fn so serde's
/// `#[serde(default)]` populates it when an older `config.toml` predates the
/// field.
fn default_meeting_auto() -> String {
    MeetingAuto::Ask.as_str().to_string()
}

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
    /// Hard cap (seconds) on a single hold-to-talk recording. When reached
    /// the recording auto-stops and what was captured is transcribed (not
    /// discarded), so a stuck key can never record forever. Raised to 600s
    /// (10min) to make long-form dictation (users holding Right Option for
    /// several minutes at a stretch) first-class rather than an edge case.
    pub max_hold_secs: u64,
    /// Hard cap (seconds) on a single hands-free (tap-to-toggle) recording.
    /// Kept equal to `max_hold_secs` now that both are sized for long-form
    /// dictation rather than "hold" being the short one.
    pub max_handsfree_secs: u64,
    /// Launch the app at login (mirrors tauri-plugin-autostart state; kept
    /// here too so Settings can render it without an async round-trip).
    pub launch_at_login: bool,
    /// Base component (ms) of the LLM cleanup deadline — see
    /// `cleanup_manager::cleanup_deadline_ms` for the full formula, which
    /// adds `cleanup_timeout_per_char_ms` per input character (capped at
    /// `cleanup_timeout_max_ms`) so long dictations get a proportionally
    /// longer window instead of racing a flat deadline sized for a short
    /// sentence. If generation (model load + inference) hasn't produced a
    /// result by the computed deadline, the raw (dictionary-corrected)
    /// transcript is pasted instead — cleanup must never block a dictation
    /// past that deadline.
    pub cleanup_timeout_ms: u64,
    /// Additional cleanup deadline (ms) granted per character of input —
    /// see `cleanup_manager::cleanup_deadline_ms` for the derivation
    /// (~6ms/char, from Qwen3-1.7B-Q4_K_M's measured 40-60 tok/s decode
    /// speed on M5).
    pub cleanup_timeout_per_char_ms: u64,
    /// Absolute ceiling (ms) on the cleanup deadline regardless of input
    /// length — a single dictation can never make the user wait longer
    /// than this for the LLM before falling back to the raw transcript.
    pub cleanup_timeout_max_ms: u64,
    /// Seconds of continuous sub-threshold audio (following at least one
    /// loud frame) before a hands-free recording auto-stops.
    pub handsfree_silence_secs: f64,
    /// Whether `clean`/`polish` LLM cleanup is allowed to run at all.
    /// Defaults to `true`; set `false` to force every profile to behave as
    /// if it were `raw` mode without editing `profiles.toml`. Exists for
    /// low-RAM machines (e.g. 8GB Intel Macs) where the ~1.1GB cleanup GGUF
    /// plus its inference context is a meaningful chunk of available
    /// memory — flipping this off skips downloading/loading it entirely,
    /// at the call sites that check it (see `crates/flow-cli`'s standalone
    /// pipeline; the desktop daemon path is not yet wired to this flag).
    pub cleanup_enabled: bool,
    /// Meeting auto-detection behavior: `"ask"` (default) shows a notification
    /// when a Zoom/Meet/Teams call is detected, `"auto"` starts transcribing
    /// immediately, `"off"` disables detection entirely. Parsed via
    /// [`MeetingAuto::parse`]; see `meeting::detect` for the detection logic.
    /// `#[serde(default)]` on the struct means a `config.toml` written before
    /// this field existed loads fine and gets `"ask"`.
    #[serde(default = "default_meeting_auto")]
    pub meeting_auto: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            hotkey_keycode: DEFAULT_HOTKEY_KEYCODE,
            hotkey_label: "Right Option".to_string(),
            hold_threshold_ms: 300,
            idle_unload_secs: 300,
            max_hold_secs: 600,
            max_handsfree_secs: 600,
            launch_at_login: false,
            cleanup_timeout_ms: 2500,
            cleanup_timeout_per_char_ms: 6,
            cleanup_timeout_max_ms: 20_000,
            handsfree_silence_secs: 2.5,
            cleanup_enabled: true,
            meeting_auto: default_meeting_auto(),
        }
    }
}

impl Config {
    /// Typed view of [`Config::meeting_auto`].
    pub fn meeting_auto_mode(&self) -> MeetingAuto {
        MeetingAuto::parse(&self.meeting_auto)
    }
}

/// `~/.config/vzt-flow` on macOS (deliberately not `dirs::config_dir()`,
/// which would resolve to `~/Library/Application Support` — every existing
/// install and this module's own doc comments assume the literal
/// `~/.config` path). On Windows there is no `~/.config` convention, so we
/// use `dirs::config_dir()` there instead, which resolves to `%APPDATA%`.
pub fn config_dir() -> Result<PathBuf> {
    // An explicit override wins on every platform (isolated/scripted runs,
    // tests) — see [`CONFIG_DIR_ENV`].
    if let Some(dir) = std::env::var_os(CONFIG_DIR_ENV) {
        if !dir.is_empty() {
            return Ok(PathBuf::from(dir));
        }
    }
    #[cfg(target_os = "macos")]
    {
        let home = dirs::home_dir().context("could not determine home directory")?;
        Ok(home.join(".config").join("vzt-flow"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let base = dirs::config_dir().context("could not determine config directory")?;
        Ok(base.join("vzt-flow"))
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A `config.toml` written before `meeting_auto` existed must still load,
    /// defaulting the new field to `"ask"` (the additive-field contract).
    #[test]
    fn old_config_without_meeting_auto_loads_and_defaults_to_ask() {
        let old = r#"
            hotkey_keycode = 61
            hotkey_label = "Right Option"
            hold_threshold_ms = 300
            idle_unload_secs = 300
            max_hold_secs = 600
            max_handsfree_secs = 600
            launch_at_login = false
            cleanup_timeout_ms = 2500
            cleanup_timeout_per_char_ms = 6
            cleanup_timeout_max_ms = 20000
            handsfree_silence_secs = 2.5
            cleanup_enabled = true
        "#;
        let cfg: Config = toml::from_str(old).expect("old config must still parse");
        assert_eq!(cfg.meeting_auto, "ask");
        assert_eq!(cfg.meeting_auto_mode(), MeetingAuto::Ask);
    }

    #[test]
    fn meeting_auto_round_trips_and_parses() {
        let mut cfg = Config::default();
        cfg.meeting_auto = "auto".to_string();
        let raw = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&raw).unwrap();
        assert_eq!(back.meeting_auto_mode(), MeetingAuto::Auto);
    }

    #[test]
    fn meeting_auto_parse_is_lenient() {
        assert_eq!(MeetingAuto::parse("AUTO"), MeetingAuto::Auto);
        assert_eq!(MeetingAuto::parse(" off "), MeetingAuto::Off);
        assert_eq!(MeetingAuto::parse("ask"), MeetingAuto::Ask);
        // Unrecognized values default to Ask, never silently disabling detection.
        assert_eq!(MeetingAuto::parse("banana"), MeetingAuto::Ask);
    }
}
