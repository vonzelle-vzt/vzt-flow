//! Hold-to-talk key monitoring via a `CGEventTap`, not
//! `tauri-plugin-global-shortcut`.
//!
//! **Why not the plugin:** `tauri-plugin-global-shortcut` (and the
//! `global-hotkey` crate it wraps) registers shortcuts on macOS through
//! Carbon's `RegisterEventHotKey`, which fires on `kEventHotKeyPressed` /
//! `kEventHotKeyReleased` — real key-down/key-up events for a virtual
//! keycode plus a modifier mask. A bare modifier key held alone (Right
//! Option, our default binding) never generates a keyDown/keyUp of its
//! own; it only ever produces `flagsChanged` events, and
//! `global-hotkey`'s macOS `key_to_scancode` has no mapping for
//! `Code::AltRight` (or any modifier `Code`) — registering it returns
//! `FailedToRegister("Unknown scancode ...")`. Verified against
//! `tauri-apps/global-hotkey` v0.8.0 source
//! (`src/platform_impl/macos/mod.rs`) before writing this module. So for a
//! modifier-only hold key we listen to `CGEventType::FlagsChanged`
//! directly instead.
//!
//! The tap is `ListenOnly`, so it never consumes/blocks events — Escape
//! (and everything else) still reaches whatever app is frontmost. Rather
//! than installing/removing a second tap to "arm" Escape only while
//! recording (the OS-level equivalent of dynamic register/unregister),
//! this single tap always watches for both FlagsChanged and Escape
//! keyDown, and gates the Escape *action* on the `is_recording` flag the
//! coordinator maintains — functionally the same "only cancels while
//! recording" behavior, with one tap instead of two.

use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread;

use core_foundation::runloop::CFRunLoop;
use core_graphics::event::{
    CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventType, CallbackResult, EventField,
};

use crate::config::ESCAPE_KEYCODE;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEvent {
    /// The configured hold-to-talk key transitioned from up to down.
    HoldKeyPressed,
    /// The configured hold-to-talk key transitioned from down to up.
    HoldKeyReleased,
    /// Escape was pressed while `is_recording` was true.
    CancelRequested,
}

/// Shared flag the recording coordinator flips so the tap knows whether
/// Escape should currently act as "cancel recording".
pub fn new_recording_flag() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

/// Maps a modifier key's virtual keycode to the device-independent
/// `CGEventFlags` bit that reflects its current up/down state. Only
/// modifier keys are supported as hold-to-talk bindings (a non-modifier
/// key held down would auto-repeat keyDown events instead of producing a
/// clean single FlagsChanged transition, which is what makes "hold" vs
/// "tap" detection reliable here).
fn modifier_bit_for_keycode(keycode: u16) -> Option<CGEventFlags> {
    match keycode {
        56 | 60 => Some(CGEventFlags::CGEventFlagShift), // Left/Right Shift
        59 | 62 => Some(CGEventFlags::CGEventFlagControl), // Left/Right Control
        58 | 61 => Some(CGEventFlags::CGEventFlagAlternate), // Left/Right Option
        55 | 54 => Some(CGEventFlags::CGEventFlagCommand), // Left/Right Command
        57 => Some(CGEventFlags::CGEventFlagAlphaShift),   // Caps Lock
        63 => Some(CGEventFlags::CGEventFlagSecondaryFn),  // Fn
        _ => None,
    }
}

/// Spawns a dedicated OS thread that installs a `ListenOnly` CGEventTap and
/// runs a `CFRunLoop` forever, forwarding hold-key and cancel events on
/// `tx`. `hotkey_keycode` is checked live via the returned `AtomicU16`
/// handle so Settings can change the binding without restarting the tap.
///
/// Returns `Err` if the tap could not be created — almost always because
/// Accessibility/Input Monitoring permission hasn't been granted yet, since
/// `CGEventTapCreate` fails silently (`None`) without it.
pub fn spawn_monitor(
    initial_keycode: u16,
    is_recording: Arc<AtomicBool>,
    tx: Sender<HotkeyEvent>,
) -> Result<Arc<AtomicU16>, ()> {
    let keycode = Arc::new(AtomicU16::new(initial_keycode));
    let keycode_for_thread = keycode.clone();

    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), ()>>();

    thread::Builder::new()
        .name("vzt-flow-hotkey-tap".into())
        .spawn(move || {
            let hold_was_down = AtomicBool::new(false);
            let result = CGEventTap::with_enabled(
                CGEventTapLocation::HID,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::ListenOnly,
                vec![CGEventType::FlagsChanged, CGEventType::KeyDown],
                move |_proxy, event_type, event| {
                    let this_keycode = keycode_for_thread.load(Ordering::Relaxed);
                    let physical_key =
                        event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;

                    match event_type {
                        CGEventType::FlagsChanged if physical_key == this_keycode => {
                            let bit = modifier_bit_for_keycode(physical_key);
                            let down = bit
                                .map(|b| event.get_flags().contains(b))
                                .unwrap_or(false);
                            let was_down = hold_was_down.swap(down, Ordering::Relaxed);
                            if down && !was_down {
                                let _ = tx.send(HotkeyEvent::HoldKeyPressed);
                            } else if !down && was_down {
                                let _ = tx.send(HotkeyEvent::HoldKeyReleased);
                            }
                        }
                        CGEventType::KeyDown
                            if physical_key == ESCAPE_KEYCODE
                                && is_recording.load(Ordering::Relaxed) =>
                        {
                            let _ = tx.send(HotkeyEvent::CancelRequested);
                        }
                        _ => {}
                    }

                    CallbackResult::Keep
                },
                || {
                    let _ = ready_tx.send(Ok(()));
                    CFRunLoop::run_current();
                },
            );
            if result.is_err() {
                let _ = ready_tx.send(Err(()));
            }
        })
        .expect("failed to spawn hotkey monitor thread");

    ready_rx.recv().unwrap_or(Err(()))?;
    Ok(keycode)
}
