//! macOS permission + secure-input checks.
//!
//! These call directly into `ApplicationServices` (Carbon/HIToolbox) via
//! thin FFI declarations rather than pulling in a wrapper crate — each
//! function is a single well-known no-argument (or null-argument) C call,
//! so hand-rolled `extern "C"` bindings are less surface area than a full
//! dependency.

/// Result of `IOHIDCheckAccess(kIOHIDRequestTypeListenEvent)` — whether this
/// process may listen to global keyboard events, i.e. macOS "Input
/// Monitoring", the permission the hold-to-talk `CGEventTap` needs. Mirrors
/// the three `kIOHIDAccessType*` values. `Unknown` is deliberately distinct
/// from `Granted`: an indeterminate check must never be read as permission to
/// arm the tap (which would make us hammer `CGEventTapCreate`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMonitoringAccess {
    Granted,
    Denied,
    Unknown,
}

#[cfg(target_os = "macos")]
mod macos {
    use std::os::raw::{c_int, c_void};

    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        /// Returns true while any process on the system has "secure input
        /// mode" enabled (e.g. a password field is focused) — during which
        /// synthetic keystrokes must not be sent, since the OS blocks (and
        /// some apps flag) programmatic input to secure fields.
        fn IsSecureEventInputEnabled() -> bool;

        /// `options` is a `CFDictionaryRef`; passing null means "just report
        /// current trust status, do not prompt the user".
        fn AXIsProcessTrustedWithOptions(options: *const c_void) -> bool;
    }

    // Input Monitoring (global-keystroke) access lives in IOKit, not
    // ApplicationServices — a distinct TCC bucket from Accessibility, and the
    // one a `CGEventTap` actually requires. `AXIsProcessTrustedWithOptions`
    // above cannot see it (grep the tree: this was the whole late-grant blind
    // spot). Declared thin per this module's hand-rolled-FFI convention rather
    // than pulling in an `io-kit-sys` crate; `#[link(name = "IOKit")]` is all
    // the extra linkage needed (IOKit is a base macOS framework, always
    // present — see /System/Library/Frameworks/IOKit.framework).
    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        /// `IOHIDAccessType IOHIDCheckAccess(IOHIDRequestType)` — non-
        /// prompting; reports current access. Both parameter and return are C
        /// `enum`s (ABI `int` → `c_int`).
        fn IOHIDCheckAccess(request: c_int) -> c_int;
        /// `bool IOHIDRequestAccess(IOHIDRequestType)` — prompts the user the
        /// first time (like `probe_microphone` triggering the mic prompt);
        /// afterwards it just returns the current grant without re-prompting.
        fn IOHIDRequestAccess(request: c_int) -> bool;
    }

    // From <IOKit/hidsystem/IOHIDLib.h> in the MacOSX SDK (verified against the
    // header, not memory — the two request types are trivially invertible):
    //   typedef enum { kIOHIDRequestTypePostEvent,   // = 0
    //                  kIOHIDRequestTypeListenEvent } // = 1
    //   IOHIDRequestType;
    // Input Monitoring is the *listen* (receive keystrokes) capability, so 1.
    const KIOHID_REQUEST_TYPE_LISTEN_EVENT: c_int = 1;

    pub fn secure_input_enabled() -> bool {
        unsafe { IsSecureEventInputEnabled() }
    }

    pub fn accessibility_trusted() -> bool {
        unsafe { AXIsProcessTrustedWithOptions(std::ptr::null()) }
    }

    /// Current Input Monitoring access (non-prompting). Maps the
    /// `kIOHIDAccessType*` return: `Granted = 0`, `Denied = 1`,
    /// `Unknown = 2` (and any unexpected value → `Unknown`).
    pub fn input_monitoring_access() -> super::InputMonitoringAccess {
        match unsafe { IOHIDCheckAccess(KIOHID_REQUEST_TYPE_LISTEN_EVENT) } {
            0 => super::InputMonitoringAccess::Granted,
            1 => super::InputMonitoringAccess::Denied,
            _ => super::InputMonitoringAccess::Unknown,
        }
    }

    /// True only when access is `Granted` — `Unknown` is never treated as
    /// granted.
    pub fn input_monitoring_trusted() -> bool {
        matches!(
            input_monitoring_access(),
            super::InputMonitoringAccess::Granted
        )
    }

    /// Prompts the user for Input Monitoring access (first call only; macOS
    /// won't re-prompt once decided — the Settings pane deep link is the
    /// fallback after that). Returns whether access is granted.
    pub fn request_input_monitoring() -> bool {
        unsafe { IOHIDRequestAccess(KIOHID_REQUEST_TYPE_LISTEN_EVENT) }
    }
}

