//! Local, privacy-preserving meeting **auto-detection**.
//!
//! The detector answers one question on a cheap 5-second poll: "is the user
//! in a Zoom / Google Meet / Microsoft Teams call right now?" It combines two
//! entirely local signals and never captures pixels, screenshots, or OCR —
//! **window titles only**:
//!
//! * **Signal A — a meeting window is open.** We enumerate on-screen windows
//!   with `CGWindowListCopyWindowInfo` and match each window's *owner* and
//!   *title* against a small, data-driven [rule table](Rule). Reading other
//!   apps' window *titles* requires the **Screen Recording** TCC permission —
//!   the same grant ScreenCaptureKit needs for meeting capture — so the
//!   detector checks [`screen_capture_permitted`] and degrades gracefully
//!   (titles come back empty, nothing matches) when it isn't granted.
//! * **Signal B — the microphone is live.** CoreAudio's
//!   `kAudioDevicePropertyDeviceIsRunningSomewhere` on the default input
//!   device is a cheap boolean poll that's true whenever *any* process is
//!   capturing from the mic.
//!
//! A meeting is **A AND B**. A [`Debouncer`] smooths the raw polls: it takes
//! two consecutive positive polls to *start* considering a meeting (so a
//! transient title match doesn't fire), and the meeting is only considered
//! *ended* after Signal A (the window) has been absent for three consecutive
//! polls — never on Signal B alone, because muting yourself flips B off
//! mid-call.
//!
//! ### Privacy
//!
//! Everything here is local and metadata-only. We read window owner/title
//! strings and a single mic-activity boolean. No audio, no pixels, no
//! screenshots, no network. The matching logic ([`match_meeting`]) and the
//! debounce state machine ([`Debouncer`]) are pure and unit-tested; only the
//! two signal *sources* touch the OS.

use std::time::Duration;

/// How often the background detector samples the two signals.
pub const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Consecutive positive (A && B) polls required before a meeting is
/// considered started. Two polls (~10s) debounces a transient title match.
pub const START_POLLS: u32 = 2;

/// Consecutive Signal-A-absent polls required before a meeting is considered
/// ended. Three polls (~15s) rides out a window briefly losing focus/title
/// and, crucially, does **not** end on Signal B alone (mute toggles B).
pub const END_POLLS: u32 = 3;

/// Which conferencing app a window belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeetingApp {
    Zoom,
    GoogleMeet,
    Teams,
}

impl MeetingApp {
    /// Human-readable label used in notifications / transcript titles.
    pub fn label(&self) -> &'static str {
        match self {
            MeetingApp::Zoom => "Zoom",
            MeetingApp::GoogleMeet => "Google Meet",
            MeetingApp::Teams => "Microsoft Teams",
        }
    }
}

/// The owner + title of a single on-screen window — the only two pieces of
/// per-window metadata the matcher looks at.
#[derive(Debug, Clone)]
pub struct WindowInfo {
    /// `kCGWindowOwnerName` — the owning application's name (always readable).
    pub owner: String,
    /// `kCGWindowName` — the window's title. Empty unless Screen Recording
    /// permission is granted (macOS hides other apps' titles otherwise).
    pub title: String,
}

impl WindowInfo {
    pub fn new(owner: impl Into<String>, title: impl Into<String>) -> Self {
        Self { owner: owner.into(), title: title.into() }
    }
}

/// Browser application owner-names whose windows can host a Google Meet call.
/// Kept as a simple list so adding a browser is a one-line change.
const BROWSERS: &[&str] = &[
    "Google Chrome",
    "Google Chrome Canary",
    "Chromium",
    "Safari",
    "Safari Technology Preview",
    "Arc",
    "Microsoft Edge",
    "Brave Browser",
    "Vivaldi",
    "Opera",
    "Firefox",
];

