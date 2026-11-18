//! OCR fallback, platform-independent parts: frames, change detection, region cropping and
//! merging OCR lines across snapshots.
//!
//! OCR runs only on regions that changed since the previous frame. A region is a full-width
//! horizontal band of changed tile rows: text lines run horizontally, so a band never cuts a
//! line in half the way a tight box can. Lines OCR'd earlier outside the changed bands are kept.

use ssc_core::Result;
use ssc_core::types::{Rect, ScreenNode};

/// A grayscale capture of the focused window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// Where the frame sits on screen, in the same units as accessibility bounds (points on
    /// macOS, physical pixels on Windows).
    pub screen: Rect,
    pub width: u32,
    pub height: u32,
    /// `width * height` luminance bytes, row-major.
    pub gray: Vec<u8>,
}

/// A pixel rectangle inside a frame or crop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PxRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl PxRect {
    pub fn intersects(&self, o: &PxRect) -> bool {
        self.x < o.x + o.w && o.x < self.x + self.w && self.y < o.y + o.h && o.y < self.y + self.h
    }
}

/// One recognised line, with bounds in the pixel space of the image given to the engine.
#[derive(Debug, Clone, PartialEq)]
pub struct OcrLine {
    pub text: String,
    pub bounds: PxRect,
}

/// Captures the focused window's pixels (needs Screen Recording permission on macOS).
pub trait FrameSource: Send + Sync {
    fn capture(&self, window: Rect) -> Result<Frame>;
}

/// An 8-bit RGB image, row-major, top row first (`width * height * 3` bytes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RgbFrame {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
}

/// Colour capture of a screen rectangle for vision models (CR-20), scaled so neither side
/// exceeds `max_edge` pixels (aspect ratio kept, never upscaled).
pub trait ColorSource: Send + Sync {
    fn capture_color(&self, window: Rect, max_edge: u32) -> Result<RgbFrame>;
}

/// Text recognition on a grayscale image (Windows.Media.Ocr, Apple Vision).
pub trait OcrEngine: Send + Sync {
    fn recognize(&self, gray: &[u8], width: u32, height: u32) -> Result<Vec<OcrLine>>;
}

/// Tile size for change detection, in pixels.
pub const TILE: u32 = 32;
/// A pixel counts as changed when its luminance moves by more than this.
const PIXEL_DELTA: u8 = 24;
/// A tile counts as changed when at least this many pixels changed (ignores a blinking caret's
/// anti-aliased edge but not a new word).
const MIN_CHANGED_PIXELS: usize = 4;

/// Changed full-width bands from `prev` to `next`, top to bottom. No previous frame, or a
/// different size, means the whole frame changed.
pub fn changed_bands(prev: Option<&Frame>, next: &Frame) -> Vec<PxRect> {
    let full = PxRect { x: 0, y: 0, w: next.width, h: next.height };
    if next.width == 0 || next.height == 0 {
        return vec![];
    }
    let Some(prev) = prev.filter(|p| p.width == next.width && p.height == next.height) else {
        return vec![full];
    };
    let (w, h) = (next.width as usize, next.height as usize);
    let rows = h.div_ceil(TILE as usize);
    let cols = w.div_ceil(TILE as usize);
    let mut row_changed = vec![false; rows];
    for (tr, changed) in row_changed.iter_mut().enumerate() {
        let y0 = tr * TILE as usize;
        let y1 = (y0 + TILE as usize).min(h);
        'tiles: for tc in 0..cols {
            let x0 = tc * TILE as usize;
            let x1 = (x0 + TILE as usize).min(w);
            let mut n = 0;
            for y in y0..y1 {
                let a = &prev.gray[y * w + x0..y * w + x1];
                let b = &next.gray[y * w + x0..y * w + x1];
                n += a.iter().zip(b).filter(|(a, b)| a.abs_diff(**b) > PIXEL_DELTA).count();
                if n >= MIN_CHANGED_PIXELS {
                    *changed = true;
                    break 'tiles;
                }
            }
        }
    }
    // Pad each changed tile row by one row each side (text straddling a tile edge), then merge runs.
    let padded: Vec<bool> = (0..rows)
        .map(|r| row_changed[r] || (r > 0 && row_changed[r - 1]) || (r + 1 < rows && row_changed[r + 1]))
        .collect();
    let mut bands = Vec::new();
    let mut r = 0;
    while r < rows {
        if padded[r] {
            let start = r;
            while r < rows && padded[r] {
                r += 1;
            }
            let y = start as u32 * TILE;
            let y_end = (r as u32 * TILE).min(next.height);
            bands.push(PxRect { x: 0, y, w: next.width, h: y_end - y });
        } else {
            r += 1;
        }
    }
    bands
}

