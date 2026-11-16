//! macOS backend: AXUIElement tree walk, CoreGraphics window capture, Apple Vision OCR.
//!
//! Needs the Accessibility permission (AX) and, for OCR only, Screen Recording. Without them the
//! calls return [`SscError::Unavailable`]; nothing here prompts the user.
#![allow(unsafe_code)] // FFI to ApplicationServices, CoreGraphics and Vision.

use crate::ocr::{Frame, FrameSource, OcrEngine, OcrLine, PxRect};
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
        let name = CFString::new(name);
        let mut v: CFTypeRef = std::ptr::null();
        // SAFETY: valid element and attribute; `v` receives a +1 reference on success.
        let err = unsafe { AXUIElementCopyAttributeValue(self.0, name.as_concrete_TypeRef(), &mut v) };
        if err != kAXErrorSuccess || v.is_null() {
            return None;
        }
        // SAFETY: Copy rule, +1.
        Some(unsafe { CFType::wrap_under_create_rule(v) })
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

impl Default for AxReader {
    fn default() -> Self {
        Self::new()
    }
}

impl AxBackend for AxReader {
    fn read_focused(&self, limits: &WalkLimits) -> Result<WindowRead> {
        if !Self::is_trusted() {
            return Err(SscError::Unavailable("macOS Accessibility permission not granted".into()));
        }
        // SAFETY: Create rule, +1.
        let sys = AxEl::from_create(unsafe { AXUIElementCreateSystemWide() })
            .ok_or_else(|| SscError::Unavailable("AXUIElementCreateSystemWide failed".into()))?;
        // A hung app must not stall the walk: cap every AX message (global when set on system-wide).
        // SAFETY: valid element.
        unsafe { AXUIElementSetMessagingTimeout(sys.0, limits.budget.as_secs_f32().max(0.02)) };
        let app = sys
            .element(kAXFocusedApplicationAttribute)
            .ok_or_else(|| SscError::Unavailable("no focused application".into()))?;
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

// ---------- capture (CoreGraphics) ----------

use objc2_core_foundation::{CFData, CGPoint, CGRect, CGSize};
#[allow(deprecated)] // CGWindowListCreateImage, see CgCapture.
use objc2_core_graphics::{
    CGBitmapContextCreate, CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGContext, CGDataProvider, CGImage,
    CGImageAlphaInfo, CGPreflightScreenCaptureAccess, CGWindowImageOption, CGWindowListCreateImage, CGWindowListOption,
    kCGNullWindowID,
};

/// Captures a screen rectangle with `CGWindowListCreateImage` (needs Screen Recording).
pub struct CgCapture;

impl FrameSource for CgCapture {
    fn capture(&self, window: Rect) -> Result<Frame> {
        if !CGPreflightScreenCaptureAccess() {
            return Err(SscError::Unavailable("macOS Screen Recording permission not granted".into()));
        }
        let rect = CGRect {
            origin: CGPoint { x: window.x.into(), y: window.y.into() },
            size: CGSize { width: window.w.into(), height: window.h.into() },
        };
        // Deprecated in favour of ScreenCaptureKit (async, macOS 14+); still functional and far
        // simpler for one synchronous grab.
        #[allow(deprecated)]
        let img = CGWindowListCreateImage(
            rect,
            CGWindowListOption::OptionOnScreenOnly,
            kCGNullWindowID,
            CGWindowImageOption::Default,
        )
        .ok_or_else(|| SscError::Unavailable("CGWindowListCreateImage returned null".into()))?;
        let (w, h) = (CGImage::width(Some(&img)), CGImage::height(Some(&img)));
        if w == 0 || h == 0 {
            return Err(SscError::Unavailable("empty capture".into()));
        }
        // Draw into an 8-bit gray bitmap we own: CoreGraphics converts whatever the source format is.
        let mut gray = vec![0u8; w * h];
        let cs = CGColorSpace::new_device_gray();
        // SAFETY: `gray` is w*h bytes, bytes_per_row = w, 8 bpc gray, no alpha; it outlives the context.
        let ctx = unsafe {
            CGBitmapContextCreate(gray.as_mut_ptr().cast(), w, h, 8, w, cs.as_deref(), CGImageAlphaInfo::None.0)
        }
        .ok_or_else(|| SscError::Unavailable("CGBitmapContextCreate failed".into()))?;
        let full = CGRect { origin: CGPoint { x: 0.0, y: 0.0 }, size: CGSize { width: w as f64, height: h as f64 } };
        CGContext::draw_image(Some(&ctx), full, Some(&img));
        drop(ctx);
        Ok(Frame { screen: window, width: w as u32, height: h as u32, gray })
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
