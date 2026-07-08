//! Clipboard-save / set / Cmd+V / clipboard-restore paste pipeline.

use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use arboard::{Clipboard, ImageData};
use enigo::{Direction, Enigo, Key, Keyboard, Settings};

use crate::permissions::{accessibility_trusted, secure_input_enabled};

/// Snapshot of the user's clipboard taken before we overwrite it with the
/// transcript, so we can put it back afterwards. arboard only exposes text
/// and image pasteboard types; anything else (file references, custom UTIs)
/// we cannot capture, so we record that we couldn't and decline to restore
/// rather than blow it away with an empty value.
enum SavedClipboard {
    Text(String),
    Image(ImageData<'static>),
    /// Present but not text/image (e.g. copied files) — uncapturable.
    Unsupported,
}

/// Whether it is safe to restore the saved clipboard after the paste delay.
/// Only restore when the transcript we set is still exactly what's on the
/// clipboard; if the user copied something new in the meantime, leave their
/// content alone (F6). Factored out so the decision is unit-testable.
fn should_restore(current_clipboard_text: Option<&str>, transcript_we_set: &str) -> bool {
    current_clipboard_text == Some(transcript_we_set)
}

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

    // Capture the previous clipboard as text, else image, else record that it
    // was something we can't preserve — never assume it was text (F5).
    let previous = if let Ok(t) = clipboard.get_text() {
        SavedClipboard::Text(t)
    } else if let Ok(img) = clipboard.get_image() {
        SavedClipboard::Image(img)
    } else {
        SavedClipboard::Unsupported
    };

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

    // Restore the previous clipboard off the calling thread, but only if our
    // transcript is still sitting on it — otherwise the user copied something
    // new during the paste delay and we must not clobber it (F6).
    let transcript = text.to_string();
    thread::spawn(move || {
        thread::sleep(CLIPBOARD_RESTORE_DELAY);
        let Ok(mut clipboard) = Clipboard::new() else {
            return;
        };
        let current = clipboard.get_text().ok();
        if !should_restore(current.as_deref(), &transcript) {
            return; // user's fresh copy (or an image) — leave it be
        }
        match previous {
            SavedClipboard::Text(prev) => {
                let _ = clipboard.set_text(prev);
            }
            SavedClipboard::Image(img) => {
                let _ = clipboard.set_image(img);
            }
            SavedClipboard::Unsupported => {
                // Couldn't capture the original (e.g. copied files); leaving
                // the transcript is the least-destructive option. Log it so
                // this is never silent.
                eprintln!(
                    "[vzt-flow] previous clipboard contents were not text or image and \
                     could not be restored; transcript left on clipboard"
                );
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

    #[test]
    fn restore_only_when_transcript_still_present() {
        // Our transcript is still on the clipboard: safe to restore.
        assert!(should_restore(Some("hello world"), "hello world"));
        // User copied something new: leave it alone.
        assert!(!should_restore(Some("user's new copy"), "hello world"));
        // Clipboard now holds a non-text type (get_text failed): don't restore.
        assert!(!should_restore(None, "hello world"));
        // Empty transcript edge: still only matches an exactly-empty clipboard.
        assert!(should_restore(Some(""), ""));
        assert!(!should_restore(None, ""));
    }

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
