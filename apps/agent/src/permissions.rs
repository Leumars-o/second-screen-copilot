//! macOS privacy permissions for the first-run checklist: Screen Recording (screen + system
//! audio through ScreenCaptureKit), Accessibility (reading the focused window) and Microphone.
//!
//! Status checks use the documented non-prompting APIs. Requesting shows the system prompt where
//! macOS offers one (Screen Recording, Microphone), otherwise opens the settings pane.
//!
//! The Microphone pane has no "+" button: an app only appears there after it has called
//! `requestAccessForMediaType:`. When the agent runs unbundled from a terminal, macOS charges the
//! request to the terminal's app (Terminal, VS Code, ...), so that is the entry that appears.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Granted,
    Missing,
    /// Not checkable on this OS.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
pub struct Item {
    pub id: &'static str,
    pub name: &'static str,
    pub why: &'static str,
    pub status: Status,
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)] // Two argument-less, side-effect-free C status functions.
mod ffi {
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXIsProcessTrusted() -> bool;
    }
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGPreflightScreenCaptureAccess() -> bool;
        fn CGRequestScreenCaptureAccess() -> bool;
    }
    pub fn accessibility() -> bool {
        // SAFETY: no arguments, returns a plain bool.
        unsafe { AXIsProcessTrusted() }
    }
    pub fn screen_recording() -> bool {
        // SAFETY: no arguments, returns a plain bool; does not prompt.
        unsafe { CGPreflightScreenCaptureAccess() }
    }
    pub fn request_screen_recording() -> bool {
        // SAFETY: no arguments; shows the system prompt at most once per app.
        unsafe { CGRequestScreenCaptureAccess() }
    }

    use objc2::msg_send;
    use objc2::runtime::{AnyClass, AnyObject, Bool};

    #[link(name = "AVFoundation", kind = "framework")]
    unsafe extern "C" {
        /// `AVMediaType` (an immutable `NSString *` constant).
        static AVMediaTypeAudio: *const AnyObject;
    }

    /// `AVAuthorizationStatus`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Mic {
        NotDetermined,
        Denied,
        Authorized,
    }

    fn capture_device() -> Option<&'static AnyClass> {
        AnyClass::get(c"AVCaptureDevice")
    }

    /// Does not prompt.
    pub fn microphone() -> Mic {
        let Some(cls) = capture_device() else { return Mic::Denied };
        // SAFETY: documented class method taking an AVMediaType, returning NSInteger.
        let s: isize = unsafe { msg_send![cls, authorizationStatusForMediaType: AVMediaTypeAudio] };
        match s {
            0 => Mic::NotDetermined,
            3 => Mic::Authorized,
            _ => Mic::Denied, // 1 restricted, 2 denied
        }
    }

    /// Shows the system prompt if the user has not decided yet (and registers the app in the
    /// Microphone pane). Returns at once; the answer arrives on a background queue.
    pub fn request_microphone() {
        let Some(cls) = capture_device() else { return };
        let done = block2::RcBlock::new(|_granted: Bool| {});
        // SAFETY: documented class method; the block is copied by AVFoundation.
        let _: () = unsafe {
            msg_send![cls, requestAccessForMediaType: AVMediaTypeAudio, completionHandler: &*done]
        };
    }
}

fn status(b: bool) -> Status {
    if b { Status::Granted } else { Status::Missing }
}

pub fn checklist() -> Vec<Item> {
    #[cfg(target_os = "macos")]
    let (screen, ax, mic) = (
        status(ffi::screen_recording()),
        status(ffi::accessibility()),
        status(ffi::microphone() == ffi::Mic::Authorized),
    );
    #[cfg(not(target_os = "macos"))]
    let (screen, ax, mic) = (Status::Unknown, Status::Unknown, Status::Unknown);
    vec![
        Item {
            id: "screen",
            name: "Screen Recording",
            why: "Hear the other side of the call (system audio) and read the screen when text isn't accessible.",
            status: screen,
        },
        Item { id: "accessibility", name: "Accessibility", why: "Read text from the focused window.", status: ax },
        Item { id: "microphone", name: "Microphone", why: "Transcribe what you say.", status: mic },
    ]
}

/// System Settings deep link for a checklist item.
pub fn settings_url(id: &str) -> Option<&'static str> {
    Some(match id {
        "screen" => "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture",
        "accessibility" => "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility",
        "microphone" => "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone",
        _ => return None,
    })
}

/// Ask for a permission: the system prompt where macOS offers one, then the settings pane.
pub fn request(id: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        if id == "screen" && ffi::request_screen_recording() {
            return Ok(());
        }
        // Undecided: the system prompt is the only way to get listed in the Microphone pane.
        if id == "microphone" && ffi::microphone() == ffi::Mic::NotDetermined {
            ffi::request_microphone();
            return Ok(());
        }
    }
    let url = settings_url(id).ok_or_else(|| format!("unknown permission {id}"))?;
    std::process::Command::new("/usr/bin/open").arg(url).status().map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checklist_has_the_three_permissions_with_links() {
        let c = checklist();
        let ids: Vec<&str> = c.iter().map(|i| i.id).collect();
        assert_eq!(ids, ["screen", "accessibility", "microphone"]);
        assert!(ids.iter().all(|id| settings_url(id).is_some()));
        assert!(settings_url("nope").is_none());
    }
}
