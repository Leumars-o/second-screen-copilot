//! Windows backend: UI Automation tree walk (`uiautomation` crate), GDI capture of the window
//! rectangle, Windows.Media.Ocr.
//!
//! Coordinates: UIA bounding rectangles and the GDI capture are both in physical pixels when
//! the host process is per-monitor DPI aware (the Tauri shell is); otherwise Windows scales
//! them consistently for the process, so OCR bounds still line up with UIA bounds.
#![allow(unsafe_code)] // Win32 GDI and process FFI.

use crate::ocr::{Frame, FrameSource, OcrEngine, OcrLine, PxRect, downscale_to_fit};
use crate::reader::{AxBackend, WindowRead};
use crate::walk::{AxNode, WalkLimits, walk};
use ssc_core::types::Rect;
use ssc_core::{Result, SscError};
use uiautomation::core::UICacheRequest;
use uiautomation::patterns::UITextPattern;
use uiautomation::types::{ControlType, Handle, UIProperty};
use uiautomation::variants::Variant;
use uiautomation::{UIAutomation, UIElement, UITreeWalker};

fn uia_err(e: uiautomation::Error) -> SscError {
    SscError::Unavailable(format!("UI Automation: {e}"))
}

// ---------- UI Automation ----------

/// Properties fetched with each element in one cross-process call. `ValueValue` is deliberately
/// not cached: it is read live, and only after `IsPassword` was checked.
const CACHED: &[UIProperty] = &[
    UIProperty::Name,
    UIProperty::ControlType,
    UIProperty::IsPassword,
    UIProperty::BoundingRectangle,
    UIProperty::IsValuePatternAvailable,
    UIProperty::IsTextPatternAvailable,
];

struct Ctx {
    walker: UITreeWalker,
    cache: UICacheRequest,
    max_text: usize,
    /// Walk deadline: sibling enumeration of a huge list stops here too.
    deadline: std::time::Instant,
}

struct WinNode<'a> {
    el: UIElement,
    ctx: &'a Ctx,
}

fn cached_bool(el: &UIElement, p: UIProperty) -> bool {
    el.get_cached_property_value(p).ok().and_then(|v: Variant| v.try_into().ok()).unwrap_or(false)
}

fn to_rect(r: uiautomation::types::Rect) -> Rect {
    Rect { x: r.get_left(), y: r.get_top(), w: r.get_right() - r.get_left(), h: r.get_bottom() - r.get_top() }
}

impl AxNode for WinNode<'_> {
    fn is_password(&self) -> bool {
        // Fail closed: if the property cannot be read, treat the element as a password field.
        self.el.is_cached_password().unwrap_or(true)
    }

    fn role(&self) -> String {
        self.el.get_cached_control_type().map(|t| format!("{t:?}")).unwrap_or_default()
    }

    fn text(&self) -> Option<String> {
        let ct = self.el.get_cached_control_type().ok()?;
        if matches!(ct, ControlType::Window | ControlType::ScrollBar | ControlType::Thumb | ControlType::TitleBar) {
            return None;
        }
        let name = self.el.get_cached_name().unwrap_or_default();
        let editable = matches!(ct, ControlType::Edit | ControlType::ComboBox | ControlType::Document);
        if editable && cached_bool(&self.el, UIProperty::IsValuePatternAvailable) {
            let value: Option<String> = self
                .el
                .get_property_value(UIProperty::ValueValue)
                .ok()
                .and_then(|v| v.try_into().ok())
                .filter(|s: &String| !s.trim().is_empty());
            if let Some(v) = value {
                return Some(v);
            }
        }
        // Text pattern only on leaves: on a container (a browser document) it would repeat
        // every child's text.
        if editable
            && cached_bool(&self.el, UIProperty::IsTextPatternAvailable)
            && self.ctx.walker.get_first_child(&self.el).is_err()
            && let Ok(t) = self
                .el
                .get_pattern::<UITextPattern>()
                .and_then(|p| p.get_document_range())
                .and_then(|r| r.get_text(self.ctx.max_text as i32))
            && !t.trim().is_empty()
        {
            return Some(t);
        }
        (!name.trim().is_empty()).then_some(name)
    }

    fn bounds(&self) -> Option<Rect> {
        self.el.get_cached_bounding_rectangle().ok().map(to_rect).filter(|r| r.w > 0 && r.h > 0)
    }

    fn children(&self, max: usize) -> Vec<Self> {
        let mut out = Vec::new();
        let mut next = self.ctx.walker.get_first_child_build_cache(&self.el, &self.ctx.cache);
        while let Ok(el) = next {
            if out.len() >= max || std::time::Instant::now() >= self.ctx.deadline {
                break;
            }
            next = self.ctx.walker.get_next_sibling_build_cache(&el, &self.ctx.cache);
            out.push(WinNode { el, ctx: self.ctx });
        }
        out
    }
}

