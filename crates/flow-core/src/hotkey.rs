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
    /// Some *other* key produced a keyDown while the hold-to-talk key was
    /// physically held down. On macOS the default binding (Right Option) is
    /// the special-character modifier — Option+e = ´, Option+u = ¨, etc. —
    /// so a keyDown arriving mid-hold means the user is typing a special
    /// character, not push-to-talking. The coordinator treats this as an
    /// accidental-press guard: it cancels any recording the hold had already
    /// started (a false start) and suppresses the tap-to-toggle that a bare
    /// short hold would otherwise arm. The tap is `ListenOnly`, so the typed
    /// character itself is never swallowed — it reaches the frontmost app
    /// unmodified.
    OtherKeyDuringHold,
}

/// Shared flag the recording coordinator flips so the tap knows whether
/// Escape should currently act as "cancel recording".
pub fn new_recording_flag() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

/// All keycodes the macOS tap can read a live up/down state for via
/// `FlagsChanged` (i.e. every arm `modifier_bit_for_keycode` maps to
/// `Some`). This is the single source of truth for "is this keycode a
/// modifier key at all" — kept platform-agnostic (unlike
/// `modifier_bit_for_keycode`, which lives inside the `#[cfg(target_os =
/// "macos")]` module) so `flow-cli`'s `doctor` can validate a configured
/// keycode on any build target, not just macOS.
///
/// Caps Lock (57) is included here but is deliberately **excluded** from
/// [`hold_capable_hotkey_keycodes`] — see that function's docs. Don't use
/// this list to populate a hold-to-talk picker; use the hold-capable one.
pub const SUPPORTED_HOTKEY_KEYCODES: &[u16] =
    &[54, 55, 56, 57, 58, 59, 60, 61, 62, 63];

/// The subset of [`SUPPORTED_HOTKEY_KEYCODES`] that actually behaves as a
/// *hold*-to-talk key. Caps Lock (57) is excluded: `CGEventFlagAlphaShift`
/// reflects the caps-lock **latched state** (the LED on/off), not whether
/// the physical key is currently held down. Bound as a hold key, pressing
/// Caps Lock would start recording and leave Caps Lock ON; pressing it
/// again would stop recording — a toggle that also hijacks the user's caps
/// lock state, not push-to-talk. `modifier_bit_for_keycode(57)` still
/// returns `Some(_)` (the tap *can* read a flag for it), so it stays in
/// `SUPPORTED_HOTKEY_KEYCODES`; it just isn't a valid *hold* binding.
pub const HOLD_CAPABLE_HOTKEY_KEYCODES: &[u16] =
    &[54, 55, 56, 58, 59, 60, 61, 62, 63];

/// Returns [`SUPPORTED_HOTKEY_KEYCODES`].
pub fn supported_hotkey_keycodes() -> &'static [u16] {
    SUPPORTED_HOTKEY_KEYCODES
}

/// Returns [`HOLD_CAPABLE_HOTKEY_KEYCODES`] — the keycodes safe to offer as
/// a hold-to-talk binding (excludes Caps Lock; see its docs).
pub fn hold_capable_hotkey_keycodes() -> &'static [u16] {
    HOLD_CAPABLE_HOTKEY_KEYCODES
}

/// Whether `keycode` is one the tap can read a live flag for at all
/// (includes Caps Lock, which is supported-but-not-hold-capable — see
/// [`hotkey_keycode_is_hold_capable`] for the stricter check).
pub fn hotkey_keycode_is_supported(keycode: u16) -> bool {
    SUPPORTED_HOTKEY_KEYCODES.contains(&keycode)
}

/// Whether `keycode` is valid to bind as a *hold*-to-talk key (excludes
/// Caps Lock's toggle semantics; see [`HOLD_CAPABLE_HOTKEY_KEYCODES`]).
pub fn hotkey_keycode_is_hold_capable(keycode: u16) -> bool {
    HOLD_CAPABLE_HOTKEY_KEYCODES.contains(&keycode)
}