/// Copies `r` out of `frame` as a tightly packed grayscale buffer.
pub fn crop(frame: &Frame, r: PxRect) -> Vec<u8> {
    let w = frame.width as usize;
    let mut out = Vec::with_capacity((r.w * r.h) as usize);
    for y in r.y..r.y + r.h {
        let start = y as usize * w + r.x as usize;
        out.extend_from_slice(&frame.gray[start..start + r.w as usize]);
    }
    out
}

/// Integer box downscale so neither side exceeds `max_dim` (Windows OCR rejects larger images).
/// Returns the new buffer, its size and the factor used.
pub fn downscale_to_fit(gray: &[u8], width: u32, height: u32, max_dim: u32) -> (Vec<u8>, u32, u32, u32) {
    let factor = width.max(height).div_ceil(max_dim.max(1)).max(1);
    if factor == 1 {
        return (gray.to_vec(), width, height, 1);
    }
    let (nw, nh) = (width / factor, height / factor);
    let mut out = Vec::with_capacity((nw * nh) as usize);
    for y in 0..nh {
        for x in 0..nw {
            let mut sum = 0u32;
            for dy in 0..factor {
                let row = ((y * factor + dy) * width) as usize;
                for dx in 0..factor {
                    sum += u32::from(gray[row + (x * factor + dx) as usize]);
                }
            }
            out.push((sum / (factor * factor)) as u8);
        }
    }
    (out, nw, nh, factor)
}

/// Converts 32-bit BGRA/RGBA rows to luminance (alpha ignored). `stride` is bytes per row.
pub fn to_gray(pixels: &[u8], width: u32, height: u32, stride: usize, bgr: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity((width * height) as usize);
    for y in 0..height as usize {
        let row = &pixels[y * stride..y * stride + width as usize * 4];
        for px in row.chunks_exact(4) {
            let (r, g, b) = if bgr { (px[2], px[1], px[0]) } else { (px[0], px[1], px[2]) };
            // ITU-R BT.601 luma, integer.
            out.push(((u32::from(r) * 299 + u32::from(g) * 587 + u32::from(b) * 114) / 1000) as u8);
        }
    }
    out
}

/// OCR state carried between snapshots of the same window.
#[derive(Debug, Default)]
pub struct OcrCache {
    pub frame: Option<Frame>,
    /// Lines in frame pixel space.
    pub lines: Vec<OcrLine>,
}

impl OcrCache {
    /// OCRs the changed bands of `frame`, merges with cached lines outside them, and returns the
    /// lines in reading order. On an engine error the cache is left untouched.
    pub fn update(&mut self, frame: Frame, engine: &dyn OcrEngine) -> Result<&[OcrLine]> {
        let bands = changed_bands(self.frame.as_ref(), &frame);
        let size_changed = self.frame.as_ref().is_none_or(|p| p.width != frame.width || p.height != frame.height);
        let mut fresh = Vec::new();
        for band in &bands {
            let buf = crop(&frame, *band);
            for mut l in engine.recognize(&buf, band.w, band.h)? {
                l.bounds.x += band.x;
                l.bounds.y += band.y;
                fresh.push(l);
            }
        }
        let mut lines: Vec<OcrLine> = if size_changed {
            Vec::new()
        } else {
            std::mem::take(&mut self.lines)
                .into_iter()
                .filter(|l| !bands.iter().any(|b| b.intersects(&l.bounds)))
                .collect()
        };
        lines.extend(fresh);
        lines.sort_by_key(|l| (l.bounds.y, l.bounds.x));
        self.lines = lines;
        self.frame = Some(frame);
        Ok(&self.lines)
    }
}

