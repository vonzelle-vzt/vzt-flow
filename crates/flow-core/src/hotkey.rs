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

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

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

/// macOS hold-to-talk monitoring via a `CGEventTap`. Gated out on every
/// other platform — see the module docs above for why this can't be
/// `tauri-plugin-global-shortcut` for a modifier-only binding, and see
/// `apps/desktop/src-tauri/src/coordinator.rs` for the Windows equivalent
/// (which *does* use that plugin, since Windows has no modifier-only
/// binding to support in the first place — its default binding is a normal
/// key combo, and registering that only needs an `AppHandle`, which this
/// platform-agnostic crate deliberately doesn't depend on).
#[cfg(target_os = "macos")]
mod macos {
    use super::HotkeyEvent;
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU16, Ordering};
    use std::sync::mpsc::Sender;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    use core_foundation::base::TCFType;
    use core_foundation::mach_port::CFMachPortRef;
    use core_foundation::runloop::{kCFRunLoopCommonModes, kCFRunLoopDefaultMode, CFRunLoop};
    use core_graphics::event::{
        CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
        CGEventType, CallbackResult, EventField,
    };

    use crate::config::ESCAPE_KEYCODE;

    // `core-graphics` 0.25 exposes `CGEventTap::enable()` but keeps the raw
    // `CGEventTapEnable` FFI private, and there is no way to reach the owning
    // `CGEventTap` from inside its own callback. We re-declare the symbol so the
    // callback can re-arm the tap the instant macOS disables it (see F1). The
    // symbol lives in the CoreGraphics framework, already linked transitively via
    // `core-graphics`, so no extra `#[link]` is required.
    extern "C" {
        fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
    }

    /// How often the watchdog wakes to unconditionally re-arm the tap, as a
    /// belt-and-braces backstop to the in-callback re-enable.
    const WATCHDOG_INTERVAL: Duration = Duration::from_secs(5);

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