#[cfg(not(target_os = "macos"))]
mod macos {
    /// No "secure input mode" concept off macOS; never block a paste for it.
    pub fn secure_input_enabled() -> bool {
        false
    }
    /// No TCC/Accessibility-style permission gate off macOS — neither Windows
    /// nor Linux (X11) has an equivalent one-time grant to check, so report
    /// "trusted" rather than permanently skipping every paste. Platform paste
    /// failures that *can't* be detected via a permission bit are handled in
    /// `insert.rs` instead: Windows UIPI privilege-boundary drops, and Linux
    /// Wayland (where synthetic Ctrl+V can't reach native Wayland clients, so
    /// the transcript is left on the clipboard). If a future Linux build uses
    /// an `evdev`/`uinput` input backend instead of X11 XTEST, this is where a
    /// real "user is in the `input` group / has /dev/uinput access" check
    /// would live.
    pub fn accessibility_trusted() -> bool {
        true
    }

    /// No macOS-style Input Monitoring gate off macOS — Windows/Linux use
    /// `tauri-plugin-global-shortcut`, which has no equivalent one-time TCC
    /// grant to poll. Report `Granted`/`true` (nothing to arm behind), mirroring
    /// the `accessibility_trusted -> true` convention above.
    pub fn input_monitoring_access() -> super::InputMonitoringAccess {
        super::InputMonitoringAccess::Granted
    }
    pub fn input_monitoring_trusted() -> bool {
        true
    }
    pub fn request_input_monitoring() -> bool {
        true
    }
}

pub use macos::{
    accessibility_trusted, input_monitoring_access, input_monitoring_trusted,
    request_input_monitoring, secure_input_enabled,
};

/// Opens System Settings to the Accessibility privacy pane so the user can
/// grant this app permission.
pub fn open_accessibility_settings() {
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open")
            .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
            .status();
    }
}

/// Opens System Settings to the Input Monitoring privacy pane — the permission
/// the global hold-to-talk `CGEventTap` needs (distinct from Accessibility).
/// Mirrors [`open_accessibility_settings`]; the pane anchor is
/// `Privacy_ListenEvent`. No-op off macOS.
pub fn open_input_monitoring_settings() {
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open")
            .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent")
            .status();
    }
}

/// Best-effort check that the microphone is reachable: opens a cpal input
/// stream briefly and tears it down. macOS surfaces the mic permission
/// prompt (and subsequent grant/deny) the first time an app actually opens
/// a capture stream, so this doubles as the trigger for that OS prompt.
pub fn probe_microphone() -> bool {
    crate::audio::default_input_device_info().is_ok()
}

/// Frontmost application's bundle identifier, if cheaply available.
#[cfg(target_os = "macos")]
pub fn frontmost_bundle_id() -> Option<String> {
    use objc2::rc::Retained;
    use objc2_app_kit::NSWorkspace;
    use objc2_foundation::NSString;

    let workspace = NSWorkspace::sharedWorkspace();
    let app: Option<Retained<objc2_app_kit::NSRunningApplication>> = workspace.frontmostApplication();
    let bundle_id: Retained<NSString> = app?.bundleIdentifier()?;
    Some(bundle_id.to_string())
}

#[cfg(not(target_os = "macos"))]
pub fn frontmost_bundle_id() -> Option<String> {
    None
}