/// Maps OCR lines to screen-space nodes, dropping any line over a password field.
pub fn lines_to_nodes(lines: &[OcrLine], frame: &Frame, password_regions: &[Rect]) -> Vec<ScreenNode> {
    let sx = f64::from(frame.screen.w) / f64::from(frame.width.max(1));
    let sy = f64::from(frame.screen.h) / f64::from(frame.height.max(1));
    lines
        .iter()
        .filter(|l| !l.text.trim().is_empty())
        .map(|l| {
            let b = Rect {
                x: frame.screen.x + (f64::from(l.bounds.x) * sx) as i32,
                y: frame.screen.y + (f64::from(l.bounds.y) * sy) as i32,
                w: (f64::from(l.bounds.w) * sx).ceil() as i32,
                h: (f64::from(l.bounds.h) * sy).ceil() as i32,
            };
            ScreenNode { role: "OcrLine".into(), text: l.text.trim().to_string(), bounds: Some(b) }
        })
        .filter(|n| n.bounds.is_none_or(|b| !password_regions.iter().any(|p| rects_intersect(&b, p))))
        .collect()
}

pub fn rects_intersect(a: &Rect, b: &Rect) -> bool {
    a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
}

#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use std::sync::Mutex;

    /// "Recognises" every 16-row strip that is not blank as one line `L<y>`; records each call.
    #[derive(Default)]
    pub struct FakeOcr {
        pub calls: Mutex<Vec<(u32, u32)>>,
    }

    impl OcrEngine for FakeOcr {
        fn recognize(&self, gray: &[u8], width: u32, height: u32) -> Result<Vec<OcrLine>> {
            self.calls.lock().unwrap().push((width, height));
            let mut out = Vec::new();
            for y in (0..height).step_by(16) {
                let h = 16.min(height - y);
                let strip = &gray[(y * width) as usize..((y + h) * width) as usize];
                if let Some(v) = strip.iter().copied().find(|&v| v != 0) {
                    out.push(OcrLine { text: format!("text{v}"), bounds: PxRect { x: 0, y, w: width, h } });
                }
            }
            Ok(out)
        }
    }

    pub fn blank(w: u32, h: u32) -> Frame {
        Frame {
            screen: Rect { x: 100, y: 50, w: (w / 2) as i32, h: (h / 2) as i32 },
            width: w,
            height: h,
            gray: vec![0; (w * h) as usize],
        }
    }

    /// Paints rows `y0..y1` with value `v`.
    pub fn paint(f: &mut Frame, y0: u32, y1: u32, v: u8) {
        for y in y0..y1 {
            let s = (y * f.width) as usize;
            f.gray[s..s + f.width as usize].fill(v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::*;
    use super::*;

    #[test]
    fn first_frame_is_one_full_band() {
        let f = blank(100, 70);
        assert_eq!(changed_bands(None, &f), vec![PxRect { x: 0, y: 0, w: 100, h: 70 }]);
    }

    #[test]
    fn identical_frames_have_no_bands() {
        let f = blank(256, 256);
        assert!(changed_bands(Some(&f), &f.clone()).is_empty());
    }

    #[test]
    fn tiny_change_is_ignored() {
        let a = blank(256, 256);
        let mut b = a.clone();
        b.gray[1000] = 255; // one pixel: caret blink, cursor edge
        assert!(changed_bands(Some(&a), &b).is_empty());
    }

    #[test]
    fn change_yields_padded_full_width_band() {
        let a = blank(256, 320);
        let mut b = a.clone();
        paint(&mut b, 130, 140, 200); // tile row 4
        assert_eq!(changed_bands(Some(&a), &b), vec![PxRect { x: 0, y: 96, w: 256, h: 96 }]);
    }

    #[test]
    fn separate_changes_give_separate_bands() {
        let a = blank(64, 640);
        let mut b = a.clone();
        paint(&mut b, 0, 5, 200);
        paint(&mut b, 600, 610, 200);
        let bands = changed_bands(Some(&a), &b);
        assert_eq!(bands.len(), 2);
        assert_eq!(bands[0].y, 0);
        assert_eq!(bands[1].y + bands[1].h, 640);
    }

    #[test]
    fn resized_frame_is_full_change() {
        assert_eq!(changed_bands(Some(&blank(64, 64)), &blank(64, 96)).len(), 1);
    }

    #[test]
    fn crop_and_downscale() {
        let mut f = blank(4, 4);
        f.gray = (0..16).collect();
        assert_eq!(crop(&f, PxRect { x: 1, y: 2, w: 2, h: 2 }), vec![9, 10, 13, 14]);
        let (d, w, h, k) = downscale_to_fit(&f.gray, 4, 4, 2);
        assert_eq!((w, h, k), (2, 2, 2));
        assert_eq!(d, vec![10 / 4, 18 / 4, 42 / 4, 50 / 4]); // means of each 2x2 block
        let (d, w, h, k) = downscale_to_fit(&f.gray, 4, 4, 2600);
        assert_eq!((d.len(), w, h, k), (16, 4, 4, 1));
    }

    #[test]
    fn gray_conversion() {
        // One white, one pure-red pixel, BGRA, stride with padding.
        let px = [255, 255, 255, 255, 0, 0, 255, 255, 9, 9];
        assert_eq!(to_gray(&px, 2, 1, 10, true), vec![255, 76]);
        assert_eq!(to_gray(&px[..8], 2, 1, 8, false), vec![255, 29]);
    }

    #[test]
    fn cache_only_ocrs_changed_regions_and_keeps_the_rest() {
        let engine = FakeOcr::default();
        let mut cache = OcrCache::default();
        let mut f1 = blank(64, 640);
        paint(&mut f1, 16, 32, 7);
        paint(&mut f1, 400, 416, 8);
        let lines = cache.update(f1.clone(), &engine).unwrap().to_vec();
        assert_eq!(lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(), vec!["text7", "text8"]);
        assert_eq!(engine.calls.lock().unwrap().as_slice(), &[(64, 640)]);

        // Change only the bottom: the engine sees one small band, the top line comes from cache.
        let mut f2 = f1.clone();
        paint(&mut f2, 400, 416, 200);
        let lines = cache.update(f2.clone(), &engine).unwrap().to_vec();
        assert_eq!(lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(), vec!["text7", "text200"]);
        let calls = engine.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 2);
        assert!(calls[1].1 <= 3 * TILE, "OCR'd {} rows", calls[1].1);
        assert_eq!(lines[1].bounds.y, 400);

        // No change: no OCR call at all.
        cache.update(f2, &engine).unwrap();
        assert_eq!(engine.calls.lock().unwrap().len(), 2);
    }

    #[test]
    fn nodes_are_mapped_to_screen_and_password_lines_dropped() {
        let f = blank(200, 100); // screen rect 100,50 100x50 => scale 0.5
        let lines = vec![
            OcrLine { text: " Hello ".into(), bounds: PxRect { x: 0, y: 0, w: 200, h: 20 } },
            OcrLine { text: "••••••".into(), bounds: PxRect { x: 0, y: 60, w: 200, h: 20 } },
        ];
        let pw = [Rect { x: 110, y: 80, w: 50, h: 10 }];
        let nodes = lines_to_nodes(&lines, &f, &pw);
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].text, "Hello");
        assert_eq!(nodes[0].bounds, Some(Rect { x: 100, y: 50, w: 100, h: 10 }));
    }
}
