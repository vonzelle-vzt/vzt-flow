//! Where the desktop app's diagnostics go.
//!
//! Every diagnostic in this codebase is an `eprintln!`, and a GUI app launched
//! from Finder, the Dock or at login has its stderr pointed at `/dev/null`. So
//! a normally-launched VZT Flow logged nothing, anywhere: when long dictations
//! started vanishing there was no record of a timeout, a failed chunk or even
//! of the dictation having happened (the history file is written on success
//! only). The desktop app now points stderr at a file at startup
//! ([`log_file_path`]), which captures every existing `eprintln!`, the panic
//! hook and llama.cpp's own output with no call-site changes.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

/// Set to `1` to keep stderr where the launcher put it (a terminal, or a
/// file you redirected to) instead of the log file.
pub const LOG_STDERR_ENV: &str = "VZT_FLOW_LOG_STDERR";

/// The log is rotated to `<name>.1` at launch once it exceeds this size.
pub const ROTATE_BYTES: u64 = 5 * 1024 * 1024;

/// `~/Library/Logs/VZT Flow/vzt-flow.log` on macOS (where Console.app looks),
/// `$XDG_STATE_HOME/vzt-flow/vzt-flow.log` (default `~/.local/state`) on
/// other Unixes. `None` on Windows, where the app is a GUI-subsystem binary
/// with no stderr to redirect.
pub fn log_file_path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        dirs::home_dir().map(|h| h.join("Library").join("Logs").join("VZT Flow").join("vzt-flow.log"))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let state = std::env::var_os("XDG_STATE_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|h| h.join(".local").join("state")))?;
        Some(state.join("vzt-flow").join("vzt-flow.log"))
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Whether to redirect stderr, given the value of [`LOG_STDERR_ENV`].
pub fn should_redirect(env_value: Option<&std::ffi::OsStr>) -> bool {
    env_value.map(|v| v != "1").unwrap_or(true)
}

/// Whether a log of `len` bytes should be rotated before appending.
pub fn needs_rotation(len: u64) -> bool {
    len > ROTATE_BYTES
}

/// Creates the log directory, rotates an oversized log to `<path>.1`
/// (replacing any previous `.1`), and opens `path` for appending.
pub fn open_for_append(path: &Path) -> io::Result<File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if let Ok(meta) = std::fs::metadata(path) {
        if needs_rotation(meta.len()) {
            let mut rotated = path.as_os_str().to_owned();
            rotated.push(".1");
            std::fs::rename(path, PathBuf::from(rotated))?;
        }
    }
    OpenOptions::new().create(true).append(true).open(path)
}

/// Local wall-clock timestamp for log lines (`2026-09-29 10:31:07`).
pub fn timestamp() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn redirect_is_the_default_and_the_env_var_opts_out() {
        assert!(should_redirect(None));
        assert!(!should_redirect(Some(std::ffi::OsStr::new("1"))));
        assert!(should_redirect(Some(std::ffi::OsStr::new("0"))));
    }

    #[test]
    fn rotation_threshold_is_five_megabytes() {
        assert!(!needs_rotation(ROTATE_BYTES));
        assert!(needs_rotation(ROTATE_BYTES + 1));
    }

    #[test]
    fn an_oversized_log_is_rotated_and_a_fresh_one_opened() {
        let dir = std::env::temp_dir().join(format!("vzt-logfile-test-{}", std::process::id()));
        let path = dir.join("sub").join("vzt-flow.log");
        {
            let mut f = open_for_append(&path).unwrap();
            f.write_all(&vec![b'x'; ROTATE_BYTES as usize + 10]).unwrap();
        }
        let mut f = open_for_append(&path).unwrap();
        writeln!(f, "fresh").unwrap();
        drop(f);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fresh\n");
        let rotated = dir.join("sub").join("vzt-flow.log.1");
        assert_eq!(std::fs::metadata(rotated).unwrap().len(), ROTATE_BYTES + 10);
        // A small log is appended to, not rotated.
        let mut f = open_for_append(&path).unwrap();
        writeln!(f, "again").unwrap();
        drop(f);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fresh\nagain\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_log_lives_where_console_app_looks() {
        let p = log_file_path().unwrap();
        assert!(p.ends_with("Library/Logs/VZT Flow/vzt-flow.log"), "{}", p.display());
    }
}