/// How a rule matches a window's *owner* name.
#[derive(Debug, Clone, Copy)]
enum Owner {
    /// Owner must equal this exactly (e.g. Zoom's owner is `"zoom.us"`).
    Exact(&'static str),
    /// Owner must contain this substring (Teams ships under several owner
    /// names: "Microsoft Teams", "Microsoft Teams (work or school)", …).
    Contains(&'static str),
    /// Owner must be one of [`BROWSERS`] (Google Meet runs in a browser tab).
    AnyBrowser,
}

impl Owner {
    fn matches(&self, owner: &str) -> bool {
        match self {
            Owner::Exact(s) => owner == *s,
            Owner::Contains(s) => owner.contains(s),
            Owner::AnyBrowser => BROWSERS.iter().any(|b| owner == *b),
        }
    }
}

/// One entry in the data-driven detection table: an app, how to match its
/// owning process, and a predicate over the window title. Adding a new
/// platform is appending one `Rule` plus its title predicate.
struct Rule {
    app: MeetingApp,
    owner: Owner,
    title: fn(&str) -> bool,
}

/// Normalizes the several dash characters real-world titles use — hyphen-minus
/// `-`, en dash `–` (U+2013), em dash `—` (U+2014), and the non-breaking
/// hyphen `‑` (U+2011) — to a plain hyphen so a single title predicate matches
/// every localized variant. (macOS/Chrome render the Meet separator as an en
/// dash; other locales/apps vary.)
fn normalize_dashes(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}' => '-',
            other => other,
        })
        .collect()
}

/// Zoom in-call window: the app's own name is `zoom.us` and the meeting
/// window is titled "Zoom Meeting" (the idle/home window is just "Zoom", which
/// must NOT match).
fn title_zoom(title: &str) -> bool {
    title.contains("Zoom Meeting")
}

/// Google Meet tab: title starts with "Meet -" / contains "Meet - " (after
/// dash normalization) or references `meet.google.com`. Crafted so a
/// "Meetup.com" tab does not match (no "Meet -", no google.com host).
fn title_meet(title: &str) -> bool {
    let n = normalize_dashes(title);
    n.starts_with("Meet -") || n.contains("Meet - ") || n.contains("meet.google.com")
}

/// Microsoft Teams call window: owned by a "Microsoft Teams" process and
/// titled with "Meeting" (e.g. "Meeting in <name> | Microsoft Teams",
/// "Meeting with <name> | Microsoft Teams"). Requiring "Meeting" keeps the
/// idle "Chat | Microsoft Teams" window from matching.
fn title_teams(title: &str) -> bool {
    title.contains("Meeting")
}

/// The detection table. First matching rule wins.
const RULES: &[Rule] = &[
    Rule { app: MeetingApp::Zoom, owner: Owner::Exact("zoom.us"), title: title_zoom },
    Rule { app: MeetingApp::GoogleMeet, owner: Owner::AnyBrowser, title: title_meet },
    Rule { app: MeetingApp::Teams, owner: Owner::Contains("Microsoft Teams"), title: title_teams },
];

/// Pure Signal A: returns the first [`MeetingApp`] whose rule matches any
/// window in `windows`, or `None` if none do. This is the whole title-match
/// policy and is exhaustively unit-tested against real-world title variants.
pub fn match_meeting(windows: &[WindowInfo]) -> Option<MeetingApp> {
    for w in windows {
        for rule in RULES {
            if rule.owner.matches(&w.owner) && (rule.title)(&w.title) {
                return Some(rule.app);
            }
        }
    }
    None
}

/// An event emitted by the [`Debouncer`] as poll results are fed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectEvent {
    /// Nothing changed this poll.
    None,
    /// A meeting just crossed the start threshold. Fires exactly once per
    /// meeting (on the [`START_POLLS`]-th consecutive positive poll).
    Started(MeetingApp),
    /// The current meeting just crossed the end threshold (Signal A absent
    /// for [`END_POLLS`] consecutive polls). Fires exactly once.
    Ended,
}

/// The debounce state machine — pure, deterministic, and unit-tested over
/// poll sequences. Feed it one [`poll`](Debouncer::poll) per sample with the
/// raw Signal A (matched app, if any) and Signal B (mic live); it emits a
/// [`DetectEvent`].
///
/// It emits `Started` at most once per meeting and does not re-arm until an
/// `Ended`, so callers can safely map `Started` → "ask/auto-start" without
/// nagging every poll. Once `Started` has fired the machine is "in a meeting"
/// even if the caller chose not to start a session (the user answered "no" to
/// the ask prompt) — so the same meeting is never re-prompted, and the machine
/// re-arms only after the meeting genuinely ends.
#[derive(Debug, Default)]
pub struct Debouncer {
    in_meeting: bool,
    /// Consecutive (A && B) polls while not yet in a meeting.
    positive_streak: u32,
    /// Consecutive (A absent) polls while in a meeting.
    absent_streak: u32,
}

