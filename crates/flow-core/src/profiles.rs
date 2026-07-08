//! Per-app cleanup profiles: maps the frontmost app's bundle id to a
//! `{mode, tone}` pair so a terminal gets `code` mode (no LLM rewriting) and
//! Mail gets `clean` + `formal`, etc.
//!
//! Persisted at `~/.config/vzt-flow/profiles.toml`. Read-only from the
//! Settings UI — the file path is shown there for manual editing.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::config_dir;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileRule {
    /// One of "raw", "clean", "polish", "code".
    pub mode: String,
    /// One of "neutral", "formal", "casual" (free-form; passed through to
    /// the cleanup prompt as a hint).
    pub tone: String,
}

impl Default for ProfileRule {
    fn default() -> Self {
        Self { mode: "clean".to_string(), tone: "neutral".to_string() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profiles {
    #[serde(default)]
    pub default: ProfileRule,
    /// Bundle-id (optionally trailing-`*` glob) -> rule. `BTreeMap` for
    /// stable, human-readable serialization order.
    #[serde(flatten)]
    pub apps: BTreeMap<String, ProfileRule>,
}

impl Default for Profiles {
    fn default() -> Self {
        seed_profiles()
    }
}

pub fn seed_profiles() -> Profiles {
    let mut apps = BTreeMap::new();
    let code = ProfileRule { mode: "code".to_string(), tone: "neutral".to_string() };
    apps.insert("com.apple.Terminal".to_string(), code.clone());
    apps.insert("com.googlecode.iterm2".to_string(), code.clone());
    apps.insert("dev.warp.Warp".to_string(), code);
    apps.insert(
        "com.apple.mail".to_string(),
        ProfileRule { mode: "clean".to_string(), tone: "formal".to_string() },
    );
    apps.insert(
        "com.tinyspeck.slackmacgap".to_string(),
        ProfileRule { mode: "clean".to_string(), tone: "casual".to_string() },
    );
    Profiles { default: ProfileRule::default(), apps }
}

pub fn profiles_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("profiles.toml"))
}

pub fn load_or_seed() -> Result<Profiles> {
    let path = profiles_path()?;
    if !path.exists() {
        let seed = seed_profiles();
        save(&seed)?;
        return Ok(seed);
    }
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    toml::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))
}

pub fn save(profiles: &Profiles) -> Result<()> {
    let dir = config_dir()?;
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let raw = toml::to_string_pretty(profiles).context("failed to serialize profiles")?;
    fs::write(profiles_path()?, raw).context("failed to write profiles.toml")?;
    Ok(())
}

/// Case-insensitive match of `bundle_id` against `pattern`, where a
/// trailing `*` in `pattern` means "starts with".
fn glob_match(pattern: &str, bundle_id: &str) -> bool {
    let pattern = pattern.to_lowercase();
    let bundle_id = bundle_id.to_lowercase();
    if let Some(prefix) = pattern.strip_suffix('*') {
        bundle_id.starts_with(prefix)
    } else {
        pattern == bundle_id
    }
}

impl Profiles {
    /// Resolves the rule for the frontmost app's bundle id, falling back to
    /// `default` when nothing matches (or the bundle id is unknown).
    pub fn resolve(&self, bundle_id: Option<&str>) -> ProfileRule {
        let Some(bundle_id) = bundle_id else {
            return self.default.clone();
        };
        for (pattern, rule) in &self.apps {
            if glob_match(pattern, bundle_id) {
                return rule.clone();
            }
        }
        self.default.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_seeded_terminal_apps_to_code_mode() {
        let profiles = seed_profiles();
        assert_eq!(profiles.resolve(Some("com.apple.Terminal")).mode, "code");
        assert_eq!(profiles.resolve(Some("com.googlecode.iterm2")).mode, "code");
        assert_eq!(profiles.resolve(Some("dev.warp.Warp")).mode, "code");
    }

    #[test]
    fn resolves_mail_and_slack() {
        let profiles = seed_profiles();
        let mail = profiles.resolve(Some("com.apple.mail"));
        assert_eq!(mail.mode, "clean");
        assert_eq!(mail.tone, "formal");
        let slack = profiles.resolve(Some("com.tinyspeck.slackmacgap"));
        assert_eq!(slack.tone, "casual");
    }

    #[test]
    fn unknown_app_falls_back_to_default() {
        let profiles = seed_profiles();
        let rule = profiles.resolve(Some("com.example.unknown"));
        assert_eq!(rule, ProfileRule::default());
        assert_eq!(profiles.resolve(None), ProfileRule::default());
    }

    #[test]
    fn trailing_star_glob_matches_prefix() {
        let mut profiles = seed_profiles();
        profiles.apps.insert(
            "com.example.*".to_string(),
            ProfileRule { mode: "polish".to_string(), tone: "neutral".to_string() },
        );
        assert_eq!(profiles.resolve(Some("com.example.anything")).mode, "polish");
    }

    #[test]
    fn matching_is_case_insensitive() {
        let profiles = seed_profiles();
        assert_eq!(profiles.resolve(Some("COM.APPLE.TERMINAL")).mode, "code");
    }
}
