//! Clipboard-save / set / Cmd+V / clipboard-restore paste pipeline.

use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use arboard::Clipboard;
use enigo::{Direction, Enigo, Key, Keyboard, Settings};

use crate::permissions::{accessibility_trusted, secure_input_enabled};

/// How long to wait after pasting before restoring the user's previous
/// clipboard contents. Long enough for the target app's paste handler to
/// have read the pasteboard.
const CLIPBOARD_RESTORE_DELAY: Duration = Duration::from_millis(1000);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasteOutcome {
    /// Transcript was pasted into the frontmost app via simulated Cmd+V.
    Pasted,
    /// A secure input field is focused (e.g. a password box); we left the
    /// transcript on the clipboard instead of risking a blocked/garbled
    /// synthetic paste.
    SkippedSecureField,
    /// Accessibility permission hasn't been granted, so enigo cannot
    /// synthesize the Cmd+V keystroke; transcript is left on the clipboard.
    SkippedNoAccessibility,
}

/// Save the current clipboard, set it to `text`, optionally simulate
/// Cmd+V, then restore the previous clipboard contents after a short delay.
///
/// The restore happens on a spawned thread so callers (the recording
/// coordinator) aren't blocked for a full second per dictation.
pub fn paste_text(text: &str) -> Result<PasteOutcome> {
    let mut clipboard = Clipboard::new().context("failed to access system clipboard")?;
    let previous = clipboard.get_text().ok();

    clipboard
        .set_text(text.to_string())
        .context("failed to write transcript to clipboard")?;
    drop(clipboard);

    let outcome = if secure_input_enabled() {
        PasteOutcome::SkippedSecureField
    } else if !accessibility_trusted() {
        PasteOutcome::SkippedNoAccessibility
    } else {
        simulate_cmd_v()?;
        PasteOutcome::Pasted
    };

    // Restore whatever was on the clipboard before, off the calling thread.
    thread::spawn(move || {
        thread::sleep(CLIPBOARD_RESTORE_DELAY);
        if let Some(prev) = previous {
            if let Ok(mut clipboard) = Clipboard::new() {
                let _ = clipboard.set_text(prev);
            }
        }
    });

    Ok(outcome)
}

fn simulate_cmd_v() -> Result<()> {
    let mut enigo = Enigo::new(&Settings::default()).context("failed to init enigo")?;
    enigo
        .key(Key::Meta, Direction::Press)
        .context("failed to press Cmd")?;
    enigo
        .key(Key::Unicode('v'), Direction::Click)
        .context("failed to click V")?;
    enigo
        .key(Key::Meta, Direction::Release)
        .context("failed to release Cmd")?;
    Ok(())
}

/// Exercises the clipboard save/set/[maybe paste]/restore path end-to-end
/// and reports what happened, for the hidden `flow paste-test` CLI command.
/// Honest about permission state instead of pretending a real paste
/// happened when it didn't.
pub fn run_paste_test(text: &str) -> Result<()> {
    println!("secure input enabled : {}", secure_input_enabled());
    println!("accessibility trusted: {}", accessibility_trusted());

    let mut clipboard = Clipboard::new().context("failed to access system clipboard")?;
    let previous = clipboard.get_text().ok();
    println!(
        "clipboard before     : {:?}",
        previous.as_deref().unwrap_or("<empty>")
    );
    drop(clipboard);

    let outcome = paste_text(text)?;
    println!("outcome              : {outcome:?}");

    let mut clipboard = Clipboard::new().context("failed to access system clipboard")?;
    let now = clipboard.get_text().unwrap_or_default();
    println!("clipboard after call : {now:?}");
    match outcome {
        PasteOutcome::Pasted => {
            println!("Cmd+V was simulated. Clipboard will restore to the previous value in ~1s.")
        }
        PasteOutcome::SkippedSecureField => {
            println!("Skipped paste: a secure input field appears focused. Transcript is on the clipboard — paste manually.")
        }
        PasteOutcome::SkippedNoAccessibility => {
            println!("Skipped paste: Accessibility permission not granted. Transcript is on the clipboard — paste manually, or grant Accessibility and retry.")
        }
    }

    // paste_text's restore happens on a spawned thread after
    // CLIPBOARD_RESTORE_DELAY; a short-lived CLI process would otherwise
    // exit (and kill that thread) before it ever runs. Wait it out here so
    // this diagnostic actually demonstrates the full round-trip instead of
    // just claiming it will happen.
    thread::sleep(Duration::from_millis(1200));
    let mut clipboard = Clipboard::new().context("failed to access system clipboard")?;
    let restored = clipboard.get_text().unwrap_or_default();
    let expected = previous.as_deref().unwrap_or("");
    if restored == expected {
        println!("clipboard restored   : yes ({restored:?})");
    } else {
        println!(
            "clipboard restored   : NO — expected {expected:?}, found {restored:?}"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exercises the clipboard save/set/restore mechanics without invoking
    /// the real Cmd+V simulation, so `cargo test` never fires a synthetic
    /// keystroke into whatever happens to have focus.
    #[test]
    fn clipboard_set_and_restore() {
        let mut clipboard = match Clipboard::new() {
            Ok(c) => c,
            Err(_) => return, // no clipboard access in this environment (e.g. headless CI)
        };
        let sentinel = "vzt-flow-paste-test-sentinel";
        clipboard.set_text(sentinel.to_string()).unwrap();
        drop(clipboard);

        // Mirror paste_text's save/set logic directly rather than calling
        // paste_text (which may simulate a real keystroke if this machine
        // happens to have Accessibility granted for the test binary).
        let mut clipboard = Clipboard::new().unwrap();
        let previous = clipboard.get_text().ok();
        assert_eq!(previous.as_deref(), Some(sentinel));
        clipboard.set_text("hello world".to_string()).unwrap();
        drop(clipboard);

        let mut clipboard = Clipboard::new().unwrap();
        assert_eq!(clipboard.get_text().unwrap(), "hello world");
        clipboard.set_text(previous.unwrap()).unwrap();
    }
}