/// Executable stem of a process, e.g. "Zoom" or "chrome".
fn process_name(pid: u32) -> Option<String> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    };
    use windows::core::PWSTR;
    // SAFETY: handle is closed below; the buffer outlives the call and `len` is its capacity.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(h);
        if !ok {
            return None;
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        std::path::Path::new(&path).file_stem().map(|s| s.to_string_lossy().into_owned())
    }
}

/// Reads the foreground window through UI Automation.
pub struct UiaReader;

impl AxBackend for UiaReader {
    fn read_focused(&self, limits: &WalkLimits) -> Result<WindowRead> {
        use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
        // COM may already be initialised on this thread in another mode; then use it as is.
        let automation = UIAutomation::new().or_else(|_| UIAutomation::new_direct()).map_err(uia_err)?;
        let cache = automation.create_cache_request().map_err(uia_err)?;
        for p in CACHED {
            cache.add_property(*p).map_err(uia_err)?;
        }
        let walker = automation.get_control_view_walker().map_err(uia_err)?;
        // SAFETY: no arguments; returns a null HWND when nothing is focused.
        let hwnd = unsafe { GetForegroundWindow() };
        if hwnd.is_invalid() {
            return Err(SscError::Unavailable("no foreground window".into()));
        }
        let root = automation.element_from_handle_build_cache(Handle::from(hwnd), &cache).map_err(uia_err)?;
        let window_title = root.get_cached_name().unwrap_or_default();
        let app = root.get_process_id().ok().and_then(process_name).unwrap_or_default();
        let bounds = root.get_cached_bounding_rectangle().ok().map(to_rect);
        let deadline = std::time::Instant::now() + limits.budget;
        let ctx = Ctx { walker, cache, max_text: limits.max_text_bytes, deadline };
        let walk = walk(WinNode { el: root, ctx: &ctx }, limits);
        Ok(WindowRead { app, window_title, bounds, walk })
    }
}

// ---------- capture (GDI) ----------

/// Copies the window rectangle from the screen DC (what the user sees, overlaps included).
pub struct GdiCapture;

impl FrameSource for GdiCapture {
    fn capture(&self, window: Rect) -> Result<Frame> {
        use windows::Win32::Graphics::Gdi::{
            BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DIB_RGB_COLORS,
            DeleteDC, DeleteObject, GetDC, GetDIBits, ReleaseDC, SRCCOPY, SelectObject,
        };
        let (w, h) = (window.w, window.h);
        if w <= 0 || h <= 0 {
            return Err(SscError::Invalid("empty window rectangle".into()));
        }
        let mut bgra = vec![0u8; (w * h * 4) as usize];
        // SAFETY: standard GDI screen grab; every object created here is released before return,
        // and `bgra` holds exactly w*h 32-bit pixels for GetDIBits (top-down, negative height).
        let copied = unsafe {
            let screen = GetDC(None);
            let mem = CreateCompatibleDC(Some(screen));
            let bmp = CreateCompatibleBitmap(screen, w, h);
            let old = SelectObject(mem, bmp.into());
            let blt = BitBlt(mem, 0, 0, w, h, Some(screen), window.x, window.y, SRCCOPY);
            let mut info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let lines = GetDIBits(mem, bmp, 0, h as u32, Some(bgra.as_mut_ptr().cast()), &mut info, DIB_RGB_COLORS);
            SelectObject(mem, old);
            let _ = DeleteObject(bmp.into());
            let _ = DeleteDC(mem);
            ReleaseDC(None, screen);
            blt.is_ok() && lines == h
        };
        if !copied {
            return Err(SscError::Unavailable("GDI screen capture failed".into()));
        }
        let gray = crate::ocr::to_gray(&bgra, w as u32, h as u32, w as usize * 4, true);
        Ok(Frame { screen: window, width: w as u32, height: h as u32, gray })
    }
}