/// Pure edge detector: given the previous latched state and the freshly
/// derived current state of the hold key, decide which (if any) transition
/// event to emit. Factored out of the tap callback so the tap-vs-hold edge
/// logic can be unit-tested without a live CGEventTap.
///
/// `down` is always derived from the event's flags mask (not by toggling a
/// latch), so a stale latch only ever affects *edge* detection — and the
/// callback resets that latch on tap re-arm and on binding changes so it can
/// never invert (F7).
fn hold_edge(was_down: bool, down: bool) -> Option<HotkeyEvent> {
    if down && !was_down {
        Some(HotkeyEvent::HoldKeyPressed)
    } else if !down && was_down {
        Some(HotkeyEvent::HoldKeyReleased)
    } else {
        None
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
            // Edge-detection latch for the hold key, plus the keycode it
            // currently pertains to. Both live entirely inside the callback;
            // `latch_keycode` lets us notice a live binding change and drop a
            // now-meaningless latch (F7).
            let hold_was_down = AtomicBool::new(false);
            let latch_keycode = AtomicU16::new(initial_keycode);

            // Raw `CFMachPortRef` of the tap, shared into the callback so it
            // can re-arm the tap the moment macOS delivers a
            // `TapDisabled*` event (F1). Null until the tap is created below.
            let tap_port: Arc<AtomicPtr<c_void>> = Arc::new(AtomicPtr::new(std::ptr::null_mut()));
            let tap_port_for_cb = tap_port.clone();

            let tap = CGEventTap::new(
                CGEventTapLocation::HID,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::ListenOnly,
                vec![CGEventType::FlagsChanged, CGEventType::KeyDown],
                move |_proxy, event_type, event| {
                    match event_type {
                        // macOS disabled the tap (it timed out under load, or
                        // the user's input momentarily suspended it). Re-arm
                        // immediately, and reset the edge latch: while the tap
                        // was dead we may have missed a key-up, so a stale
                        // "down" latch would otherwise swallow the next press.
                        // This is also the path a wake-from-sleep takes.
                        CGEventType::TapDisabledByTimeout
                        | CGEventType::TapDisabledByUserInput => {
                            hold_was_down.store(false, Ordering::Relaxed);
                            let port = tap_port_for_cb.load(Ordering::Acquire);
                            if !port.is_null() {
                                unsafe { CGEventTapEnable(port as CFMachPortRef, true) };
                            }
                        }
                        CGEventType::FlagsChanged => {
                            let this_keycode = keycode_for_thread.load(Ordering::Relaxed);
                            let physical_key = event
                                .get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE)
                                as u16;
                            if physical_key != this_keycode {
                                return CallbackResult::Keep;
                            }
                            // Binding changed since the latch was last set:
                            // the previous key's up/down state says nothing
                            // about this one, so drop it (F7).
                            if latch_keycode.swap(this_keycode, Ordering::Relaxed) != this_keycode {
                                hold_was_down.store(false, Ordering::Relaxed);
                            }
                            let down = modifier_bit_for_keycode(physical_key)
                                .map(|b| event.get_flags().contains(b))
                                .unwrap_or(false);
                            let was_down = hold_was_down.swap(down, Ordering::Relaxed);
                            if let Some(ev) = hold_edge(was_down, down) {
                                let _ = tx.send(ev);
                            }
                        }
                        CGEventType::KeyDown => {
                            let physical_key = event
                                .get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE)
                                as u16;
                            if physical_key == ESCAPE_KEYCODE
                                && is_recording.load(Ordering::Relaxed)
                            {
                                let _ = tx.send(HotkeyEvent::CancelRequested);
                            }
                        }
                        _ => {}
                    }

                    CallbackResult::Keep
                },
            );

            let tap = match tap {
                Ok(t) => t,
                Err(()) => {
                    let _ = ready_tx.send(Err(()));
                    return;
                }
            };

            // Publish the port so the callback can re-arm, then wire the tap
            // into this thread's run loop and enable it.
            tap_port.store(
                tap.mach_port().as_concrete_TypeRef() as *mut c_void,
                Ordering::Release,
            );
            let loop_source = match tap.mach_port().create_runloop_source(0) {
                Ok(s) => s,
                Err(()) => {
                    let _ = ready_tx.send(Err(()));
                    return;
                }
            };
            CFRunLoop::get_current().add_source(&loop_source, unsafe { kCFRunLoopCommonModes });
            tap.enable();
            let _ = ready_tx.send(Ok(()));

            // Watchdog loop: run the run loop in ~5s slices (processing tap
            // events the whole time) and unconditionally re-arm on each wake.
            // `enable()` on an already-enabled tap is a harmless no-op, so this
            // is a cheap safety net beneath the in-callback re-enable. The tap
            // is held for the whole loop, so it is never dropped (which would
            // invalidate the mach port).
            loop {
                CFRunLoop::run_in_mode(
                    unsafe { kCFRunLoopDefaultMode },
                    WATCHDOG_INTERVAL,
                    false,
                );
                tap.enable();
            }
        })
        .expect("failed to spawn hotkey monitor thread");

    ready_rx.recv().unwrap_or(Err(()))?;
    Ok(keycode)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_rising_emits_pressed() {
        assert_eq!(hold_edge(false, true), Some(HotkeyEvent::HoldKeyPressed));
    }

    #[test]
    fn edge_falling_emits_released() {
        assert_eq!(hold_edge(true, false), Some(HotkeyEvent::HoldKeyReleased));
    }

    #[test]
    fn edge_no_change_emits_nothing() {
        assert_eq!(hold_edge(false, false), None);
        assert_eq!(hold_edge(true, true), None);
    }

    #[test]
    fn stale_down_latch_would_swallow_press_until_reset() {
        // Reproduces the F7 failure mode: if a key-up is missed, the latch is
        // left "down". A fresh press then reads down==true, was_down==true and
        // is swallowed...
        assert_eq!(hold_edge(true, true), None);
        // ...which is exactly why the callback resets the latch to false on
        // tap re-arm / binding change. After the reset the same press is seen.
        let after_reset = false;
        assert_eq!(hold_edge(after_reset, true), Some(HotkeyEvent::HoldKeyPressed));
    }
    }
} // mod macos

#[cfg(target_os = "macos")]
pub use macos::spawn_monitor;

/// Non-macOS stub. Always fails to install — there is no in-process global
/// hotkey monitor in `flow-core` on this platform. The desktop app installs
/// its own platform-appropriate monitor instead (see
/// `apps/desktop/src-tauri/src/coordinator.rs`, which uses
/// `tauri-plugin-global-shortcut` on Windows); this crate has no `AppHandle`
/// to register shortcuts through, so it can't do that itself.
#[cfg(not(target_os = "macos"))]
pub fn spawn_monitor(
    _initial_keycode: u16,
    _is_recording: Arc<AtomicBool>,
    _tx: std::sync::mpsc::Sender<HotkeyEvent>,
) -> Result<Arc<std::sync::atomic::AtomicU16>, ()> {
    Err(())
}
