//! macOS privacy permissions for the first-run checklist: Screen Recording (screen + system
//! audio through ScreenCaptureKit), Accessibility (reading the focused window) and Microphone.
//!
//! Status checks use the documented non-prompting APIs where they exist; the microphone status
//! needs AVFoundation, so the checklist shows it as "check" and links to the settings pane.
//! Granting always happens in System Settings (OS dialogs need a human click, Build Sheet §9).

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Granted,
    Missing,
    /// Not checkable without prompting; the user confirms in System Settings.
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
}

fn status(b: bool) -> Status {
    if b { Status::Granted } else { Status::Missing }
}

pub fn checklist() -> Vec<Item> {
    #[cfg(target_os = "macos")]
    let (screen, ax) = (status(ffi::screen_recording()), status(ffi::accessibility()));
    #[cfg(not(target_os = "macos"))]
    let (screen, ax) = (Status::Unknown, Status::Unknown);
    vec![
        Item {
            id: "screen",
            name: "Screen Recording",
            why: "Hear the other side of the call (system audio) and read the screen when text isn't accessible.",
            status: screen,
        },
        Item { id: "accessibility", name: "Accessibility", why: "Read text from the focused window.", status: ax },
        Item { id: "microphone", name: "Microphone", why: "Transcribe what you say.", status: Status::Unknown },
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
    if id == "screen" && ffi::request_screen_recording() {
        return Ok(());
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
