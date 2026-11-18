//! CR-20 screen hotkey: one capture of the focused window, as a JPEG for a vision model plus
//! whatever text could be read from it.

use crate::ocr::RgbFrame;
pub use ssc_core::types::ScreenGrab;
use ssc_core::types::{PromptImage, ScreenSnapshot};
use ssc_core::{Result, SscError};

/// Longest side of the image sent to the model. Enough for body text in a full window on a
/// Retina display to stay legible; keeps the request near ~1.5k image tokens.
pub const GRAB_MAX_EDGE: u32 = 1600;
pub const JPEG_QUALITY: u8 = 82;

/// Build the core [`ScreenGrab`] from a snapshot plus an optional encoded image.
pub(crate) fn make_grab(
    snap: ScreenSnapshot,
    image: Option<(u32, u32, Vec<u8>)>,
    image_error: Option<String>,
) -> ScreenGrab {
    let mut g = ScreenGrab::from_snapshot(snap, image_error);
    if let Some((w, h, data)) = image {
        g.image = Some(PromptImage { mime: "image/jpeg".into(), data });
        g.image_size = Some((w, h));
    }
    g
}

/// Target size so the longer side is at most `max_edge` (never upscaled, never 0).
pub fn fit_within(w: u32, h: u32, max_edge: u32) -> (u32, u32) {
    let long = w.max(h);
    if long <= max_edge || long == 0 {
        return (w.max(1), h.max(1));
    }
    let scale = f64::from(max_edge) / f64::from(long);
    let s = |v: u32| ((f64::from(v) * scale).round() as u32).max(1);
    (s(w), s(h))
}

pub fn encode_jpeg(f: &RgbFrame, quality: u8) -> Result<Vec<u8>> {
    let (Ok(w), Ok(h)) = (u16::try_from(f.width), u16::try_from(f.height)) else {
        return Err(SscError::Invalid(format!("image too large for JPEG: {}x{}", f.width, f.height)));
    };
    if f.rgb.len() != f.width as usize * f.height as usize * 3 {
        return Err(SscError::Invalid("RGB buffer size does not match the frame".into()));
    }
    let mut out = Vec::new();
    jpeg_encoder::Encoder::new(&mut out, quality)
        .encode(&f.rgb, w, h, jpeg_encoder::ColorType::Rgb)
        .map_err(|e| SscError::Unavailable(format!("JPEG encode failed: {e}")))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ocr::ColorSource;
    use crate::reader::{AxBackend, ScreenReader, WindowRead};
    use crate::walk::fake::{FakeNode, FakeTree};
    use crate::walk::{WalkLimits, walk};
    use ssc_core::types::Rect;
    use std::sync::Mutex;

    #[test]
    fn fit_within_keeps_aspect_and_never_upscales() {
        assert_eq!(fit_within(3200, 2000, 1600), (1600, 1000));
        assert_eq!(fit_within(1000, 4000, 1600), (400, 1600));
        assert_eq!(fit_within(800, 600, 1600), (800, 600));
        assert_eq!(fit_within(5000, 1, 1600), (1600, 1));
        assert_eq!(fit_within(0, 0, 1600), (1, 1));
    }

    #[test]
    fn jpeg_has_markers_and_rejects_bad_sizes() {
        let f = RgbFrame { width: 4, height: 2, rgb: vec![200; 4 * 2 * 3] };
        let j = encode_jpeg(&f, 80).unwrap();
        assert_eq!(&j[..2], &[0xFF, 0xD8]);
        assert_eq!(&j[j.len() - 2..], &[0xFF, 0xD9]);
        assert!(encode_jpeg(&RgbFrame { width: 4, height: 2, rgb: vec![0; 5] }, 80).is_err());
        assert!(encode_jpeg(&RgbFrame { width: 70_000, height: 1, rgb: vec![0; 210_000] }, 80).is_err());
    }

    struct FakeAx(Option<Rect>);
    impl AxBackend for FakeAx {
        fn read_focused(&self, limits: &WalkLimits) -> Result<WindowRead> {
            Ok(WindowRead {
                app: "Word".into(),
                window_title: "Quiz.docx".into(),
                bounds: self.0,
                walk: walk(FakeNode::root(&FakeTree::fixture(3)), limits),
            })
        }
    }
    struct Color(Mutex<Vec<(Rect, u32)>>, bool);
    impl ColorSource for Color {
        fn capture_color(&self, r: Rect, max_edge: u32) -> Result<RgbFrame> {
            self.0.lock().unwrap().push((r, max_edge));
            if !self.1 {
                return Err(SscError::Unavailable("screen recording permission".into()));
            }
            Ok(RgbFrame { width: 8, height: 4, rgb: vec![10; 8 * 4 * 3] })
        }
    }
    const WIN: Option<Rect> = Some(Rect { x: 10, y: 20, w: 800, h: 600 });

    #[test]
    fn grab_with_image_captures_the_focused_window_bounds() {
        let r = ScreenReader::new(Box::new(FakeAx(WIN))).with_color(Box::new(Color(Mutex::new(vec![]), true)));
        let g = r.grab_window(true).unwrap();
        let img = g.image.as_ref().expect("image");
        assert_eq!(img.mime, "image/jpeg");
        assert_eq!(&img.data[..2], &[0xFF, 0xD8]);
        assert_eq!(g.image_size, Some((8, 4)));
        assert_eq!(g.label(), "Word — Quiz.docx");
        assert!(!g.text.is_empty(), "accessibility text rides along");
        assert!(g.image_error.is_none());
    }

    #[test]
    fn grab_falls_back_to_text_when_capture_fails_or_is_not_asked() {
        let r = ScreenReader::new(Box::new(FakeAx(WIN))).with_color(Box::new(Color(Mutex::new(vec![]), false)));
        let g = r.grab_window(true).unwrap();
        assert!(g.image.is_none());
        assert!(g.image_error.as_deref().unwrap().contains("permission"));
        assert!(!g.text.is_empty());

        let r = ScreenReader::new(Box::new(FakeAx(WIN)));
        let g = r.grab_window(false).unwrap();
        assert!(g.image.is_none() && g.image_error.is_none());

        let r = ScreenReader::new(Box::new(FakeAx(None))).with_color(Box::new(Color(Mutex::new(vec![]), true)));
        assert!(r.grab_window(true).unwrap().image_error.unwrap().contains("bounds"));
    }
}

/// Manual (needs Screen Recording + Accessibility): grabs the real focused window.
/// `cargo test -p ssc-screen real_grab -- --ignored --nocapture`
#[cfg(all(test, target_os = "macos"))]
mod real {
    #[test]
    #[ignore = "captures the real screen"]
    fn real_grab() {
        let r = crate::platform_reader().unwrap();
        let t = std::time::Instant::now();
        let g = r.grab_window(true).unwrap();
        let ms = t.elapsed().as_millis();
        println!(
            "grab {ms} ms: app={:?} image={:?} jpeg_bytes={} text_chars={} error={:?}",
            g.app,
            g.image_size,
            g.image.as_ref().map_or(0, |i| i.data.len()),
            g.text.chars().count(),
            g.image_error
        );
        if let Some((w, h)) = g.image_size {
            assert!(w.max(h) <= super::GRAB_MAX_EDGE);
        }
    }
}