/// Whether the late-grant re-arm driver should attempt to install a
/// `CGEventTap` on this tick. True **only** when the tap is not already armed
/// *and* Input Monitoring reads `Granted`.
///
/// This single predicate is what makes re-arming safe. [`spawn_monitor`] is
/// **not idempotent**: a *successful* call parks a thread on an infinite
/// `CFRunLoop` holding the tap, with no shutdown handle, so calling it twice
/// after a success leaks a thread + tap + mach port forever. Gating on
/// `!active` structurally guarantees we never re-call after a success — the
/// sole unsafe direction. Gating on `Granted` (never `Unknown`, never
/// `Denied`) means a permanently-denied machine never calls `CGEventTapCreate`
/// at all; only the cheap non-prompting `IOHIDCheckAccess` poll runs.
pub fn should_attempt_arm(active: bool, access: crate::permissions::InputMonitoringAccess) -> bool {
    !active && matches!(access, crate::permissions::InputMonitoringAccess::Granted)
}

/// One iteration of the late-grant re-arm driver's control loop, factored pure
/// so the "never spawn after a success, never spawn while denied" invariants
/// are unit-testable without a real `CGEventTap`. Side effects are injected:
/// `check_access` reads the current Input Monitoring grant, `try_arm` attempts
/// the (non-idempotent) tap install and returns whether it came up. Returns the
/// tap's armed state after this tick.
///
///  - already armed         → returns `true`, `try_arm` is never called;
///  - unarmed + not Granted  → returns `false`, `try_arm` is never called
///    (so a denied grant only ever pays for the cheap access poll);
///  - unarmed + Granted      → calls `try_arm` exactly once; its result is the
///    new armed state (a failed arm returns `false` and is safely retried next
///    tick — a failed [`spawn_monitor`] returns immediately without leaking).
pub fn rearm_tick(
    active: bool,
    check_access: impl FnOnce() -> crate::permissions::InputMonitoringAccess,
    try_arm: impl FnOnce() -> bool,
) -> bool {
    if active {
        return true;
    }
    if !should_attempt_arm(active, check_access()) {
        return false;
    }
    try_arm()
}

/// The tap's latched view of the hold key.
///
/// `down` is the edge-detection latch. `validated` records that, during the
/// current press, the CoreGraphics key-state table (`CGEventSourceKeyState`)
/// has *also* reported the key as down. Only a validated press may be ended by
/// a physical-state reading, because that table is not updated by synthetic
/// (posted) events — measured on this machine: a posted Right Option /
/// Right Control `flagsChanged` down reaches the tap, yet both the HID and the
/// combined-session key state keep reading "up" for the whole hold. Trusting
/// an unvalidated "up" would cut every synthetic hold (automation tools,
/// remappers, our own end-to-end harness) at the next watchdog tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HoldLatch {
    pub down: bool,
    pub validated: bool,
}

/// What prompted a physical-state resync of the hold latch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResyncCause {
    /// macOS disabled the tap (`TapDisabledByTimeout`/`ByUserInput`) and it was
    /// just re-armed: events may have been missed in either direction.
    TapReenabled,
    /// The periodic 5s watchdog tick while the tap was believed healthy.
    Watchdog,
}

/// Reconciles the latch with the key's physical state after a moment when
/// events may have been lost. Pure, so the policy is testable without a tap.
pub fn resync_hold_latch(
    latch: HoldLatch,
    physically_down: bool,
    cause: ResyncCause,
) -> (HoldLatch, Option<HotkeyEvent>) {
    const HELD: HoldLatch = HoldLatch { down: true, validated: true };
    match (latch.down, physically_down) {
        // Held, and the key-state table agrees: (re)confirm the hold. This is
        // the fix for the swallowed release — the old handler reset the latch
        // to "up" here, so the real key-up produced no edge.
        (true, true) => (HELD, None),
        // Latched down but physically up: a missed release — but only a hold
        // the table has already confirmed can be ended on its say-so.
        (true, false) if latch.validated => (HoldLatch::default(), Some(HotkeyEvent::HoldKeyReleased)),
        (true, false) => match cause {
            // Can't tell a missed release from a reading that never tracks
            // this press: keep the legacy reset (the next key-down is then
            // seen as a press, never swallowed).
            ResyncCause::TapReenabled => (HoldLatch::default(), None),
            ResyncCause::Watchdog => (latch, None),
        },
        // Latched up but physically down: the press happened while the tap
        // was dead. Only a re-arm reports it; the watchdog never starts a
        // recording on its own.
        (false, true) => match cause {
            ResyncCause::TapReenabled => (HELD, Some(HotkeyEvent::HoldKeyPressed)),
            ResyncCause::Watchdog => (latch, None),
        },
        (false, false) => (HoldLatch::default(), None),
    }
}