// ---------- OCR (Windows.Media.Ocr) ----------

/// Windows.Media.Ocr with the user's profile languages.
pub struct WinOcr;

impl OcrEngine for WinOcr {
    fn recognize(&self, gray: &[u8], width: u32, height: u32) -> Result<Vec<OcrLine>> {
        use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
        use windows::Media::Ocr::OcrEngine as WinOcrEngine;
        use windows::Storage::Streams::DataWriter;

        let os = |e: windows::core::Error| SscError::Unavailable(format!("Windows OCR: {e}"));
        if width == 0 || height == 0 || gray.len() < (width * height) as usize {
            return Err(SscError::Invalid("bad OCR image size".into()));
        }
        let engine = WinOcrEngine::TryCreateFromUserProfileLanguages().map_err(os)?;
        let max = WinOcrEngine::MaxImageDimension().map_err(os)?;
        let (small, w, h, factor) = downscale_to_fit(gray, width, height, max);
        // Bgra8 is the format every OCR sample uses; expand gray with opaque alpha.
        let bgra: Vec<u8> = small.iter().flat_map(|&g| [g, g, g, 255]).collect();
        let writer = DataWriter::new().map_err(os)?;
        writer.WriteBytes(&bgra).map_err(os)?;
        let buffer = writer.DetachBuffer().map_err(os)?;
        let bitmap = SoftwareBitmap::Create(BitmapPixelFormat::Bgra8, w as i32, h as i32).map_err(os)?;
        bitmap.CopyFromBuffer(&buffer).map_err(os)?;
        let result = engine.RecognizeAsync(&bitmap).map_err(os)?.join().map_err(os)?;
        let mut out = Vec::new();
        for line in result.Lines().map_err(os)? {
            let text = line.Text().map_err(os)?.to_string_lossy();
            let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, 0f32, 0f32);
            for word in line.Words().map_err(os)? {
                let r = word.BoundingRect().map_err(os)?;
                x0 = x0.min(r.X);
                y0 = y0.min(r.Y);
                x1 = x1.max(r.X + r.Width);
                y1 = y1.max(r.Y + r.Height);
            }
            if x1 <= x0 || y1 <= y0 {
                continue;
            }
            let f = factor as f32;
            out.push(OcrLine {
                text,
                bounds: PxRect {
                    x: (x0 * f) as u32,
                    y: (y0 * f) as u32,
                    w: ((x1 - x0) * f).ceil() as u32,
                    h: ((y1 - y0) * f).ceil() as u32,
                },
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ssc_core::traits::ScreenSource;

    /// Runs on Windows CI (no window needed): OCR of a blank image returns no lines.
    #[test]
    fn win_ocr_blank_image() {
        match WinOcr.recognize(&vec![255u8; 200 * 50], 200, 50) {
            Ok(lines) => assert!(lines.is_empty(), "{lines:?}"),
            // Server SKUs without the OCR language pack.
            Err(SscError::Unavailable(e)) => eprintln!("OCR unavailable on this machine: {e}"),
            Err(e) => panic!("{e}"),
        }
    }

    /// *Manual*: reads the real foreground window (Chrome, Zoom).
    #[test]
    #[ignore = "manual: needs a real foreground window"]
    fn real_foreground_window() {
        let reader = crate::platform_reader().unwrap();
        let t = std::time::Instant::now();
        let s = reader.snapshot().unwrap();
        eprintln!(
            "{:?} app={:?} title={:?} nodes={} source={:?}\n{}",
            t.elapsed(),
            s.app,
            s.window_title,
            s.nodes.len(),
            s.source,
            s.text()
        );
    }
}
