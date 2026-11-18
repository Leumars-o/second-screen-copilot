//! [`ScreenReader`]: the [`ScreenSource`] implementation. Accessibility first; OCR on changed
//! regions only when the accessibility text is empty or under [`OCR_MIN_CHARS`] characters.

use crate::ocr::{ColorSource, FrameSource, OcrCache, OcrEngine, lines_to_nodes};
use crate::walk::{WalkLimits, WalkOutput};
use ssc_core::Result;
use ssc_core::traits::{Clock, ScreenSource, SystemClock};
use ssc_core::types::{Rect, ScreenDiff, ScreenSnapshot, ScreenTextSource};
use std::sync::{Arc, Mutex};

/// Below this many non-whitespace characters of accessibility text, OCR is tried.
pub const OCR_MIN_CHARS: usize = 20;

/// The focused window as read by an accessibility backend.
#[derive(Debug, Clone, Default)]
pub struct WindowRead {
    pub app: String,
    pub window_title: String,
    /// Window bounds on screen, used for the OCR capture.
    pub bounds: Option<Rect>,
    pub walk: WalkOutput,
}

/// Platform accessibility backend (UI Automation, AXUIElement, or a fake in tests).
pub trait AxBackend: Send + Sync {
    fn read_focused(&self, limits: &WalkLimits) -> Result<WindowRead>;
}

struct Ocr {
    frames: Box<dyn FrameSource>,
    engine: Box<dyn OcrEngine>,
    /// (app, window title) the cache belongs to, plus the cache.
    state: Mutex<(String, String, OcrCache)>,
}

pub struct ScreenReader {
    ax: Box<dyn AxBackend>,
    ocr: Option<Ocr>,
    color: Option<Box<dyn ColorSource>>,
    limits: WalkLimits,
    clock: Arc<dyn Clock>,
}

impl ScreenReader {
    pub fn new(ax: Box<dyn AxBackend>) -> Self {
        Self { ax, ocr: None, color: None, limits: WalkLimits::default(), clock: Arc::new(SystemClock) }
    }
    /// Enables colour capture for [`Self::grab`] (CR-20).
    pub fn with_color(mut self, color: Box<dyn ColorSource>) -> Self {
        self.color = Some(color);
        self
    }
    /// Enables the OCR fallback.
    pub fn with_ocr(mut self, frames: Box<dyn FrameSource>, engine: Box<dyn OcrEngine>) -> Self {
        self.ocr = Some(Ocr { frames, engine, state: Mutex::new(Default::default()) });
        self
    }
    pub fn with_limits(mut self, limits: WalkLimits) -> Self {
        self.limits = limits;
        self
    }
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    fn try_ocr(&self, ocr: &Ocr, read: &WindowRead) -> Result<Option<ScreenSnapshot>> {
        let Some(bounds) = read.bounds.filter(|b| b.w > 0 && b.h > 0) else { return Ok(None) };
        let frame = ocr.frames.capture(bounds)?;
        let mut state = ocr.state.lock().unwrap_or_else(|e| e.into_inner());
        let (app, title, cache) = &mut *state;
        if *app != read.app || *title != read.window_title {
            *cache = OcrCache::default();
            app.clone_from(&read.app);
            title.clone_from(&read.window_title);
        }
        cache.update(frame, ocr.engine.as_ref())?;
        let frame = cache.frame.as_ref().expect("set by update");
        let nodes = lines_to_nodes(&cache.lines, frame, &read.walk.password_regions);
        Ok(Some(ScreenSnapshot {
            app: read.app.clone(),
            window_title: read.window_title.clone(),
            nodes,
            source: ScreenTextSource::Ocr,
            captured_at_ms: self.clock.now_ms(),
        }))
    }
}

