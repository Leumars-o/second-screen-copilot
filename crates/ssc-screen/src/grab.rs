//! CR-20 screen hotkey: one capture of the focused window, as a JPEG for a vision model plus
//! whatever text could be read from it.

use crate::ocr::RgbFrame;
pub use ssc_core::types::ScreenGrab;
use ssc_core::types::{PromptImage, Rect, ScreenSnapshot};
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

/// Grab from a captured frame: the full JPEG for the model plus the phone thumbnail.
pub(crate) fn grab_from_frame(snap: ScreenSnapshot, f: &RgbFrame, window: Rect) -> ssc_core::Result<ScreenGrab> {
    let jpeg = encode_jpeg(f, JPEG_QUALITY)?;
    let mut g = make_grab(snap, Some((f.width, f.height, jpeg)), None);
    g.thumb = thumbnail_jpeg(f);
    g.window_size = Some((window.w, window.h));
    Ok(g)
}

/// Thumbnail limits: one relay frame is 64 KB, and the thumbnail rides base64 in `answer.start`.
pub const THUMB_MAX_EDGE: u32 = 480;
pub const THUMB_MAX_BYTES: usize = 30_000;

/// Area-average downscale (deterministic box filter) so the longer side is at most `max_edge`.
pub fn downscale(f: &RgbFrame, max_edge: u32) -> RgbFrame {
    let (w, h) = fit_within(f.width, f.height, max_edge);
    if (w, h) == (f.width, f.height) {
        return f.clone();
    }
    let (sw, sh) = (f.width as usize, f.height as usize);
    let mut out = vec![0u8; w as usize * h as usize * 3];
    for ty in 0..h as usize {
        let y0 = ty * sh / h as usize;
        let y1 = ((ty + 1) * sh / h as usize).max(y0 + 1).min(sh);
        for tx in 0..w as usize {
            let x0 = tx * sw / w as usize;
            let x1 = ((tx + 1) * sw / w as usize).max(x0 + 1).min(sw);
            let mut acc = [0u32; 3];
            for y in y0..y1 {
                let row = &f.rgb[(y * sw + x0) * 3..(y * sw + x1) * 3];
                for px in row.chunks_exact(3) {
                    acc[0] += u32::from(px[0]);
                    acc[1] += u32::from(px[1]);
                    acc[2] += u32::from(px[2]);
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as u32;
            let o = (ty * w as usize + tx) * 3;
            for c in 0..3 {
                out[o + c] = (acc[c] / n) as u8;
            }
        }
    }
    RgbFrame { width: w, height: h, rgb: out }
}

/// A JPEG thumbnail under [`THUMB_MAX_BYTES`], trying lower quality, then a smaller size.
pub fn thumbnail_jpeg(f: &RgbFrame) -> Option<Vec<u8>> {
    for (edge, q) in [(THUMB_MAX_EDGE, 60), (THUMB_MAX_EDGE, 40), (360, 40), (240, 35)] {
        let j = encode_jpeg(&downscale(f, edge), q).ok()?;
        if j.len() <= THUMB_MAX_BYTES {
            return Some(j);
        }
    }
    None
}

/// One on-screen window from the window server (front to back order), for [`pick_front_window`].
#[derive(Debug, Clone, PartialEq)]
pub struct WinInfo {
    pub app: String,
    pub title: String,
    pub pid: i64,
    pub layer: i64,
    pub alpha: f64,
    pub rect: Rect,
}

/// Smallest window treated as an app's real window, points. Browser hover cards, bubbles and
/// tooltips are normal-layer windows on top of the page but smaller than this (CR-22c).
pub const MIN_MAIN_WINDOW: (i32, i32) = (300, 200);

/// The window the user is looking at: the front app is the owner of the frontmost normal window;
/// its frontmost window of at least [`MIN_MAIN_WINDOW`] is the one to capture (not a helper
/// bubble that happens to be first). Skips our own process, invisible windows, and system UI.
pub fn pick_front_window(wins: &[WinInfo], me: i64) -> Option<(String, String, Rect)> {
    let candidates: Vec<&WinInfo> = wins
        .iter()
        .filter(|w| {
            w.layer == 0
                && w.alpha > 0.0
                && w.pid != me
                && w.rect.w >= 100
                && w.rect.h >= 60
                && crate::reader::is_grab_target(&w.app)
        })
        .collect();
    let front = *candidates.first()?;
    let main = candidates
        .iter()
        .find(|w| w.pid == front.pid && w.rect.w >= MIN_MAIN_WINDOW.0 && w.rect.h >= MIN_MAIN_WINDOW.1)
        .copied()
        .unwrap_or(front);
    Some((main.app.clone(), main.title.clone(), main.rect))
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

    fn win(app: &str, pid: i64, w: i32, h: i32) -> WinInfo {
        WinInfo { app: app.into(), title: String::new(), pid, layer: 0, alpha: 1.0, rect: Rect { x: 0, y: 0, w, h } }
    }

    #[test]
    fn front_window_is_the_front_apps_main_window_not_a_helper_bubble() {
        // The lead's quiz: a Chrome hover card sits first in front-to-back order.
        let wins = vec![
            win("Google Chrome", 7, 260, 90),    // hover card over the tab strip
            win("Google Chrome", 7, 1840, 1300), // the page
            win("Code", 3, 1440, 872),
        ];
        let (app, _, r) = pick_front_window(&wins, 99).unwrap();
        assert_eq!((app.as_str(), r.w, r.h), ("Google Chrome", 1840, 1300));
        // A big window from another app behind is never chosen over the front app's own.
        let wins = vec![win("Safari", 5, 1200, 800), win("Code", 3, 1440, 872)];
        assert_eq!(pick_front_window(&wins, 99).unwrap().0, "Safari");
        // Only a small window: use it rather than nothing.
        assert_eq!(pick_front_window(&[win("Calculator", 4, 230, 380)], 99).unwrap().2.w, 230);
        // Our own window, system UI, invisible and non-normal layers are skipped.
        let mut hidden = win("Safari", 5, 1200, 800);
        hidden.alpha = 0.0;
        let mut menu = win("Safari", 5, 1200, 800);
        menu.layer = 25;
        let wins = vec![
            win("ssc-agent", 99, 900, 760),
            win("UserNotificationCenter", 2, 400, 120),
            hidden,
            menu,
            win("Code", 3, 1440, 872),
        ];
        assert_eq!(pick_front_window(&wins, 99).unwrap().0, "Code");
    }

    #[test]
    fn downscale_averages_and_thumbnail_fits_a_relay_frame() {
        // 4x2: left half black, right half white -> 2x1: black, white.
        let mut rgb = vec![0u8; 4 * 2 * 3];
        for y in 0..2 {
            for x in 2..4 {
                rgb[(y * 4 + x) * 3..(y * 4 + x) * 3 + 3].fill(255);
            }
        }
        let d = downscale(&RgbFrame { width: 4, height: 2, rgb }, 2);
        assert_eq!((d.width, d.height, d.rgb.clone()), (2, 1, vec![0, 0, 0, 255, 255, 255]));
        // Noisy 1600x1000 frame (worst case for JPEG): still under the limit.
        let mut seed = 7u32;
        let rgb: Vec<u8> = (0..1600 * 1000 * 3)
            .map(|_| {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                (seed >> 16) as u8
            })
            .collect();
        let t = thumbnail_jpeg(&RgbFrame { width: 1600, height: 1000, rgb }).unwrap();
        assert!(t.len() <= THUMB_MAX_BYTES, "{}", t.len());
        assert_eq!(&t[..2], &[0xFF, 0xD8]);
    }

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

    /// Accessibility fails ("no focused application"), the window server still has the window.
    struct NoAx(Option<(String, String, Rect)>);
    impl AxBackend for NoAx {
        fn read_focused(&self, _: &WalkLimits) -> Result<WindowRead> {
            Err(SscError::Unavailable("no focused application (AXError -25204: cannot complete)".into()))
        }
        fn front_window(&self) -> Option<(String, String, Rect)> {
            self.0.clone()
        }
    }

    #[test]
    fn grab_uses_the_window_server_when_accessibility_fails() {
        let front = Some(("WPS Office".to_string(), "Notes.docx".to_string(), WIN.unwrap()));
        let color = Color(Mutex::new(vec![]), true);
        let r = ScreenReader::new(Box::new(NoAx(front))).with_color(Box::new(color));
        let g = r.grab_window(true).unwrap();
        assert!(g.image.is_some(), "image still captured");
        assert_eq!(g.label(), "WPS Office — Notes.docx");
        assert!(g.text.is_empty());

        // Neither source: a clear error naming the AX failure.
        let r = ScreenReader::new(Box::new(NoAx(None))).with_color(Box::new(Color(Mutex::new(vec![]), true)));
        let e = r.grab_window(true).unwrap_err().to_string();
        assert!(e.contains("-25204") && e.contains("window server"), "{e}");
    }

    #[test]
    fn a_focused_system_alert_or_our_own_window_is_skipped_for_the_front_window() {
        struct Alert;
        impl AxBackend for Alert {
            fn read_focused(&self, l: &WalkLimits) -> Result<WindowRead> {
                Ok(WindowRead {
                    app: "UserNotificationCenter".into(),
                    window_title: String::new(),
                    bounds: Some(Rect { x: 0, y: 0, w: 260, h: 364 }),
                    walk: walk(FakeNode::root(&FakeTree::fixture(1)), l),
                })
            }
            fn front_window(&self) -> Option<(String, String, Rect)> {
                Some(("WPS Office".into(), "Notes.docx".into(), WIN.unwrap()))
            }
        }
        let calls = std::sync::Arc::new(Mutex::new(vec![]));
        struct Rec(std::sync::Arc<Mutex<Vec<(Rect, u32)>>>);
        impl ColorSource for Rec {
            fn capture_color(&self, r: Rect, m: u32) -> Result<RgbFrame> {
                self.0.lock().unwrap().push((r, m));
                Ok(RgbFrame { width: 8, height: 4, rgb: vec![0; 96] })
            }
        }
        let r = ScreenReader::new(Box::new(Alert)).with_color(Box::new(Rec(calls.clone())));
        let g = r.grab_window(true).unwrap();
        assert_eq!(g.label(), "WPS Office — Notes.docx");
        assert_eq!(calls.lock().unwrap()[0].0, WIN.unwrap(), "captured the real window, not the alert");
        assert!(!crate::reader::NOT_A_GRAB_TARGET.contains(&"WPS Office"));
    }

    /// Accessibility that takes `delay` and reports `app`; the window server says WPS is in front.
    struct SlowAx(std::time::Duration, &'static str);
    impl AxBackend for SlowAx {
        fn read_focused(&self, l: &WalkLimits) -> Result<WindowRead> {
            std::thread::sleep(self.0);
            Ok(WindowRead {
                app: self.1.into(),
                window_title: "Notes.docx".into(),
                bounds: WIN,
                walk: walk(FakeNode::root(&FakeTree::fixture(2)), l),
            })
        }
        fn front_window(&self) -> Option<(String, String, Rect)> {
            Some(("WPS Office".into(), String::new(), WIN.unwrap()))
        }
    }

    #[test]
    fn fast_grab_never_waits_for_slow_accessibility() {
        let r = ScreenReader::new(Box::new(SlowAx(std::time::Duration::from_secs(2), "WPS Office")))
            .with_color(Box::new(Color(Mutex::new(vec![]), true)));
        let t = std::time::Instant::now();
        let g = r.grab_window(true).unwrap();
        assert!(t.elapsed() < std::time::Duration::from_millis(500), "waited {:?}", t.elapsed());
        assert!(g.image.is_some());
        assert!(g.text.is_empty(), "text arrived too late: left out");
        assert_eq!(g.label(), "WPS Office");
    }

    #[test]
    fn fast_grab_includes_text_that_is_ready_and_from_the_same_app() {
        let r = ScreenReader::new(Box::new(SlowAx(std::time::Duration::ZERO, "WPS Office")))
            .with_color(Box::new(Color(Mutex::new(vec![]), true)));
        let g = r.grab_window(true).unwrap();
        assert!(g.image.is_some() && !g.text.is_empty());
        assert_eq!(g.label(), "WPS Office — Notes.docx", "title from AX when the window server has none");
        // Focus on an alert: its text is not the front window's.
        let r = ScreenReader::new(Box::new(SlowAx(std::time::Duration::ZERO, "UserNotificationCenter")))
            .with_color(Box::new(Color(Mutex::new(vec![]), true)));
        let g = r.grab_window(true).unwrap();
        assert!(g.image.is_some() && g.text.is_empty());
    }

    #[test]
    fn grab_walks_with_a_longer_budget_than_the_poll_loop() {
        struct Budget(Mutex<Vec<std::time::Duration>>);
        impl AxBackend for Budget {
            fn read_focused(&self, l: &WalkLimits) -> Result<WindowRead> {
                self.0.lock().unwrap().push(l.budget);
                Ok(WindowRead { bounds: WIN, ..Default::default() })
            }
        }
        let b = std::sync::Arc::new(Budget(Mutex::new(vec![])));
        struct Shared(std::sync::Arc<Budget>);
        impl AxBackend for Shared {
            fn read_focused(&self, l: &WalkLimits) -> Result<WindowRead> {
                self.0.read_focused(l)
            }
        }
        let r = ScreenReader::new(Box::new(Shared(b.clone())));
        r.grab_window(false).unwrap();
        assert_eq!(b.0.lock().unwrap()[0], crate::reader::GRAB_WALK_BUDGET);
    }

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
        println!("front window (window server): {:?}", crate::macos::front_window().map(|(a, t, r)| (a, t.len(), r)));
        let t = std::time::Instant::now();
        let ax = crate::macos::AxReader::new();
        let res = crate::AxBackend::read_focused(&ax, &crate::WalkLimits::default());
        println!(
            "ax read {} ms: {:?}",
            t.elapsed().as_millis(),
            res.as_ref().map(|r| r.app.clone()).map_err(ToString::to_string)
        );
        let t = std::time::Instant::now();
        let fw = crate::macos::front_window();
        println!("window server {} ms", t.elapsed().as_millis());
        if let Some((_, _, b)) = fw {
            for i in 0..2 {
                let t = std::time::Instant::now();
                let f =
                    crate::ocr::ColorSource::capture_color(&crate::macos::SckCapture, b, super::GRAB_MAX_EDGE).unwrap();
                let c = t.elapsed().as_millis();
                let t = std::time::Instant::now();
                let j = super::encode_jpeg(&f, super::JPEG_QUALITY).unwrap();
                println!("capture #{i} {c} ms, jpeg {} ms ({} bytes)", t.elapsed().as_millis(), j.len());
            }
        }
        let r = crate::platform_reader().unwrap();
        let t = std::time::Instant::now();
        let g = r.grab_window(true).unwrap();
        let ms = t.elapsed().as_millis();
        println!(
            "grab {ms} ms: app={:?} image={:?} jpeg_bytes={} thumb_bytes={} window={:?} text_chars={} error={:?}",
            g.app,
            g.image_size,
            g.image.as_ref().map_or(0, |i| i.data.len()),
            g.thumb.as_ref().map_or(0, Vec::len),
            g.window_size,
            g.text.chars().count(),
            g.image_error
        );
        if let Some((w, h)) = g.image_size {
            assert!(w.max(h) <= super::GRAB_MAX_EDGE);
        }
    }
}
