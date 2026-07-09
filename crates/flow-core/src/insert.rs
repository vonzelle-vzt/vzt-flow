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
    /// Transcript was pasted into the frontmost app via simulated Cmd+V —
    /// either verified present in the focused field, or its value was
    /// unreadable via Accessibility (most web/Electron/secure fields) so we
    /// optimistically assume success. See [`verify_paste`].
    Pasted,
    /// A secure input field is focused (e.g. a password box); we left the
    /// transcript on the clipboard instead of risking a blocked/garbled
    /// synthetic paste.
    SkippedSecureField,
    /// Accessibility permission hasn't been granted, so enigo cannot
    /// synthesize the Cmd+V keystroke; transcript is left on the clipboard.
    SkippedNoAccessibility,
    /// Linux/Wayland: there is no reachable X server for enigo's XTEST paste
    /// (no `DISPLAY`, only `WAYLAND_DISPLAY`), so a synthetic Ctrl+V can't
    /// reach the focused Wayland client. The transcript is left on the
    /// clipboard for the user to paste manually with Ctrl+V.
    ClipboardOnly,
    /// macOS: the Cmd+V was synthesized but post-paste Accessibility
    /// verification could read the focused field and the transcript tail was
    /// *not* present, even after one retry (Feature C). The transcript is
    /// deliberately left on the clipboard (not restored) so the user can paste
    /// it manually.
    VerificationFailed,
}

/// How many trailing characters of the transcript to look for in the focused
/// field when verifying a paste (Feature C). The tail — rather than the whole
/// transcript — keeps the check robust to a field that already held text
/// before the paste, and short enough to compare cheaply.
const TAIL_MATCH_CHARS: usize = 20;

/// How long to wait after a synthesized Cmd+V before reading the focused field
/// to verify the paste landed. Worst case this is paid twice (initial check +
/// one retry) = 300ms, kept well under the 400ms blocking budget the
/// coordinator tolerates on the paste path.
#[cfg(target_os = "macos")]
const PASTE_VERIFY_DELAY: Duration = Duration::from_millis(150);

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
    } else if !can_synthesize_paste() {
        PasteOutcome::ClipboardOnly
    } else {
        simulate_paste()?;
        // Feature C: confirm the paste actually landed (macOS AX read). On a
        // possible failure this returns VerificationFailed after one retry;
        // on every other platform this is a no-op that returns Pasted.
        verify_paste(text)
    };

    // Restore the previous clipboard off the calling thread, but only if our
    // transcript is still sitting on it — otherwise the user copied something
    // new during the paste delay and we must not clobber it (F6). Skipped
    // entirely for VerificationFailed: there we intentionally leave the
    // transcript on the clipboard so the user can paste it by hand (Feature C).
    if outcome != PasteOutcome::VerificationFailed {
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
                    // Couldn't capture the original (e.g. copied files);
                    // leaving the transcript is the least-destructive option.
                    // Log it so this is never silent.
                    eprintln!(
                        "[vzt-flow] previous clipboard contents were not text or image and \
                         could not be restored; transcript left on clipboard"
                    );
                }
            }
        });
    }

    Ok(outcome)
}

/// The platform's "paste" modifier: Cmd on macOS, Ctrl everywhere else
/// (Windows/Linux). enigo itself is cross-platform; only the key differs.
#[cfg(target_os = "macos")]
fn paste_modifier() -> Key {
    Key::Meta
}
#[cfg(not(target_os = "macos"))]
fn paste_modifier() -> Key {
    Key::Control
}

/// Whether the platform can reliably synthesize the paste keystroke into the
/// focused window. macOS and Windows always can here — their permission /
/// secure-input caveats are handled by the checks in `paste_text` above.
///
/// On Linux, enigo's default backend drives X11 via the XTEST extension,
/// which needs a reachable X server (`DISPLAY`). Under a pure-Wayland session
/// with no XWayland (`DISPLAY` unset, only `WAYLAND_DISPLAY` present),
/// `Enigo::new` can't connect and a synthetic Ctrl+V would reach nothing, so
/// we report `false` and leave the transcript on the clipboard for a manual
/// Ctrl+V instead. When `DISPLAY` *is* set (real X11, or XWayland) we attempt
/// the paste; note that XTEST-into-Wayland injection is honored by some
/// compositors (GNOME/Mutter, KDE) but not all (e.g. some wlroots-based ones)
/// — the transcript is on the clipboard first either way, so the worst case
/// is a manual paste. See docs/USAGE-Linux.md for the full support matrix.
#[cfg(not(target_os = "linux"))]
fn can_synthesize_paste() -> bool {
    true
}
#[cfg(target_os = "linux")]
fn can_synthesize_paste() -> bool {
    std::env::var_os("DISPLAY").is_some()
}