impl Debouncer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the machine currently considers a meeting to be in progress.
    pub fn in_meeting(&self) -> bool {
        self.in_meeting
    }

    /// Advances the state machine by one poll. `app` is the Signal-A match
    /// (`Some` if a meeting window is present), `mic_live` is Signal B.
    pub fn poll(&mut self, app: Option<MeetingApp>, mic_live: bool) -> DetectEvent {
        if self.in_meeting {
            // END is governed by Signal A only — muting flips B off mid-call,
            // so B must never end a meeting.
            if app.is_none() {
                self.absent_streak += 1;
                if self.absent_streak >= END_POLLS {
                    self.in_meeting = false;
                    self.absent_streak = 0;
                    self.positive_streak = 0;
                    return DetectEvent::Ended;
                }
            } else {
                self.absent_streak = 0;
            }
            DetectEvent::None
        } else {
            match app {
                Some(app) if mic_live => {
                    self.positive_streak += 1;
                    if self.positive_streak >= START_POLLS {
                        self.in_meeting = true;
                        self.positive_streak = 0;
                        self.absent_streak = 0;
                        return DetectEvent::Started(app);
                    }
                    DetectEvent::None
                }
                _ => {
                    self.positive_streak = 0;
                    DetectEvent::None
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Signal sources — the only OS-touching, non-pure part of this module.
// ---------------------------------------------------------------------------

/// Whether this process holds the Screen Recording permission, which is
/// required to read *other* apps' window titles (Signal A) — the same grant
/// ScreenCaptureKit needs to capture meeting audio. Checked without prompting.
#[cfg(target_os = "macos")]
pub fn screen_capture_permitted() -> bool {
    extern "C" {
        fn CGPreflightScreenCaptureAccess() -> bool;
    }
    unsafe { CGPreflightScreenCaptureAccess() }
}

#[cfg(not(target_os = "macos"))]
pub fn screen_capture_permitted() -> bool {
    false
}

/// Signal A source: the current on-screen windows (owner + title), filtered to
/// normal application windows. Returns an empty vec off macOS.
#[cfg(target_os = "macos")]
pub fn list_windows() -> Vec<WindowInfo> {
    use core_foundation::base::{CFType, TCFType};
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::string::CFString;
    use core_graphics::window::{
        copy_window_info, kCGWindowLayer, kCGWindowListExcludeDesktopElements,
        kCGWindowListOptionOnScreenOnly, kCGWindowName, kCGWindowOwnerName,
    };

    let option = kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements;
    let Some(array) = copy_window_info(option, 0) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    // The array elements are CFDictionaryRefs describing each window.
    for i in 0..array.len() {
        let Some(item) = array.get(i) else { continue };
        // SAFETY: CGWindowListCopyWindowInfo yields CFDictionary elements.
        let dict = unsafe { CFDictionary::<CFString, CFType>::wrap_under_get_rule(*item as _) };

        // Only consider normal windows (layer 0). Menu-bar extras, the Dock,
        // tooltips, etc. live on other layers and would only add noise.
        let layer = unsafe { get_number(&dict, kCGWindowLayer) };
        if layer.unwrap_or(0) != 0 {
            continue;
        }

        let owner = unsafe { get_string(&dict, kCGWindowOwnerName) }.unwrap_or_default();
        let title = unsafe { get_string(&dict, kCGWindowName) }.unwrap_or_default();
        if owner.is_empty() && title.is_empty() {
            continue;
        }
        out.push(WindowInfo { owner, title });
    }
    out
}

/// Reads a CFString value out of a window-info dictionary by its (extern
/// static) CFString key.
#[cfg(target_os = "macos")]
unsafe fn get_string(
    dict: &core_foundation::dictionary::CFDictionary<
        core_foundation::string::CFString,
        core_foundation::base::CFType,
    >,
    key: core_foundation::string::CFStringRef,
) -> Option<String> {
    use core_foundation::base::TCFType;
    use core_foundation::string::CFString;
    let key = CFString::wrap_under_get_rule(key);
    let value = dict.find(&key)?;
    value.downcast::<CFString>().map(|s| s.to_string())
}

/// Reads an integer CFNumber value (e.g. the window layer) out of a
/// window-info dictionary.
#[cfg(target_os = "macos")]
unsafe fn get_number(
    dict: &core_foundation::dictionary::CFDictionary<
        core_foundation::string::CFString,
        core_foundation::base::CFType,
    >,
    key: core_foundation::string::CFStringRef,
) -> Option<i64> {
    use core_foundation::base::TCFType;
    use core_foundation::number::CFNumber;
    use core_foundation::string::CFString;
    let key = CFString::wrap_under_get_rule(key);
    let value = dict.find(&key)?;
    value.downcast::<CFNumber>().and_then(|n| n.to_i64())
}

#[cfg(not(target_os = "macos"))]
pub fn list_windows() -> Vec<WindowInfo> {
    Vec::new()
}

/// Signal B source: whether the default input device (microphone) is currently
/// running in *some* process — a cheap CoreAudio property poll. Returns false
/// off macOS.
#[cfg(target_os = "macos")]
pub fn mic_in_use() -> bool {
    coreaudio_mic::mic_in_use()
}

#[cfg(not(target_os = "macos"))]
pub fn mic_in_use() -> bool {
    false
}

/// Minimal CoreAudio FFI for the "is the mic live anywhere" boolean. Kept
/// self-contained (raw `AudioObjectGetPropertyData`) rather than pulling in a
/// CoreAudio binding crate for a single property read.
#[cfg(target_os = "macos")]
mod coreaudio_mic {
    use std::os::raw::c_void;

    type OSStatus = i32;
    type AudioObjectID = u32;

    #[repr(C)]
    struct AudioObjectPropertyAddress {
        selector: u32,
        scope: u32,
        element: u32,
    }

    const K_AUDIO_OBJECT_SYSTEM_OBJECT: AudioObjectID = 1;

    /// Builds a FourCharCode (`'abcd'`) constant the way the CoreAudio headers
    /// do — big-endian packing of four ASCII bytes.
    const fn fourcc(s: &[u8; 4]) -> u32 {
        ((s[0] as u32) << 24) | ((s[1] as u32) << 16) | ((s[2] as u32) << 8) | (s[3] as u32)
    }

    // kAudioHardwarePropertyDefaultInputDevice = 'dIn '
    const DEFAULT_INPUT_DEVICE: u32 = fourcc(b"dIn ");
    // kAudioDevicePropertyDeviceIsRunningSomewhere = 'gone'
    const IS_RUNNING_SOMEWHERE: u32 = fourcc(b"gone");
    // kAudioObjectPropertyScopeGlobal = 'glob'
    const SCOPE_GLOBAL: u32 = fourcc(b"glob");
    // kAudioObjectPropertyElementMain = 0
    const ELEMENT_MAIN: u32 = 0;

    #[link(name = "CoreAudio", kind = "framework")]
    extern "C" {
        fn AudioObjectGetPropertyData(
            in_object_id: AudioObjectID,
            in_address: *const AudioObjectPropertyAddress,
            in_qualifier_data_size: u32,
            in_qualifier_data: *const c_void,
            io_data_size: *mut u32,
            out_data: *mut c_void,
        ) -> OSStatus;
    }

    pub fn mic_in_use() -> bool {
        unsafe {
            // 1) Resolve the default input device.
            let addr = AudioObjectPropertyAddress {
                selector: DEFAULT_INPUT_DEVICE,
                scope: SCOPE_GLOBAL,
                element: ELEMENT_MAIN,
            };
            let mut device: AudioObjectID = 0;
            let mut size = std::mem::size_of::<AudioObjectID>() as u32;
            let st = AudioObjectGetPropertyData(
                K_AUDIO_OBJECT_SYSTEM_OBJECT,
                &addr,
                0,
                std::ptr::null(),
                &mut size,
                &mut device as *mut _ as *mut c_void,
            );
            if st != 0 || device == 0 {
                return false;
            }

            // 2) Is that device running (capturing) in any process?
            let running_addr = AudioObjectPropertyAddress {
                selector: IS_RUNNING_SOMEWHERE,
                scope: SCOPE_GLOBAL,
                element: ELEMENT_MAIN,
            };
            let mut running: u32 = 0;
            let mut rsize = std::mem::size_of::<u32>() as u32;
            let st2 = AudioObjectGetPropertyData(
                device,
                &running_addr,
                0,
                std::ptr::null(),
                &mut rsize,
                &mut running as *mut _ as *mut c_void,
            );
            st2 == 0 && running != 0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(owner: &str, title: &str) -> WindowInfo {
        WindowInfo::new(owner, title)
    }

    // --- Signal A: title-match table (positives) ---

    #[test]
    fn matches_zoom_meeting_window() {
        let ws = vec![win("zoom.us", "Zoom Meeting")];
        assert_eq!(match_meeting(&ws), Some(MeetingApp::Zoom));
    }

    #[test]
    fn matches_zoom_meeting_with_participant_suffix() {
        let ws = vec![win("zoom.us", "Zoom Meeting - Weekly Sync")];
        assert_eq!(match_meeting(&ws), Some(MeetingApp::Zoom));
    }

    #[test]
    fn matches_google_meet_en_dash_chrome() {
        // Chrome renders the Meet tab title with an en dash (U+2013).
        let ws = vec![win("Google Chrome", "Meet \u{2013} abc-defg-hij")];
        assert_eq!(match_meeting(&ws), Some(MeetingApp::GoogleMeet));
    }

    #[test]
    fn matches_google_meet_plain_hyphen_safari() {
        let ws = vec![win("Safari", "Meet - Daily Standup")];
        assert_eq!(match_meeting(&ws), Some(MeetingApp::GoogleMeet));
    }

    #[test]
    fn matches_google_meet_em_dash_arc() {
        let ws = vec![win("Arc", "Meet \u{2014} Project Kickoff")];
        assert_eq!(match_meeting(&ws), Some(MeetingApp::GoogleMeet));
    }

    #[test]
    fn matches_google_meet_by_url_in_title() {
        let ws = vec![win("Microsoft Edge", "meet.google.com/abc-defg-hij")];
        assert_eq!(match_meeting(&ws), Some(MeetingApp::GoogleMeet));
    }

    #[test]
    fn matches_teams_meeting_window() {
        let ws = vec![win("Microsoft Teams", "Meeting in Design Review | Microsoft Teams")];
        assert_eq!(match_meeting(&ws), Some(MeetingApp::Teams));
    }

    #[test]
    fn matches_teams_work_or_school_owner_variant() {
        let ws = vec![win("Microsoft Teams (work or school)", "Meeting with Alex | Microsoft Teams")];
        assert_eq!(match_meeting(&ws), Some(MeetingApp::Teams));
    }

    // --- Signal A: negatives (must NOT match) ---

    #[test]
    fn zoom_idle_home_window_does_not_match() {
        // Zoom open but not in a call: the home window is titled just "Zoom".
        let ws = vec![win("zoom.us", "Zoom")];
        assert_eq!(match_meeting(&ws), None);
    }

    #[test]
    fn meetup_tab_does_not_match_google_meet() {
        // The classic false positive: a "Meetup.com" browser tab.
        let ws = vec![win("Google Chrome", "Upcoming Events | Meetup.com")];
        assert_eq!(match_meeting(&ws), None);
    }

    #[test]
    fn meet_word_in_prose_title_does_not_match() {
        let ws = vec![win("Safari", "How to meet people online - Blog")];
        assert_eq!(match_meeting(&ws), None);
    }

    #[test]
    fn teams_chat_window_does_not_match() {
        // Teams open on the Chat tab, not in a call.
        let ws = vec![win("Microsoft Teams", "Chat | Microsoft Teams")];
        assert_eq!(match_meeting(&ws), None);
    }

    #[test]
    fn non_browser_meet_title_does_not_match() {
        // A random app that happens to have "Meet - " in its title but is not
        // a browser must not be taken for Google Meet.
        let ws = vec![win("Notes", "Meet - agenda notes")];
        assert_eq!(match_meeting(&ws), None);
    }

    #[test]
    fn empty_window_list_matches_nothing() {
        assert_eq!(match_meeting(&[]), None);
    }

    #[test]
    fn first_matching_app_wins_among_many_windows() {
        let ws = vec![
            win("Finder", "Downloads"),
            win("Google Chrome", "Inbox (3)"),
            win("zoom.us", "Zoom Meeting"),
        ];
        assert_eq!(match_meeting(&ws), Some(MeetingApp::Zoom));
    }

    // --- Signal B interaction + Debouncer state machine ---

    #[test]
    fn debounce_requires_two_positive_polls_to_start() {
        let mut d = Debouncer::new();
        // First positive poll: consider, but don't start yet.
        assert_eq!(d.poll(Some(MeetingApp::Zoom), true), DetectEvent::None);
        assert!(!d.in_meeting());
        // Second consecutive positive poll: start.
        assert_eq!(d.poll(Some(MeetingApp::Zoom), true), DetectEvent::Started(MeetingApp::Zoom));
        assert!(d.in_meeting());
    }

    #[test]
    fn debounce_positive_streak_resets_on_gap() {
        let mut d = Debouncer::new();
        assert_eq!(d.poll(Some(MeetingApp::Teams), true), DetectEvent::None);
        // Signal A present but mic not live — resets the streak.
        assert_eq!(d.poll(Some(MeetingApp::Teams), false), DetectEvent::None);
        // Need two fresh consecutive positives again.
        assert_eq!(d.poll(Some(MeetingApp::Teams), true), DetectEvent::None);
        assert_eq!(d.poll(Some(MeetingApp::Teams), true), DetectEvent::Started(MeetingApp::Teams));
    }

    #[test]
    fn debounce_does_not_start_without_mic() {
        let mut d = Debouncer::new();
        // Meeting window open but mic never live (e.g. viewing a webinar
        // muted) — must never start.
        for _ in 0..5 {
            assert_eq!(d.poll(Some(MeetingApp::GoogleMeet), false), DetectEvent::None);
        }
        assert!(!d.in_meeting());
    }

    #[test]
    fn debounce_ends_only_after_three_absent_polls() {
        let mut d = Debouncer::new();
        d.poll(Some(MeetingApp::Zoom), true);
        d.poll(Some(MeetingApp::Zoom), true); // Started
        assert!(d.in_meeting());
        // Two absent polls: still in meeting.
        assert_eq!(d.poll(None, false), DetectEvent::None);
        assert_eq!(d.poll(None, false), DetectEvent::None);
        assert!(d.in_meeting());
        // Third absent poll: ended.
        assert_eq!(d.poll(None, false), DetectEvent::Ended);
        assert!(!d.in_meeting());
    }

    #[test]
    fn debounce_mute_does_not_end_meeting() {
        let mut d = Debouncer::new();
        d.poll(Some(MeetingApp::Zoom), true);
        d.poll(Some(MeetingApp::Zoom), true); // Started
        // User mutes: Signal B goes false but the window (A) is still there.
        // The meeting must NOT end no matter how long they stay muted.
        for _ in 0..10 {
            assert_eq!(d.poll(Some(MeetingApp::Zoom), false), DetectEvent::None);
        }
        assert!(d.in_meeting());
    }

    #[test]
    fn debounce_absent_streak_resets_if_window_reappears() {
        let mut d = Debouncer::new();
        d.poll(Some(MeetingApp::Zoom), true);
        d.poll(Some(MeetingApp::Zoom), true); // Started
        // Window blips away for two polls (e.g. minimized) then returns.
        assert_eq!(d.poll(None, true), DetectEvent::None);
        assert_eq!(d.poll(None, true), DetectEvent::None);
        assert_eq!(d.poll(Some(MeetingApp::Zoom), true), DetectEvent::None); // resets
        // Now it takes three fresh absent polls to end.
        assert_eq!(d.poll(None, false), DetectEvent::None);
        assert_eq!(d.poll(None, false), DetectEvent::None);
        assert!(d.in_meeting());
        assert_eq!(d.poll(None, false), DetectEvent::Ended);
    }

    #[test]
    fn debounce_full_lifecycle_then_rearms() {
        let mut d = Debouncer::new();
        // Meeting 1.
        assert_eq!(d.poll(Some(MeetingApp::GoogleMeet), true), DetectEvent::None);
        assert_eq!(d.poll(Some(MeetingApp::GoogleMeet), true), DetectEvent::Started(MeetingApp::GoogleMeet));
        assert_eq!(d.poll(None, false), DetectEvent::None);
        assert_eq!(d.poll(None, false), DetectEvent::None);
        assert_eq!(d.poll(None, false), DetectEvent::Ended);
        // Meeting 2 (re-arm): a fresh pair of positives starts a new one.
        assert_eq!(d.poll(Some(MeetingApp::Teams), true), DetectEvent::None);
        assert_eq!(d.poll(Some(MeetingApp::Teams), true), DetectEvent::Started(MeetingApp::Teams));
    }

    #[test]
    fn debounce_started_fires_once_per_meeting() {
        let mut d = Debouncer::new();
        d.poll(Some(MeetingApp::Zoom), true);
        assert_eq!(d.poll(Some(MeetingApp::Zoom), true), DetectEvent::Started(MeetingApp::Zoom));
        // Continuing to poll positive must not re-fire Started.
        for _ in 0..5 {
            assert_eq!(d.poll(Some(MeetingApp::Zoom), true), DetectEvent::None);
        }
    }

    #[test]
    fn normalize_dashes_maps_all_variants() {
        assert_eq!(normalize_dashes("a\u{2013}b\u{2014}c\u{2011}d-e"), "a-b-c-d-e");
    }
}