impl ScreenReader {
    /// CR-20 screen hotkey: the focused window right now. With `image`, a JPEG of the window
    /// (at most [`crate::grab::GRAB_MAX_EDGE`] px) plus its accessibility text, no OCR (the
    /// model reads the image). Without `image`, or if the capture fails, the text with the
    /// usual OCR fallback.
    pub fn grab_window(&self, image: bool) -> Result<crate::grab::ScreenGrab> {
        let read = self.ax.read_focused(&self.limits)?;
        let bounds = read.bounds.filter(|b| b.w > 0 && b.h > 0);
        let mut capture_error = None;
        if image {
            match (&self.color, bounds) {
                (Some(c), Some(b)) => match c.capture_color(b, crate::grab::GRAB_MAX_EDGE) {
                    Ok(frame) => {
                        let jpeg = crate::grab::encode_jpeg(&frame, crate::grab::JPEG_QUALITY)?;
                        let snap = ScreenSnapshot {
                            app: read.app,
                            window_title: read.window_title,
                            nodes: read.walk.nodes,
                            source: ScreenTextSource::Accessibility,
                            captured_at_ms: self.clock.now_ms(),
                        };
                        return Ok(crate::grab::make_grab(snap, Some((frame.width, frame.height, jpeg)), None));
                    }
                    Err(e) => capture_error = Some(e.to_string()),
                },
                (None, _) => capture_error = Some("no colour capture on this platform".into()),
                (_, None) => capture_error = Some("focused window has no bounds".into()),
            }
        }
        let snap = self.snapshot_from(read);
        Ok(crate::grab::make_grab(snap, None, capture_error))
    }

    fn snapshot_from(&self, read: WindowRead) -> ScreenSnapshot {
        let chars: usize = read.walk.nodes.iter().map(|n| n.text.chars().filter(|c| !c.is_whitespace()).count()).sum();
        if chars < OCR_MIN_CHARS
            && let Some(ocr) = &self.ocr
            // OCR failing (no Screen Recording permission, engine missing) is not fatal: the
            // accessibility text, however short, is still returned.
            && let Ok(Some(snap)) = self.try_ocr(ocr, &read)
        {
            return snap;
        }
        ScreenSnapshot {
            app: read.app,
            window_title: read.window_title,
            nodes: read.walk.nodes,
            source: ScreenTextSource::Accessibility,
            captured_at_ms: self.clock.now_ms(),
        }
    }
}

impl ScreenSource for ScreenReader {
    fn snapshot(&self) -> Result<ScreenSnapshot> {
        let read = self.ax.read_focused(&self.limits)?;
        Ok(self.snapshot_from(read))
    }
    fn grab(&self, image: bool) -> Result<crate::grab::ScreenGrab> {
        self.grab_window(image)
    }
}

/// Keeps the previous snapshot and diffs each new one against it.
#[derive(Debug, Default)]
pub struct ScreenDiffer {
    prev: Option<ScreenSnapshot>,
}