/// Simulates the OS paste shortcut (Cmd+V / Ctrl+V) via enigo.
///
/// Windows note: unlike macOS's secure-input check above, there is no API
/// queried here for UIPI (User Interface Privilege Isolation) — a
/// lower-privilege process's synthetic input is silently dropped by a
/// higher-privilege target window (e.g. an elevated app), with no error
/// enigo can observe. We don't attempt to detect that case; the transcript
/// is always left on the clipboard first (see `paste_text` above), so the
/// worst case is the user pastes manually instead of it landing
/// automatically. Revisit if this turns out to bite real users.
fn simulate_paste() -> Result<()> {
    let mut enigo = Enigo::new(&Settings::default()).context("failed to init enigo")?;
    let modifier = paste_modifier();
    enigo
        .key(modifier, Direction::Press)
        .context("failed to press paste modifier")?;
    enigo
        .key(Key::Unicode('v'), Direction::Click)
        .context("failed to click V")?;
    enigo
        .key(modifier, Direction::Release)
        .context("failed to release paste modifier")?;
    Ok(())
}

/// Whitespace-normalized tail of `text`: the last `n` characters after
/// collapsing every run of whitespace to a single space and trimming the ends.
/// The normalization makes the later `contains` check tolerant of a target
/// field that soft-wraps or reflows newlines differently from the source.
fn normalized_tail(text: &str, n: usize) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let chars: Vec<char> = normalized.chars().collect();
    let start = chars.len().saturating_sub(n);
    chars[start..].iter().collect()
}

/// Whether the focused field's text `field_value` contains the transcript's
/// (whitespace-normalized) [`TAIL_MATCH_CHARS`]-char tail — the evidence that
/// the paste landed. Both sides are whitespace-normalized so wrapping/newline
/// differences don't cause a false negative. An empty transcript has no tail
/// to find and is treated as trivially present. Pure and unit-tested.
fn tail_present(field_value: &str, transcript: &str) -> bool {
    let tail = normalized_tail(transcript, TAIL_MATCH_CHARS);
    if tail.is_empty() {
        return true;
    }
    let field_norm = field_value.split_whitespace().collect::<Vec<_>>().join(" ");
    field_norm.contains(&tail)
}

/// Reads the text value of the system-wide focused UI element via the
/// Accessibility API (`kAXFocusedUIElement` → `kAXValue`). Returns `Some(text)`
/// when the focused field exposes a readable string value, or `None` when it
/// is unreadable — no focused element, a non-string value, an AX error, or a
/// field that simply doesn't publish its content (most web/Electron views and
/// secure fields). Callers treat `None` as "can't tell, assume success".
#[cfg(target_os = "macos")]
fn read_focused_text() -> Option<String> {
    use core_foundation::base::{CFType, CFTypeRef, TCFType};
    use core_foundation::string::CFString;
    use std::os::raw::c_void;

    // AXUIElementRef / CFTypeRef are opaque pointers. `AXUIElementCreateSystemWide`
    // and the `Copy` accessor both follow the CoreFoundation Create/Copy rule
    // (return +1), so each must be released exactly once. ApplicationServices is
    // already linked (see `permissions.rs`).
    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXUIElementCreateSystemWide() -> *mut c_void;
        fn AXUIElementCopyAttributeValue(
            element: *mut c_void,
            attribute: *const c_void, // CFStringRef
            value: *mut *const c_void, // CFTypeRef*
        ) -> i32; // AXError; 0 == kAXErrorSuccess
        fn CFRelease(cf: *const c_void);
    }

    unsafe {
        let system_wide = AXUIElementCreateSystemWide();
        if system_wide.is_null() {
            return None;
        }

        let focused_attr = CFString::new("AXFocusedUIElement");
        let mut focused: *const c_void = std::ptr::null();
        let err = AXUIElementCopyAttributeValue(
            system_wide,
            focused_attr.as_concrete_TypeRef() as *const c_void,
            &mut focused,
        );
        CFRelease(system_wide as *const c_void);
        if err != 0 || focused.is_null() {
            return None;
        }

        let value_attr = CFString::new("AXValue");
        let mut value: *const c_void = std::ptr::null();
        let err = AXUIElementCopyAttributeValue(
            focused as *mut c_void,
            value_attr.as_concrete_TypeRef() as *const c_void,
            &mut value,
        );
        CFRelease(focused);
        if err != 0 || value.is_null() {
            return None;
        }

        // Take ownership of the +1 value and interpret it as a string; a
        // non-string AX value (number, bool, element) downcasts to None and is
        // released when the wrapping CFType drops.
        let cf = CFType::wrap_under_create_rule(value as CFTypeRef);
        cf.downcast_into::<CFString>().map(|s| s.to_string())
    }
}