#[cfg(test)]
mod resync_tests {
    use super::*;

    const DOWN_VALIDATED: HoldLatch = HoldLatch { down: true, validated: true };
    const DOWN_UNVALIDATED: HoldLatch = HoldLatch { down: true, validated: false };

    #[test]
    fn tap_reenabled_mid_hold_keeps_the_hold_when_the_key_is_still_down() {
        // The swallowed-release bug: the old handler reset the latch to "up"
        // while the key was still held, so the real release produced no edge
        // and the recording ran on to the 600s cap.
        let (latch, ev) = resync_hold_latch(DOWN_VALIDATED, true, ResyncCause::TapReenabled);
        assert_eq!(latch, DOWN_VALIDATED);
        assert_eq!(ev, None);
    }

    #[test]
    fn tap_reenabled_after_a_missed_release_emits_the_release() {
        let (latch, ev) = resync_hold_latch(DOWN_VALIDATED, false, ResyncCause::TapReenabled);
        assert_eq!(latch, HoldLatch::default());
        assert_eq!(ev, Some(HotkeyEvent::HoldKeyReleased));
    }

    #[test]
    fn tap_reenabled_after_a_missed_press_emits_the_press() {
        let (latch, ev) = resync_hold_latch(HoldLatch::default(), true, ResyncCause::TapReenabled);
        assert_eq!(latch, DOWN_VALIDATED);
        assert_eq!(ev, Some(HotkeyEvent::HoldKeyPressed));
    }

    #[test]
    fn watchdog_ends_a_validated_hold_whose_key_is_physically_up() {
        let (latch, ev) = resync_hold_latch(DOWN_VALIDATED, false, ResyncCause::Watchdog);
        assert_eq!(latch, HoldLatch::default());
        assert_eq!(ev, Some(HotkeyEvent::HoldKeyReleased));
    }

    #[test]
    fn watchdog_validates_a_hold_the_key_state_confirms() {
        let (latch, ev) = resync_hold_latch(DOWN_UNVALIDATED, true, ResyncCause::Watchdog);
        assert_eq!(latch, DOWN_VALIDATED);
        assert_eq!(ev, None);
    }

    #[test]
    fn an_unvalidated_hold_is_never_ended_by_a_physical_up_reading() {
        // Synthetic presses never validate (the key-state table ignores posted
        // events), so neither path may end them on an "up" reading.
        assert_eq!(
            resync_hold_latch(DOWN_UNVALIDATED, false, ResyncCause::Watchdog),
            (DOWN_UNVALIDATED, None)
        );
        let (_, ev) = resync_hold_latch(DOWN_UNVALIDATED, false, ResyncCause::TapReenabled);
        assert_eq!(ev, None, "an unvalidated hold must not be released by a resync");
    }

    #[test]
    fn watchdog_never_originates_a_press() {
        assert_eq!(
            resync_hold_latch(HoldLatch::default(), true, ResyncCause::Watchdog),
            (HoldLatch::default(), None)
        );
    }
}

#[cfg(test)]
mod rearm_tests {
    use super::{rearm_tick, should_attempt_arm};
    use crate::permissions::InputMonitoringAccess::{Denied, Granted, Unknown};
    use std::cell::Cell;

    #[test]
    fn attempt_only_when_unarmed_and_granted() {
        assert!(should_attempt_arm(false, Granted));
        // Already armed: never re-attempt (re-calling spawn_monitor leaks).
        assert!(!should_attempt_arm(true, Granted));
        assert!(!should_attempt_arm(false, Denied));
        // Unknown must NOT be treated as granted.
        assert!(!should_attempt_arm(false, Unknown));
        assert!(!should_attempt_arm(true, Denied));
    }

