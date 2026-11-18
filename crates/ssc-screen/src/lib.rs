//! Screen reader + diff (Build Sheet §6 W1-SCREEN).
//!
//! - [`walk`]: platform-independent accessibility tree walk (node / depth / 50 ms caps,
//!   password fields never read).
//! - [`diff`]: line-level [`ScreenDiff`](ssc_core::types::ScreenDiff), newest first, 2 KB cap.
//! - [`ocr`]: change detection and OCR merging on changed regions only.
//! - [`reader`]: [`ScreenReader`], the [`ScreenSource`] that ties them together.
//! - Platform backends: `windows_backend` (UI Automation, GDI capture, Windows.Media.Ocr) and `macos`
//!   (AXUIElement, ScreenCaptureKit capture, Apple Vision). [`platform_reader`] picks the right one.
//!
//! Screen text is untrusted data (Build Sheet §3.8): this crate only reads it.

pub mod diff;
pub mod grab;
pub mod ocr;
pub mod reader;
pub mod walk;

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "windows")]
pub mod windows_backend;

pub use diff::diff;
pub use grab::ScreenGrab;
pub use reader::{AxBackend, OCR_MIN_CHARS, ScreenDiffer, ScreenReader, WindowRead};
pub use walk::{AxNode, WalkLimits, WalkOutput};

use ssc_core::Result;
use ssc_core::traits::ScreenSource;

/// The screen reader for this OS: accessibility first, OCR fallback on changed regions.
pub fn platform_reader() -> Result<ScreenReader> {
    #[cfg(target_os = "macos")]
    {
        Ok(ScreenReader::new(Box::new(macos::AxReader::new()))
            .with_ocr(Box::new(macos::SckCapture), Box::new(macos::VisionOcr))
            .with_color(Box::new(macos::SckCapture)))
    }
    #[cfg(target_os = "windows")]
    {
        Ok(ScreenReader::new(Box::new(windows_backend::UiaReader))
            .with_ocr(Box::new(windows_backend::GdiCapture), Box::new(windows_backend::WinOcr)))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Err(ssc_core::SscError::Unavailable("screen reading is only supported on Windows and macOS".into()))
    }
}

/// [`platform_reader`] as a boxed trait object.
pub fn platform_source() -> Result<Box<dyn ScreenSource>> {
    Ok(Box::new(platform_reader()?))
}