/// Post-paste verification (Feature C). After the synthesized Cmd+V, wait
/// briefly then read the focused field via Accessibility:
///   - unreadable value → assume success (today's behavior for web/Electron/
///     secure fields);
///   - tail present → success;
///   - tail absent → retry the paste once and re-check; still absent →
///     [`PasteOutcome::VerificationFailed`].
///
/// Bounded to two [`PASTE_VERIFY_DELAY`] waits (≤300ms) so the caller never
/// blocks past its ~400ms budget.
#[cfg(target_os = "macos")]
fn verify_paste(text: &str) -> PasteOutcome {
    thread::sleep(PASTE_VERIFY_DELAY);
    match read_focused_text() {
        None => PasteOutcome::Pasted, // unreadable: can't tell, assume it worked
        Some(v) if tail_present(&v, text) => PasteOutcome::Pasted,
        Some(_) => {
            // Readable and the tail isn't there — the paste likely didn't land
            // (focus moved, app swallowed the keystroke). Retry once.
            if simulate_paste().is_err() {
                return PasteOutcome::VerificationFailed;
            }
            thread::sleep(PASTE_VERIFY_DELAY);
            match read_focused_text() {
                Some(v) if tail_present(&v, text) => PasteOutcome::Pasted,
                _ => PasteOutcome::VerificationFailed,
            }
        }
    }
}

/// Non-macOS: no Accessibility verification available; the synthesized paste
/// is assumed to have landed exactly as before this feature existed.
#[cfg(not(target_os = "macos"))]
fn verify_paste(_text: &str) -> PasteOutcome {
    PasteOutcome::Pasted
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
        PasteOutcome::ClipboardOnly => {
            println!("Skipped paste: no X server reachable (Wayland session without XWayland). Transcript is on the clipboard — paste manually with Ctrl+V.")
        }
        PasteOutcome::VerificationFailed => {
            println!("Paste may have failed: the focused field was readable via Accessibility but the transcript tail wasn't there after a retry. Transcript is on the clipboard — paste manually.")
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

    // ---- Feature C: paste-verification tail matching ----

    #[test]
    fn tail_present_finds_the_transcript_tail_in_a_longer_field() {
        // Field already had prior text; the pasted transcript's tail appears
        // at the end. Verification should pass.
        let transcript = "the quick brown fox jumps over the lazy dog";
        let field = "some earlier note the quick brown fox jumps over the lazy dog";
        assert!(tail_present(field, transcript));
    }

    #[test]
    fn tail_absent_when_field_lacks_the_transcript() {
        let transcript = "the quick brown fox jumps over the lazy dog";
        let field = "completely unrelated field contents that never got the paste";
        assert!(!tail_present(field, transcript));
    }

    #[test]
    fn tail_match_is_whitespace_normalized() {
        // Field reflowed the transcript across a newline and doubled a space;
        // normalization must still find the tail.
        let transcript = "please schedule the meeting for tomorrow";
        let field = "please schedule the\nmeeting  for tomorrow";
        assert!(tail_present(field, transcript));
    }

    #[test]
    fn tail_shorter_than_match_window_matches_whole_transcript() {
        // Transcript shorter than TAIL_MATCH_CHARS: the tail is the whole
        // (normalized) string.
        let transcript = "hi there";
        assert!(tail_present("well hi there", transcript));
        assert!(!tail_present("goodbye", transcript));
    }

    #[test]
    fn empty_transcript_is_trivially_present() {
        // Nothing to verify — never report a spurious failure.
        assert!(tail_present("anything at all", ""));
        assert!(tail_present("", ""));
    }

    #[test]
    fn normalized_tail_takes_last_n_chars_after_collapsing_whitespace() {
        assert_eq!(normalized_tail("a  b\tc\nd", 3), "c d");
        assert_eq!(normalized_tail("abcdef", 3), "def");
        assert_eq!(normalized_tail("ab", 5), "ab");
        assert_eq!(normalized_tail("   ", 5), "");
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