    #[test]
    fn first_success_arms_then_never_spawns_again() {
        // Simulate the real driver loop: read armed state, tick, store result.
        let spawn_calls = Cell::new(0);
        let mut active = false;
        for _ in 0..10 {
            active = rearm_tick(
                active,
                || Granted,
                || {
                    spawn_calls.set(spawn_calls.get() + 1);
                    true // tap came up
                },
            );
        }
        assert_eq!(
            spawn_calls.get(),
            1,
            "spawn_monitor must be called exactly once and never after a success"
        );
        assert!(active);
    }

    #[test]
    fn denied_never_calls_spawn_monitor() {
        let spawn_calls = Cell::new(0);
        let mut active = false;
        for _ in 0..100 {
            active = rearm_tick(
                active,
                || Denied,
                || {
                    spawn_calls.set(spawn_calls.get() + 1);
                    true
                },
            );
        }
        assert_eq!(spawn_calls.get(), 0, "denied => IOHIDCheckAccess only, never CGEventTapCreate");
        assert!(!active);
    }

    #[test]
    fn repeated_failure_retries_without_arming_or_panic() {
        // Granted, but the tap keeps failing to install (try_arm → false).
        let spawn_calls = Cell::new(0);
        let mut active = false;
        for _ in 0..5 {
            active = rearm_tick(
                active,
                || Granted,
                || {
                    spawn_calls.set(spawn_calls.get() + 1);
                    false // tap failed to come up
                },
            );
        }
        assert_eq!(spawn_calls.get(), 5, "granted-but-failing retries each tick");
        assert!(!active, "a failed arm must never flip the armed state");
    }