impl ScreenDiffer {
    pub fn update(&mut self, next: ScreenSnapshot) -> ScreenDiff {
        let d = crate::diff::diff(self.prev.as_ref(), &next);
        self.prev = Some(next);
        d
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ocr::fake::{FakeOcr, blank, paint};
    use crate::ocr::{Frame, OcrLine};
    use crate::walk::fake::{FakeNode, FakeTree};
    use crate::walk::walk;
    use ssc_core::SscError;
    use ssc_core::mock::MockClock;
    use std::time::{Duration, Instant};

    /// Walks a fake tree (FakeNode is !Send, so it is built per call).
    struct FakeAx(FakeTree, Option<Rect>);
    impl AxBackend for FakeAx {
        fn read_focused(&self, limits: &WalkLimits) -> Result<WindowRead> {
            Ok(WindowRead {
                app: "Zoom".into(),
                window_title: "Meeting".into(),
                bounds: self.1,
                walk: walk(FakeNode::root(&self.0), limits),
            })
        }
    }

    struct FixedFrames(Mutex<Vec<Frame>>);
    impl FrameSource for FixedFrames {
        fn capture(&self, _: Rect) -> Result<Frame> {
            let mut v = self.0.lock().unwrap();
            if v.len() > 1 { Ok(v.remove(0)) } else { Ok(v[0].clone()) }
        }
    }
    struct DeniedFrames;
    impl FrameSource for DeniedFrames {
        fn capture(&self, _: Rect) -> Result<Frame> {
            Err(SscError::Unavailable("screen recording permission".into()))
        }
    }
    struct ArcOcr(Arc<FakeOcr>);
    impl OcrEngine for ArcOcr {
        fn recognize(&self, g: &[u8], w: u32, h: u32) -> Result<Vec<OcrLine>> {
            self.0.recognize(g, w, h)
        }
    }

    const WIN: Option<Rect> = Some(Rect { x: 0, y: 0, w: 32, h: 320 });

    fn ocr_frame(v: u8) -> Frame {
        let mut f = blank(64, 640);
        paint(&mut f, 96, 112, v);
        f
    }

    #[test]
    fn accessibility_text_is_used_when_long_enough() {
        let ocr = Arc::new(FakeOcr::default());
        let r = ScreenReader::new(Box::new(FakeAx(FakeTree::fixture(3), WIN)))
            .with_ocr(Box::new(FixedFrames(Mutex::new(vec![ocr_frame(50)]))), Box::new(ArcOcr(ocr.clone())))
            .with_clock(Arc::new(MockClock::new(42)));
        let s = r.snapshot().unwrap();
        assert_eq!(s.source, ScreenTextSource::Accessibility);
        assert_eq!(s.captured_at_ms, 42);
        assert!(s.text().contains("Message 2: what is the price for 2 seats?"));
        assert!(!s.text().contains("hunter2"));
        assert!(ocr.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn short_accessibility_text_falls_back_to_ocr_on_changed_regions() {
        let ocr = Arc::new(FakeOcr::default());
        let frames = FixedFrames(Mutex::new(vec![ocr_frame(50), ocr_frame(150)]));
        let tree = FakeTree::new("Window", "Canvas");
        let r =
            ScreenReader::new(Box::new(FakeAx(tree, WIN))).with_ocr(Box::new(frames), Box::new(ArcOcr(ocr.clone())));
        let s1 = r.snapshot().unwrap();
        assert_eq!(s1.source, ScreenTextSource::Ocr);
        assert_eq!(s1.text(), "text50");
        assert_eq!(s1.nodes[0].bounds, Some(Rect { x: 100, y: 98, w: 32, h: 8 }));
        let s2 = r.snapshot().unwrap();
        assert_eq!(s2.text(), "text150");
        let calls = ocr.calls.lock().unwrap().clone();
        assert_eq!(calls[0], (64, 640), "first OCR is the whole window");
        assert!(calls[1].1 < 640, "second OCR only covers the changed band");
        let s3 = r.snapshot().unwrap();
        assert_eq!(s3.text(), "text150");
        assert_eq!(ocr.calls.lock().unwrap().len(), 2, "unchanged frame: no OCR");
    }

    #[test]
    fn ocr_failure_falls_back_to_accessibility() {
        let r = ScreenReader::new(Box::new(FakeAx(FakeTree::new("Window", "Canvas"), WIN)))
            .with_ocr(Box::new(DeniedFrames), Box::new(FakeOcr::default()));
        let s = r.snapshot().unwrap();
        assert_eq!(s.source, ScreenTextSource::Accessibility);
        assert_eq!(s.text(), "Canvas");
    }

    #[test]
    fn ocr_lines_over_password_fields_are_dropped() {
        // The password field covers screen rows 98..106, where the fake OCR "sees" text.
        let tree = FakeTree::new("Window", "Sign in").with(vec![FakeTree {
            bounds: Some(Rect { x: 100, y: 98, w: 32, h: 8 }),
            ..FakeTree::password("hunter2")
        }]);
        let r = ScreenReader::new(Box::new(FakeAx(tree, WIN)))
            .with_ocr(Box::new(FixedFrames(Mutex::new(vec![ocr_frame(50)]))), Box::new(FakeOcr::default()));
        let s = r.snapshot().unwrap();
        assert!(s.nodes.is_empty(), "{:?}", s.nodes);
    }

    #[test]
    fn differ_tracks_previous_snapshot() {
        use ssc_core::mock::MockScreen;
        let mut d = ScreenDiffer::default();
        assert_eq!(d.update(MockScreen::snapshot("Zoom", &["a"])).added_lines, vec!["a"]);
        assert!(d.update(MockScreen::snapshot("Zoom", &["a"])).is_empty());
        assert_eq!(d.update(MockScreen::snapshot("Zoom", &["a", "b"])).added_lines, vec!["b"]);
    }

    /// Acceptance: snapshot p95 under 50 ms on the fixture tree (~1,000 messages, ~4,000 nodes,
    /// so the 2,000-node cap is exercised too).
    #[test]
    fn bench_snapshot_p95_under_50ms() {
        let r = ScreenReader::new(Box::new(FakeAx(FakeTree::fixture(1000), WIN)));
        let mut times: Vec<Duration> = (0..100)
            .map(|_| {
                let t = Instant::now();
                let s = r.snapshot().unwrap();
                let took = t.elapsed();
                assert!(!s.nodes.is_empty());
                took
            })
            .collect();
        times.sort();
        let p95 = times[94];
        eprintln!("snapshot p50={:?} p95={:?} max={:?}", times[49], p95, times[99]);
        assert!(p95 < Duration::from_millis(50), "p95 {p95:?}");
    }
}
