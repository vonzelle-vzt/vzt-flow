//! macOS permission + secure-input checks.
//!
//! These call directly into `ApplicationServices` (Carbon/HIToolbox) via
//! thin FFI declarations rather than pulling in a wrapper crate — each
//! function is a single well-known no-argument (or null-argument) C call,
//! so hand-rolled `extern "C"` bindings are less surface area than a full
//! dependency.

#[cfg(target_os = "macos")]
mod macos {
    use std::os::raw::c_void;

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

    pub fn secure_input_enabled() -> bool {
        unsafe { IsSecureEventInputEnabled() }
    }

    pub fn accessibility_trusted() -> bool {
        unsafe { AXIsProcessTrustedWithOptions(std::ptr::null()) }
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
}

pub use macos::{accessibility_trusted, secure_input_enabled};

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