    #[test]
    fn arms_once_on_the_grant_transition() {
        // Denied/Unknown for a while, then the user grants.
        let accesses = [Denied, Denied, Unknown, Granted, Granted];
        let spawn_calls = Cell::new(0);
        let mut active = false;
        for a in accesses {
            active = rearm_tick(
                active,
                || a,
                || {
                    spawn_calls.set(spawn_calls.get() + 1);
                    true
                },
            );
        }
        assert_eq!(spawn_calls.get(), 1, "spawn fires once, on the Granted transition");
        assert!(active);
    }
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
        // CoreGraphics' key-state table (`CGEventSource.h`). Not exposed by
        // `core-graphics` 0.25, so declared here like `CGEventTapEnable`.
        fn CGEventSourceKeyState(state_id: i32, key: u16) -> bool;
    }

    /// `kCGEventSourceStateHIDSystemState`: the state of the physical keys.
    const HID_SYSTEM_STATE: i32 = 1;

    /// Whether `keycode` is physically down, per CoreGraphics.
    ///
    /// Gotcha (h) check: this is a CoreGraphics (window-server) query, not a
    /// TSM/HIToolbox input-source call — it does no layout lookup and has no
    /// main-queue assertion; `key_state_is_safe_to_read_off_the_main_thread`
    /// hammers it from 8 threads at once to pin that. It is called on the tap
    /// thread only. Caveat measured on this machine: posted (synthetic)
    /// events do not update this table, which is why [`super::HoldLatch`]
    /// only trusts it for a press it has confirmed.
    pub(crate) fn key_is_down(keycode: u16) -> bool {
        unsafe { CGEventSourceKeyState(HID_SYSTEM_STATE, keycode) }
    }

    fn load_latch(down: &AtomicBool, validated: &AtomicBool) -> super::HoldLatch {
        super::HoldLatch { down: down.load(Ordering::Relaxed), validated: validated.load(Ordering::Relaxed) }
    }

    fn store_latch(down: &AtomicBool, validated: &AtomicBool, latch: super::HoldLatch) {
        down.store(latch.down, Ordering::Relaxed);
        validated.store(latch.validated, Ordering::Relaxed);
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
///
/// Known pre-existing quirk (not fixed here): `CGEventFlags` bits are
/// **device-independent** — `CGEventFlagShift` is set by *either* Shift key,
/// same for Control/Option/Command. So with e.g. a Right Shift binding, if
/// the user is also holding Left Shift when they release Right Shift, the
/// flag stays set and `down` reads `true` — no `HoldKeyReleased` edge fires
/// until *both* shift keys are up. Same failure mode for any
/// Control/Option/Command binding paired with its other-side twin. Exposing
/// the left-side keycodes as bindable options raises the odds of hitting
/// this; flagging it here so the next reader doesn't have to rediscover it.
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
            // Edge-detection latch for the hold key (see `HoldLatch`), plus
            // the keycode it currently pertains to. Shared between the
            // callback and the watchdog loop below — both run on this one
            // thread, so the atomics only satisfy the callback's `Send`
            // bound. `latch_keycode` lets us notice a live binding change and
            // drop a now-meaningless latch (F7).
            let hold_was_down = Arc::new(AtomicBool::new(false));
            let hold_validated = Arc::new(AtomicBool::new(false));
            let latch_keycode = Arc::new(AtomicU16::new(initial_keycode));
            let (wd_down, wd_validated, wd_latch_kc) =
                (hold_was_down.clone(), hold_validated.clone(), latch_keycode.clone());
            let wd_keycode = keycode_for_thread.clone();
            let wd_tx = tx.clone();

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
                        // immediately, then resync the latch from the key's
                        // physical state: while the tap was dead we may have
                        // missed a key-up *or* a key-down. The old handler
                        // forced the latch to "up", which swallowed the real
                        // release of a key still held — under heavy load
                        // (when timeouts happen) a long hold then ran on to
                        // the 600s cap. This is also the path a
                        // wake-from-sleep takes.
                        CGEventType::TapDisabledByTimeout
                        | CGEventType::TapDisabledByUserInput => {
                            let port = tap_port_for_cb.load(Ordering::Acquire);
                            if !port.is_null() {
                                unsafe { CGEventTapEnable(port as CFMachPortRef, true) };
                            }
                            let kc = keycode_for_thread.load(Ordering::Relaxed);
                            let before = load_latch(&hold_was_down, &hold_validated);
                            let (after, ev) = super::resync_hold_latch(
                                before,
                                key_is_down(kc),
                                super::ResyncCause::TapReenabled,
                            );
                            store_latch(&hold_was_down, &hold_validated, after);
                            eprintln!(
                                "[vzt-flow] hotkey tap disabled by macOS ({event_type:?}); re-armed, \
                                 latch {before:?} -> {after:?}{}",
                                ev.map(|e| format!(", emitting {e:?}")).unwrap_or_default()
                            );
                            if let Some(ev) = ev {
                                let _ = tx.send(ev);
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
                                store_latch(&hold_was_down, &hold_validated, super::HoldLatch::default());
                            }
                            let down = modifier_bit_for_keycode(physical_key)
                                .map(|b| event.get_flags().contains(b))
                                .unwrap_or(false);
                            let was_down = hold_was_down.swap(down, Ordering::Relaxed);
                            if down && !was_down {
                                // Does the key-state table see this press?
                                // Hardware presses: normally yes (else the
                                // watchdog confirms within 5s). Posted ones:
                                // never — so they are never resync-released.
                                hold_validated.store(key_is_down(this_keycode), Ordering::Relaxed);
                            } else if !down {
                                hold_validated.store(false, Ordering::Relaxed);
                            }
                            if let Some(ev) = hold_edge(was_down, down) {
                                let _ = tx.send(ev);
                            }
                        }
                        CGEventType::KeyDown => {
                            let physical_key = event
                                .get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE)
                                as u16;
                            let this_keycode = keycode_for_thread.load(Ordering::Relaxed);
                            // Accidental-press guard (see
                            // `HotkeyEvent::OtherKeyDuringHold`): the hold key
                            // is physically down and a *different* key just
                            // produced a keyDown. With the default Right Option
                            // binding that is the macOS special-character
                            // modifier at work, so this hold is really the user
                            // typing a special char — report it so the
                            // coordinator discards any false start and never
                            // arms hands-free. The `!= this_keycode` guard keeps
                            // a (hypothetical) non-modifier binding's own
                            // auto-repeat keyDowns from self-triggering. This
                            // takes precedence over the Escape path below: both
                            // cancel a live recording, and reporting the guard
                            // also suppresses a not-yet-recording false start.
                            if hold_was_down.load(Ordering::Relaxed)
                                && physical_key != this_keycode
                            {
                                let _ = tx.send(HotkeyEvent::OtherKeyDuringHold);
                            } else if physical_key == ESCAPE_KEYCODE
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
            //
            // Each wake also checks a latched-down hold against the key's
            // physical state and releases it if the key is up (a release that
            // never reached us — including the Left/Right twin-modifier quirk
            // documented on `hold_edge`). Only a validated press qualifies;
            // see `resync_hold_latch`.
            loop {
                CFRunLoop::run_in_mode(
                    unsafe { kCFRunLoopDefaultMode },
                    WATCHDOG_INTERVAL,
                    false,
                );
                tap.enable();
                let kc = wd_keycode.load(Ordering::Relaxed);
                if wd_latch_kc.load(Ordering::Relaxed) != kc {
                    continue; // binding changed; the callback resets on the next event
                }
                let before = load_latch(&wd_down, &wd_validated);
                if !before.down {
                    continue;
                }
                let (after, ev) = super::resync_hold_latch(before, key_is_down(kc), super::ResyncCause::Watchdog);
                store_latch(&wd_down, &wd_validated, after);
                if let Some(ev) = ev {
                    eprintln!(
                        "[vzt-flow] hotkey watchdog: the hold key is physically up but its release \
                         never arrived; emitting {ev:?}"
                    );
                    let _ = wd_tx.send(ev);
                }
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

    /// Gotcha (h): HIToolbox/TSM input-source APIs abort the process when
    /// called off the main queue, and that is a race — one background caller
    /// usually survives. `CGEventSourceKeyState` runs on the tap thread, so
    /// prove it is not in that class the same way the paste fix was proven:
    /// concurrently, from 8 threads.
    #[test]
    fn key_state_is_safe_to_read_off_the_main_thread() {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                std::thread::spawn(|| {
                    let mut downs = 0;
                    for _ in 0..500 {
                        downs += key_is_down(61) as u32;
                    }
                    downs
                })
            })
            .collect();
        for h in handles {
            h.join().expect("key-state read panicked off the main thread");
        }
    }

    #[test]
    fn supported_keycodes_all_map_to_a_modifier_bit() {
        for &kc in crate::hotkey::supported_hotkey_keycodes() {
            assert!(
                modifier_bit_for_keycode(kc).is_some(),
                "keycode {kc} is in supported_hotkey_keycodes() but modifier_bit_for_keycode returned None"
            );
        }
    }

    #[test]
    fn non_modifier_keycodes_map_to_none_and_are_unlisted() {
        // 0 'a', 12 'q', 49 space, 36 return — ordinary (non-modifier) keys.
        for kc in [0u16, 12, 49, 36] {
            assert_eq!(modifier_bit_for_keycode(kc), None, "keycode {kc} unexpectedly has a modifier bit");
            assert!(!crate::hotkey::hotkey_keycode_is_supported(kc), "keycode {kc} unexpectedly in supported list");
            assert!(!crate::hotkey::hotkey_keycode_is_hold_capable(kc), "keycode {kc} unexpectedly hold-capable");
        }
    }

    #[test]
    fn caps_lock_is_supported_but_not_hold_capable() {
        assert!(crate::hotkey::hotkey_keycode_is_supported(57));
        assert!(
            !crate::hotkey::hotkey_keycode_is_hold_capable(57),
            "Caps Lock reflects latched state (AlphaShift), not hold state — must not be hold-capable"
        );
    }

    #[test]
    fn hold_capable_set_is_exactly_supported_minus_caps_lock() {
        let mut expected: Vec<u16> = crate::hotkey::supported_hotkey_keycodes()
            .iter()
            .copied()
            .filter(|&kc| kc != 57)
            .collect();
        expected.sort_unstable();
        let mut actual: Vec<u16> = crate::hotkey::hold_capable_hotkey_keycodes().to_vec();
        actual.sort_unstable();
        assert_eq!(actual, expected);
        assert_eq!(actual, vec![54, 55, 56, 58, 59, 60, 61, 62, 63]);
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
