//! macOS backend: AXUIElement tree walk, ScreenCaptureKit capture, Apple Vision OCR.
//!
//! Needs the Accessibility permission (AX) and, for OCR only, Screen Recording. Without them the
//! calls return [`SscError::Unavailable`]; nothing here prompts the user.
#![allow(unsafe_code)] // FFI to ApplicationServices, CoreGraphics, ScreenCaptureKit and Vision.

use crate::ocr::{ColorSource, Frame, FrameSource, OcrEngine, OcrLine, PxRect, RgbFrame};
use crate::reader::{AxBackend, WindowRead};
use crate::walk::{AxNode, WalkLimits, walk};
use accessibility_sys::*;
use core_foundation::array::{CFArray, CFArrayRef};
use core_foundation::base::{CFType, CFTypeRef, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::string::CFString;
use ssc_core::types::Rect;
use ssc_core::{Result, SscError};
use std::cell::OnceCell;
use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::Mutex;

// ---------- AXUIElement ----------

/// Owned (+1) AXUIElementRef.
struct AxEl(AXUIElementRef);

impl Drop for AxEl {
    fn drop(&mut self) {
        // SAFETY: we own one reference.
        unsafe { core_foundation::base::CFRelease(self.0 as CFTypeRef) }
    }
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct CgPoint {
    x: f64,
    y: f64,
}

impl AxEl {
    /// Takes ownership of a +1 reference (from a Create/Copy function).
    fn from_create(r: AXUIElementRef) -> Option<Self> {
        (!r.is_null()).then_some(Self(r))
    }

    /// Retains a borrowed reference.
    fn from_get(r: CFTypeRef) -> Self {
        // SAFETY: `r` is a live AXUIElementRef borrowed from a container we hold.
        unsafe { core_foundation::base::CFRetain(r) };
        Self(r as AXUIElementRef)
    }

    fn attr(&self, name: &str) -> Option<CFType> {
        self.attr_err(name).ok()
    }

    /// Like [`Self::attr`], keeping the AXError code for diagnostics.
    fn attr_err(&self, name: &str) -> std::result::Result<CFType, i32> {
        let name = CFString::new(name);
        let mut v: CFTypeRef = std::ptr::null();
        // SAFETY: valid element and attribute; `v` receives a +1 reference on success.
        let err = unsafe { AXUIElementCopyAttributeValue(self.0, name.as_concrete_TypeRef(), &mut v) };
        if err != kAXErrorSuccess || v.is_null() {
            return Err(err);
        }
        // SAFETY: Copy rule, +1.
        Ok(unsafe { CFType::wrap_under_create_rule(v) })
    }

    /// Several attributes in one IPC round trip. Missing ones come back as `None`.
    fn attrs(&self, names: &[&str]) -> Vec<Option<CFType>> {
        let keys: Vec<CFString> = names.iter().map(|n| CFString::new(n)).collect();
        let keys = CFArray::from_CFTypes(&keys);
        let mut out: CFArrayRef = std::ptr::null();
        // SAFETY: valid element and array of CFStrings; `out` receives a +1 array.
        let err = unsafe { AXUIElementCopyMultipleAttributeValues(self.0, keys.as_concrete_TypeRef(), 0, &mut out) };
        if err != kAXErrorSuccess || out.is_null() {
            return vec![None; names.len()];
        }
        // SAFETY: Copy rule, +1.
        let arr: CFArray<CFType> = unsafe { CFArray::wrap_under_create_rule(out) };
        let mut vals: Vec<Option<CFType>> = arr.iter().map(|v| Some(v.clone())).collect();
        vals.resize(names.len(), None);
        vals
    }

    fn string(&self, name: &str) -> Option<String> {
        as_string(self.attr(name))
    }

    fn element(&self, name: &str) -> Option<AxEl> {
        let v = self.attr(name)?;
        // SAFETY: plain type-id query.
        (v.type_of() == unsafe { AXUIElementGetTypeID() }).then(|| AxEl::from_get(v.as_CFTypeRef()))
    }

    fn pid(&self) -> Option<i32> {
        let mut pid = 0;
        // SAFETY: valid element, out pointer.
        (unsafe { AXUIElementGetPid(self.0, &mut pid) } == kAXErrorSuccess).then_some(pid)
    }

    fn bounds(&self) -> Option<Rect> {
        let v = self.attrs(&[kAXPositionAttribute, kAXSizeAttribute]);
        let pos = ax_value(v[0].as_ref(), kAXValueTypeCGPoint)?;
        let size = ax_value(v[1].as_ref(), kAXValueTypeCGSize)?; // CGSize has the same layout
        Some(Rect { x: pos.x as i32, y: pos.y as i32, w: size.x.ceil() as i32, h: size.y.ceil() as i32 })
    }

    fn children(&self, max: usize) -> Vec<AxEl> {
        let name = CFString::new(kAXChildrenAttribute);
        let mut out: CFArrayRef = std::ptr::null();
        // SAFETY: valid element; `out` receives a +1 array.
        let err = unsafe { AXUIElementCopyAttributeValues(self.0, name.as_concrete_TypeRef(), 0, max as _, &mut out) };
        if err != kAXErrorSuccess || out.is_null() {
            return vec![];
        }
        // SAFETY: Copy rule, +1.
        let arr: CFArray<CFType> = unsafe { CFArray::wrap_under_create_rule(out) };
        // SAFETY: plain type-id query.
        let ax_type = unsafe { AXUIElementGetTypeID() };
        arr.iter().filter(|c| c.type_of() == ax_type).map(|c| AxEl::from_get(c.as_CFTypeRef())).collect()
    }
}

fn as_string(v: Option<CFType>) -> Option<String> {
    v?.downcast::<CFString>().map(|s| s.to_string())
}

fn ax_value(v: Option<&CFType>, kind: u32) -> Option<CgPoint> {
    let v = v?;
    // SAFETY: type-checked before AXValueGetValue writes two f64s into `p`.
    unsafe {
        if v.type_of() != AXValueGetTypeID() {
            return None;
        }
        let mut p = CgPoint::default();
        AXValueGetValue(v.as_CFTypeRef() as AXValueRef, kind, (&mut p as *mut CgPoint).cast::<c_void>()).then_some(p)
    }
}

/// Roles whose own text is noise or duplicated elsewhere (the window title is reported apart).
const SKIP_TEXT_ROLES: &[&str] = &["AXWindow", "AXApplication", "AXScrollBar", "AXSplitter", "AXValueIndicator"];

struct MacNode {
    el: AxEl,
    /// (role, subrole), fetched lazily in one round trip.
    roles: OnceCell<(String, String)>,
}

impl MacNode {
    fn new(el: AxEl) -> Self {
        Self { el, roles: OnceCell::new() }
    }
    fn roles(&self) -> &(String, String) {
        self.roles.get_or_init(|| {
            let mut v = self.el.attrs(&[kAXRoleAttribute, kAXSubroleAttribute]).into_iter();
            let role = as_string(v.next().flatten()).unwrap_or_default();
            let sub = as_string(v.next().flatten()).unwrap_or_default();
            (role, sub)
        })
    }
}

impl AxNode for MacNode {
    fn is_password(&self) -> bool {
        let (role, sub) = self.roles();
        // Fail closed: an element whose role cannot be read is treated as a password field.
        role.is_empty() || role == kAXSecureTextFieldSubrole || sub == kAXSecureTextFieldSubrole
    }
    fn role(&self) -> String {
        self.roles().0.clone()
    }
    fn text(&self) -> Option<String> {
        if SKIP_TEXT_ROLES.contains(&self.roles().0.as_str()) {
            return None;
        }
        let v = self.el.attrs(&[kAXValueAttribute, kAXTitleAttribute, kAXDescriptionAttribute]);
        v.into_iter().map(as_string).find_map(|s| s.filter(|s| !s.trim().is_empty()))
    }
    fn bounds(&self) -> Option<Rect> {
        self.el.bounds()
    }
    fn children(&self, max: usize) -> Vec<Self> {
        self.el.children(max).into_iter().map(MacNode::new).collect()
    }
}

/// Reads the focused window through AXUIElement.
pub struct AxReader {
    /// Apps already asked to expose their web content (Chrome, Electron: `AXManualAccessibility`).
    enabled_pids: Mutex<HashSet<i32>>,
}

impl AxReader {
    pub fn new() -> Self {
        Self { enabled_pids: Mutex::new(HashSet::new()) }
    }

    /// True once the user has granted the Accessibility permission to this process.
    pub fn is_trusted() -> bool {
        // SAFETY: no arguments.
        unsafe { AXIsProcessTrusted() }
    }
}

/// Floor for the per-message Accessibility timeout, seconds.
const MIN_AX_MESSAGE_TIMEOUT_S: f32 = 0.25;

/// Human name of an AXError code, for logs.
fn ax_error_name(code: i32) -> String {
    let name = match code {
        -25200 => "failure",
        -25201 => "illegal argument",
        -25202 => "invalid element",
        -25204 => "cannot complete: the app did not answer in time",
        -25205 => "attribute unsupported",
        -25211 => "API disabled: Accessibility permission",
        -25212 => "no value",
        _ => "error",
    };
    format!("AXError {code}: {name}")
}

/// The frontmost normal window from the window server (no Accessibility needed): app name,
/// window title (empty without Screen Recording) and bounds in global points. Our own windows
/// are skipped. Front-to-back order, first layer-0 window big enough to be a real window.
pub fn front_window() -> Option<(String, String, Rect)> {
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::number::CFNumber;
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGWindowListCopyWindowInfo(option: u32, relative_to: u32) -> CFArrayRef;
    }
    const ON_SCREEN_ONLY: u32 = 1;
    const EXCLUDE_DESKTOP: u32 = 16;
    // SAFETY: plain call; returns a +1 CFArray of CFDictionary (or null).
    let raw = unsafe { CGWindowListCopyWindowInfo(ON_SCREEN_ONLY | EXCLUDE_DESKTOP, 0) };
    if raw.is_null() {
        return None;
    }
    // SAFETY: Copy rule, +1; elements are CFDictionaryRef.
    let list: CFArray<CFDictionary<CFString, CFType>> = unsafe { CFArray::wrap_under_create_rule(raw) };
    let num = |d: &CFDictionary<CFString, CFType>, k: &str| -> Option<f64> {
        d.find(CFString::new(k)).and_then(|v| v.downcast::<CFNumber>()).and_then(|n| n.to_f64())
    };
    let text = |d: &CFDictionary<CFString, CFType>, k: &str| -> String {
        d.find(CFString::new(k)).and_then(|v| v.downcast::<CFString>()).map(|s| s.to_string()).unwrap_or_default()
    };
    let mut wins = Vec::new();
    for w in list.iter() {
        let Some(b) = w.find(CFString::new("kCGWindowBounds")).and_then(|v| v.downcast::<CFDictionary>()) else {
            continue;
        };
        // SAFETY: the bounds dictionary maps CFString keys to CFNumber values.
        let b: CFDictionary<CFString, CFType> = unsafe { CFDictionary::wrap_under_get_rule(b.as_concrete_TypeRef()) };
        let (Some(x), Some(y), Some(width), Some(height)) =
            (num(&b, "X"), num(&b, "Y"), num(&b, "Width"), num(&b, "Height"))
        else {
            continue;
        };
        wins.push(crate::grab::WinInfo {
            app: text(&w, "kCGWindowOwnerName"),
            title: text(&w, "kCGWindowName"),
            pid: num(&w, "kCGWindowOwnerPID").map_or(-1, |p| p as i64),
            layer: num(&w, "kCGWindowLayer").map_or(-1, |l| l as i64),
            alpha: num(&w, "kCGWindowAlpha").unwrap_or(1.0),
            rect: Rect { x: x as i32, y: y as i32, w: width.ceil() as i32, h: height.ceil() as i32 },
        });
    }
    crate::grab::pick_front_window(&wins, std::process::id() as i64)
}

impl Default for AxReader {
    fn default() -> Self {
        Self::new()
    }
}

impl AxBackend for AxReader {
    fn front_window(&self) -> Option<(String, String, Rect)> {
        front_window()
    }

    fn read_focused(&self, limits: &WalkLimits) -> Result<WindowRead> {
        if !Self::is_trusted() {
            return Err(SscError::Unavailable("macOS Accessibility permission not granted".into()));
        }
        // SAFETY: Create rule, +1.
        let sys = AxEl::from_create(unsafe { AXUIElementCreateSystemWide() })
            .ok_or_else(|| SscError::Unavailable("AXUIElementCreateSystemWide failed".into()))?;
        // A hung app must not stall the walk: cap every AX message (global when set on system-wide).
        // Not below 250 ms: the walk enforces its own budget between nodes, and a busy app (a
        // large document) can take longer than a 50 ms walk budget just to name the focused app.
        // SAFETY: valid element.
        unsafe { AXUIElementSetMessagingTimeout(sys.0, limits.budget.as_secs_f32().max(MIN_AX_MESSAGE_TIMEOUT_S)) };
        let app = match sys.attr_err(kAXFocusedApplicationAttribute) {
            // SAFETY: plain type-id query.
            Ok(v) if v.type_of() == unsafe { AXUIElementGetTypeID() } => AxEl::from_get(v.as_CFTypeRef()),
            Ok(_) => return Err(SscError::Unavailable("no focused application (unexpected type)".into())),
            Err(code) => {
                return Err(SscError::Unavailable(format!("no focused application ({})", ax_error_name(code))));
            }
        };
        if let Some(pid) = app.pid() {
            let mut seen = self.enabled_pids.lock().unwrap_or_else(|e| e.into_inner());
            if seen.insert(pid) {
                let key = CFString::new("AXManualAccessibility");
                // SAFETY: valid element, CFString key, CFBoolean value. Unsupported apps return an error we ignore.
                unsafe {
                    AXUIElementSetAttributeValue(
                        app.0,
                        key.as_concrete_TypeRef(),
                        CFBoolean::true_value().as_CFTypeRef(),
                    )
                };
            }
        }
        let app_name = app.string(kAXTitleAttribute).unwrap_or_default();
        let Some(win) = app.element(kAXFocusedWindowAttribute).or_else(|| app.element(kAXMainWindowAttribute)) else {
            return Ok(WindowRead { app: app_name, ..Default::default() });
        };
        let window_title = win.string(kAXTitleAttribute).unwrap_or_default();
        let bounds = win.bounds();
        let walk = walk(MacNode::new(win), limits);
        Ok(WindowRead { app: app_name, window_title, bounds, walk })
    }
}

// ---------- capture (ScreenCaptureKit; CoreGraphics kept for comparison) ----------

use objc2_core_foundation::{CFData, CGPoint, CGRect, CGSize};
#[allow(deprecated)] // CGWindowListCreateImage, see CgCapture.
use objc2_core_graphics::{
    CGBitmapContextCreate, CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGContext, CGDataProvider, CGImage,
    CGImageAlphaInfo, CGPreflightScreenCaptureAccess, CGWindowImageOption, CGWindowListCreateImage, CGWindowListOption,
    kCGNullWindowID,
};
use screencapturekit::screenshot_manager::SCScreenshotManager;

fn check_capture_access(window: Rect) -> Result<()> {
    if window.w <= 0 || window.h <= 0 {
        return Err(SscError::Invalid(format!("empty capture rect {window:?}")));
    }
    if !CGPreflightScreenCaptureAccess() {
        return Err(SscError::Unavailable("macOS Screen Recording permission not granted".into()));
    }
    Ok(())
}

/// Renders any `CGImage` into an 8-bit gray buffer we own (`width * height` bytes, row-major,
/// top row first). CoreGraphics converts whatever the source format is (BGRA from
/// ScreenCaptureKit, the display's format from CGWindowList) with colour management.
fn cgimage_to_gray(img: &CGImage) -> Result<(u32, u32, Vec<u8>)> {
    let (w, h) = (CGImage::width(Some(img)), CGImage::height(Some(img)));
    if w == 0 || h == 0 {
        return Err(SscError::Unavailable("empty capture".into()));
    }
    let (Ok(w32), Ok(h32)) = (u32::try_from(w), u32::try_from(h)) else {
        return Err(SscError::Invalid("capture too large".into()));
    };
    let mut gray = vec![0u8; w * h];
    let cs = CGColorSpace::new_device_gray();
    // SAFETY: `gray` is w*h bytes, bytes_per_row = w, 8 bpc gray, no alpha; it outlives the context.
    let ctx =
        unsafe { CGBitmapContextCreate(gray.as_mut_ptr().cast(), w, h, 8, w, cs.as_deref(), CGImageAlphaInfo::None.0) }
            .ok_or_else(|| SscError::Unavailable("CGBitmapContextCreate failed".into()))?;
    let full = CGRect { origin: CGPoint { x: 0.0, y: 0.0 }, size: CGSize { width: w as f64, height: h as f64 } };
    CGContext::draw_image(Some(&ctx), full, Some(img));
    drop(ctx);
    Ok((w32, h32, gray))
}

/// Renders a `CGImage` into an RGB buffer of `w x h` pixels (CoreGraphics scales and colour-
/// manages), for the CR-20 screen hotkey.
fn cgimage_to_rgb(img: &CGImage, max_edge: u32) -> Result<RgbFrame> {
    let (sw, sh) = (CGImage::width(Some(img)), CGImage::height(Some(img)));
    let (Ok(sw), Ok(sh)) = (u32::try_from(sw), u32::try_from(sh)) else {
        return Err(SscError::Invalid("capture too large".into()));
    };
    if sw == 0 || sh == 0 {
        return Err(SscError::Unavailable("empty capture".into()));
    }
    let (w, h) = crate::grab::fit_within(sw, sh, max_edge);
    let (wu, hu) = (w as usize, h as usize);
    let mut rgba = vec![0u8; wu * hu * 4];
    let cs = CGColorSpace::new_device_rgb();
    // SAFETY: `rgba` is w*h*4 bytes, bytes_per_row = 4w, 8 bpc RGBA premultiplied-last; it
    // outlives the context.
    let ctx = unsafe {
        CGBitmapContextCreate(
            rgba.as_mut_ptr().cast(),
            wu,
            hu,
            8,
            wu * 4,
            cs.as_deref(),
            CGImageAlphaInfo::PremultipliedLast.0,
        )
    }
    .ok_or_else(|| SscError::Unavailable("CGBitmapContextCreate failed".into()))?;
    CGContext::set_interpolation_quality(Some(&ctx), objc2_core_graphics::CGInterpolationQuality::High);
    let full = CGRect { origin: CGPoint { x: 0.0, y: 0.0 }, size: CGSize { width: w as f64, height: h as f64 } };
    CGContext::draw_image(Some(&ctx), full, Some(img));
    drop(ctx);
    let rgb: Vec<u8> = rgba.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect();
    Ok(RgbFrame { width: w, height: h, rgb })
}

/// Captures a screen rectangle with ScreenCaptureKit (`SCScreenshotManager.captureImage(in:)`,
/// macOS 15.2+). Needs Screen Recording, the same permission `ssc-audio` uses for system audio.
/// The rect is in global display points, like accessibility bounds; the image comes back at the
/// display's backing scale (2x on Retina).
pub struct SckCapture;

impl FrameSource for SckCapture {
    fn capture(&self, window: Rect) -> Result<Frame> {
        check_capture_access(window)?;
        let rect =
            screencapturekit::cg::CGRect::new(window.x.into(), window.y.into(), window.w.into(), window.h.into());
        let img = SCScreenshotManager::capture_image_in_rect(rect)
            .map_err(|e| SscError::Unavailable(format!("ScreenCaptureKit capture failed: {e}")))?;
        if img.as_ptr().is_null() {
            return Err(SscError::Unavailable("ScreenCaptureKit returned no image".into()));
        }
        // SAFETY: `as_ptr` is a live, retained CGImageRef owned by `img`, which outlives `cg`.
        let cg: &CGImage = unsafe { &*img.as_ptr().cast::<CGImage>() };
        let (width, height, gray) = cgimage_to_gray(cg)?;
        Ok(Frame { screen: window, width, height, gray })
    }
}

impl ColorSource for SckCapture {
    fn capture_color(&self, window: Rect, max_edge: u32) -> Result<RgbFrame> {
        check_capture_access(window)?;
        let rect =
            screencapturekit::cg::CGRect::new(window.x.into(), window.y.into(), window.w.into(), window.h.into());
        let img = SCScreenshotManager::capture_image_in_rect(rect)
            .map_err(|e| SscError::Unavailable(format!("ScreenCaptureKit capture failed: {e}")))?;
        if img.as_ptr().is_null() {
            return Err(SscError::Unavailable("ScreenCaptureKit returned no image".into()));
        }
        // SAFETY: `as_ptr` is a live, retained CGImageRef owned by `img`, which outlives `cg`.
        let cg: &CGImage = unsafe { &*img.as_ptr().cast::<CGImage>() };
        cgimage_to_rgb(cg, max_edge)
    }
}

/// Captures a screen rectangle with the deprecated `CGWindowListCreateImage`. Not used by
/// [`crate::platform_reader`] (which uses [`SckCapture`]); kept for the side-by-side
/// measurement in `real_capture_compare` and as a fallback should SCK regress.
pub struct CgCapture;

impl FrameSource for CgCapture {
    fn capture(&self, window: Rect) -> Result<Frame> {
        check_capture_access(window)?;
        let rect = CGRect {
            origin: CGPoint { x: window.x.into(), y: window.y.into() },
            size: CGSize { width: window.w.into(), height: window.h.into() },
        };
        #[allow(deprecated)]
        let img = CGWindowListCreateImage(
            rect,
            CGWindowListOption::OptionOnScreenOnly,
            kCGNullWindowID,
            CGWindowImageOption::Default,
        )
        .ok_or_else(|| SscError::Unavailable("CGWindowListCreateImage returned null".into()))?;
        let (width, height, gray) = cgimage_to_gray(&img)?;
        Ok(Frame { screen: window, width, height, gray })
    }
}

// ---------- OCR (Vision) ----------

use objc2::AnyThread;
use objc2::rc::autoreleasepool;
use objc2_foundation::{NSArray, NSDictionary};
use objc2_vision::{VNImageRequestHandler, VNRecognizeTextRequest, VNRequest, VNRequestTextRecognitionLevel};

/// Apple Vision `VNRecognizeTextRequest` (accurate level, language correction on).
pub struct VisionOcr;

fn gray_image(gray: &[u8], w: u32, h: u32) -> Option<objc2_core_foundation::CFRetained<CGImage>> {
    let data = CFData::from_bytes(gray);
    let provider = CGDataProvider::with_cf_data(Some(&data))?;
    let cs = CGColorSpace::new_device_gray()?;
    // SAFETY: provider holds w*h bytes of 8-bit gray, bytes_per_row = w; decode is null (allowed).
    unsafe {
        CGImage::new(
            w as usize,
            h as usize,
            8,
            8,
            w as usize,
            Some(&cs),
            CGBitmapInfo(CGImageAlphaInfo::None.0),
            Some(&provider),
            std::ptr::null(),
            false,
            CGColorRenderingIntent::RenderingIntentDefault,
        )
    }
}

impl OcrEngine for VisionOcr {
    fn recognize(&self, gray: &[u8], width: u32, height: u32) -> Result<Vec<OcrLine>> {
        if width == 0 || height == 0 || gray.len() < (width * height) as usize {
            return Err(SscError::Invalid("bad OCR image size".into()));
        }
        let img =
            gray_image(gray, width, height).ok_or_else(|| SscError::Unavailable("CGImageCreate failed".into()))?;
        autoreleasepool(|_| {
            let options = NSDictionary::new();
            // SAFETY: valid CGImage and empty options dictionary.
            let handler = unsafe {
                VNImageRequestHandler::initWithCGImage_options(VNImageRequestHandler::alloc(), &img, &options)
            };
            // SAFETY: plain init.
            let request = unsafe { VNRecognizeTextRequest::init(VNRecognizeTextRequest::alloc()) };
            request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
            request.setUsesLanguageCorrection(true);
            let as_request: &VNRequest = &request;
            handler
                .performRequests_error(&NSArray::from_slice(&[as_request]))
                .map_err(|e| SscError::Unavailable(format!("Vision OCR failed: {}", e.localizedDescription())))?;
            let mut out = Vec::new();
            for obs in request.results().unwrap_or_default().iter() {
                let Some(top) = obs.topCandidates(1).firstObject() else { continue };
                // SAFETY: plain getter. Normalised, origin bottom-left.
                let b = unsafe { obs.boundingBox() };
                let (fw, fh) = (f64::from(width), f64::from(height));
                let x = (b.origin.x * fw).clamp(0.0, fw) as u32;
                let y = ((1.0 - b.origin.y - b.size.height) * fh).clamp(0.0, fh) as u32;
                let w = (b.size.width * fw).ceil().clamp(0.0, fw - f64::from(x)) as u32;
                let h = (b.size.height * fh).ceil().clamp(0.0, fh - f64::from(y)) as u32;
                out.push(OcrLine { text: top.string().to_string(), bounds: PxRect { x, y, w, h } });
            }
            Ok(out)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb_render_scales_down_and_keeps_colour_values() {
        // 3200x40 gray image, left half black, right half white.
        let (w, h) = (3200u32, 40u32);
        let mut gray = vec![0u8; (w * h) as usize];
        for row in gray.chunks_exact_mut(w as usize) {
            row[(w / 2) as usize..].fill(255);
        }
        let img = gray_image(&gray, w, h).unwrap();
        let f = cgimage_to_rgb(&img, 1600).unwrap();
        assert_eq!((f.width, f.height), (1600, 20));
        assert_eq!(f.rgb.len(), 1600 * 20 * 3);
        let px = |x: usize, y: usize| &f.rgb[(y * 1600 + x) * 3..(y * 1600 + x) * 3 + 3];
        assert!(px(100, 10).iter().all(|&v| v < 10), "{:?}", px(100, 10));
        assert!(px(1500, 10).iter().all(|&v| v > 245), "{:?}", px(1500, 10));
        // Small images are not upscaled.
        let small = gray_image(&[128u8; 64 * 32], 64, 32).unwrap();
        let f = cgimage_to_rgb(&small, 1600).unwrap();
        assert_eq!((f.width, f.height), (64, 32));
    }
    use ssc_core::traits::ScreenSource;

    /// The OCR engine runs without any permission. A blank image yields no text (and no crash).
    #[test]
    fn vision_runs_on_blank_image() {
        let lines = VisionOcr.recognize(&vec![255u8; 200 * 50], 200, 50).unwrap();
        assert!(lines.is_empty(), "{lines:?}");
    }

    #[test]
    fn vision_rejects_bad_sizes() {
        assert!(VisionOcr.recognize(&[0u8; 10], 10, 10).is_err());
    }

    /// The conversion both capture paths share: a gray image round-trips byte for byte, with the
    /// top row first (CoreGraphics bitmap contexts are bottom-up in user space).
    #[test]
    fn cgimage_to_gray_round_trips_and_keeps_row_order() {
        let (w, h) = (7u32, 5u32);
        let src: Vec<u8> = (0..w * h).map(|i| (i * 7 % 256) as u8).collect();
        let img = gray_image(&src, w, h).unwrap();
        let (gw, gh, gray) = cgimage_to_gray(&img).unwrap();
        assert_eq!((gw, gh), (w, h));
        assert_eq!(gray, src);
    }

    /// RGBA input (the shape ScreenCaptureKit hands back) is reduced to luminance: white stays
    /// white, black stays black, and pure green is brighter than pure blue.
    #[test]
    fn cgimage_to_gray_converts_colour() {
        let px: [[u8; 4]; 4] = [[255, 255, 255, 255], [0, 0, 0, 255], [0, 255, 0, 255], [0, 0, 255, 255]];
        let bytes: Vec<u8> = px.iter().flatten().copied().collect();
        let data = CFData::from_bytes(&bytes);
        let provider = CGDataProvider::with_cf_data(Some(&data)).unwrap();
        let cs = CGColorSpace::new_device_rgb().unwrap();
        // SAFETY: 4x1 RGBA, 8 bpc, 32 bpp, 16 bytes per row; decode is null.
        let img = unsafe {
            CGImage::new(
                4,
                1,
                8,
                32,
                16,
                Some(&cs),
                CGBitmapInfo(CGImageAlphaInfo::NoneSkipLast.0),
                Some(&provider),
                std::ptr::null(),
                false,
                CGColorRenderingIntent::RenderingIntentDefault,
            )
        }
        .unwrap();
        let (w, h, g) = cgimage_to_gray(&img).unwrap();
        assert_eq!((w, h), (4, 1));
        assert!(g[0] > 250, "white -> {}", g[0]);
        assert!(g[1] < 5, "black -> {}", g[1]);
        assert!(g[2] > g[3], "green {} should be brighter than blue {}", g[2], g[3]);
    }

    #[test]
    fn capture_rejects_empty_rect() {
        let r = Rect { x: 0, y: 0, w: 0, h: 10 };
        assert!(matches!(SckCapture.capture(r), Err(SscError::Invalid(_))));
        assert!(matches!(CgCapture.capture(r), Err(SscError::Invalid(_))));
    }

    /// CPU time (user + system) of this process so far.
    fn cpu_time() -> std::time::Duration {
        // SAFETY: getrusage fills a zeroed struct.
        let mut ru: libc_rusage::rusage = unsafe { std::mem::zeroed() };
        unsafe { libc_rusage::getrusage(0, &mut ru) };
        let tv = |t: libc_rusage::timeval| {
            std::time::Duration::from_secs(t.tv_sec as u64) + std::time::Duration::from_micros(t.tv_usec as u64)
        };
        tv(ru.ru_utime) + tv(ru.ru_stime)
    }

    /// Minimal getrusage binding (avoids a libc dependency for one manual test).
    mod libc_rusage {
        #[repr(C)]
        #[derive(Clone, Copy)]
        pub struct timeval {
            pub tv_sec: i64,
            pub tv_usec: i32,
        }
        #[repr(C)]
        pub struct rusage {
            pub ru_utime: timeval,
            pub ru_stime: timeval,
            pub rest: [i64; 14],
        }
        unsafe extern "C" {
            pub fn getrusage(who: i32, usage: *mut rusage) -> i32;
        }
    }

    /// *Manual*: needs Screen Recording for the terminal running the test. Captures the same
    /// 1200x800-point region with ScreenCaptureKit and with CGWindowListCreateImage and prints
    /// wall time and process CPU time per capture (incl. gray conversion). Run with
    /// `cargo test -p ssc-screen --release -- --ignored --nocapture real_capture_compare`.
    #[test]
    #[ignore = "manual: needs macOS Screen Recording permission"]
    fn real_capture_compare() {
        assert!(CGPreflightScreenCaptureAccess(), "grant Screen Recording to the terminal running the test");
        let rect = Rect { x: 0, y: 0, w: 1200, h: 800 };
        let sources: [(&str, &dyn FrameSource); 2] = [("sck", &SckCapture), ("cg", &CgCapture)];
        for (name, src) in sources {
            let t = std::time::Instant::now();
            let first = src.capture(rect).unwrap();
            eprintln!("{name}: first capture {:?} -> {}x{}", t.elapsed(), first.width, first.height);
            assert!(first.width >= 1200 && first.height >= 800);
            assert!(first.gray.iter().any(|&b| b != first.gray[0]), "{name}: frame is uniform (permission?)");
            let n = 10;
            let (c0, t0) = (cpu_time(), std::time::Instant::now());
            for _ in 0..n {
                src.capture(rect).unwrap();
            }
            let (wall, cpu) = (t0.elapsed() / n, (cpu_time() - c0) / n);
            eprintln!("{name}: steady state per capture: wall {wall:?}, process cpu {cpu:?}");
        }
    }

    /// *Manual*: needs the Accessibility (and for OCR, Screen Recording) permission and a real
    /// focused window such as Chrome or Zoom. Run with
    /// `cargo test -p ssc-screen -- --ignored --nocapture real_focused_window`.
    #[test]
    #[ignore = "manual: needs macOS Accessibility permission and a real focused window"]
    fn real_focused_window() {
        assert!(AxReader::is_trusted(), "grant Accessibility to the terminal running the test");
        let reader = crate::platform_reader().unwrap();
        for _ in 0..3 {
            let t = std::time::Instant::now();
            let s = reader.snapshot().unwrap();
            eprintln!(
                "{:?} app={:?} title={:?} nodes={} source={:?}\n{}",
                t.elapsed(),
                s.app,
                s.window_title,
                s.nodes.len(),
                s.source,
                s.text().chars().take(1500).collect::<String>()
            );
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }
}
