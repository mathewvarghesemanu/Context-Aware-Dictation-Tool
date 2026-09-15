//! Capture of the text surrounding the caret in the focused application, used
//! to give the post-processing LLM enough context to decide casing,
//! capitalization and whether the dictated text continues an existing sentence.
//!
//! Two capture strategies, in order:
//!
//! 1. **Accessibility text** (preferred). Reads the characters immediately
//!    before and after the insertion point from the focused UI element. Cheap,
//!    exact, works with any text model, and needs only the Accessibility
//!    permission Handy already requires for its shortcuts.
//! 2. **Screenshot fallback** (opt-in, separate setting). When the focused
//!    element exposes no usable text — canvas-drawn editors, some Electron and
//!    terminal apps — grab a small square around the caret and hand it to the
//!    model as an image. Requires the Screen Recording permission and a
//!    vision-capable model, so it is off by default and strictly a fallback.
//!
//! Only macOS is implemented today; the other platforms return `None` (Windows
//! UI Automation and AT-SPI are the natural follow-ups).
//!
//! Privacy: capture is skipped entirely while secure input is active or when the
//! focused element is a secure text field, the captured text is truncated hard,
//! it is never persisted to history, and it is only ever logged through
//! [`crate::utils::redact_text`].

use log::debug;
use std::sync::Mutex;

/// Characters captured on each side of the caret. Enough for the model to see
/// the sentence in progress without turning the prompt into a document dump.
const TEXT_WINDOW: usize = 200;

/// Side length, in points, of the square grabbed around the caret.
const SCREENSHOT_SIDE: f64 = 300.0;

/// A snapshot of what surrounds the insertion point when dictation ended.
#[derive(Debug, Clone, Default)]
pub struct CaretContext {
    /// Text immediately before the caret (up to [`TEXT_WINDOW`] characters).
    pub before: String,
    /// Text immediately after the caret (up to [`TEXT_WINDOW`] characters).
    pub after: String,
    /// Human-readable name of the focused application, when it can be resolved.
    pub app_name: Option<String>,
    /// PNG screenshot of the area around the caret, base64-encoded. Only set
    /// when the text capture came up empty and the fallback is enabled.
    pub screenshot_png_base64: Option<String>,
}

impl CaretContext {
    /// True when nothing usable was captured, so the context block should be
    /// left out of the prompt entirely.
    pub fn is_empty(&self) -> bool {
        self.before.trim().is_empty()
            && self.after.trim().is_empty()
            && self.screenshot_png_base64.is_none()
    }

    pub fn has_text(&self) -> bool {
        !self.before.trim().is_empty() || !self.after.trim().is_empty()
    }
}

/// Holds the context captured when a dictation ends until the post-processing
/// step consumes it.
///
/// A process-wide slot rather than Tauri state: there is only ever one
/// recording in flight, and the capture happens on the shortcut thread, outside
/// any per-transcription state. A new capture always replaces the previous one,
/// so an abandoned recording cannot leak its context into the next dictation.
static PENDING: Mutex<Option<CaretContext>> = Mutex::new(None);

/// Capture the caret context for the dictation that is finishing.
///
/// Called when recording stops rather than when it starts, because the caret is
/// free to move while the user is talking and it is the position at stop that
/// decides where the transcript lands.
///
/// `want_text` and `want_screenshot` come straight from the two user-facing
/// settings. Runs synchronously on the caller's thread; the Accessibility calls
/// are fast, but the screenshot path can take a few tens of milliseconds, which
/// is why it stays opt-in.
pub fn capture_for_dictation(want_text: bool, want_screenshot: bool) {
    if !want_text && !want_screenshot {
        clear();
        return;
    }

    // Secure input means a password field (or Terminal's secure entry) has the
    // keyboard. Never read text or pixels in that state.
    if crate::secure_input::is_enabled_now() {
        debug!("Caret context capture skipped: secure input is active");
        clear();
        return;
    }

    let captured = platform::capture(want_text, want_screenshot);

    match &captured {
        Some(context) => debug!(
            "Captured caret context (app: {:?}, before: '{}', after: '{}', screenshot: {})",
            context.app_name,
            crate::utils::redact_text(&context.before),
            crate::utils::redact_text(&context.after),
            context.screenshot_png_base64.is_some()
        ),
        None => debug!("No caret context available for this dictation"),
    }

    if let Ok(mut pending) = PENDING.lock() {
        *pending = captured;
    }
}

/// Take the context captured for the current dictation, leaving the slot empty
/// so it can never be reused by a later one.
pub fn take() -> Option<CaretContext> {
    PENDING.lock().ok().and_then(|mut pending| pending.take())
}

/// Drop any pending context (cancelled dictation, capture disabled).
pub fn clear() {
    if let Ok(mut pending) = PENDING.lock() {
        *pending = None;
    }
}

/// Keep the trailing `max` characters of `text` — the part nearest the caret.
fn tail(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_string();
    }
    chars[chars.len() - max..].iter().collect()
}

/// Keep the leading `max` characters of `text` — the part nearest the caret.
fn head(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_string();
    }
    chars[..max].iter().collect()
}

/// Minimal base64 encoder for the screenshot data URI. Hand-rolled to keep a
/// one-call-site dependency out of the tree.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(triple >> 18) as usize & 0x3f] as char);
        out.push(ALPHABET[(triple >> 12) as usize & 0x3f] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(triple >> 6) as usize & 0x3f] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[triple as usize & 0x3f] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(target_os = "macos")]
mod platform {
    use super::{base64_encode, head, tail, CaretContext, SCREENSHOT_SIDE, TEXT_WINDOW};
    use log::debug;
    use std::ffi::CString;
    use std::os::raw::{c_char, c_void};

    type CFTypeRef = *const c_void;
    type CFStringRef = *const c_void;
    type CFAllocatorRef = *const c_void;
    type AXUIElementRef = *const c_void;
    type AXValueRef = *const c_void;
    type CGImageRef = *const c_void;
    type CFIndex = isize;

    const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
    const AX_VALUE_TYPE_CG_RECT: u32 = 3;
    const AX_VALUE_TYPE_CF_RANGE: u32 = 4;
    const AX_ERROR_SUCCESS: i32 = 0;
    /// kCGWindowListOptionOnScreenOnly
    const CG_WINDOW_LIST_ON_SCREEN_ONLY: u32 = 1 << 0;
    /// kCGNullWindowID
    const CG_NULL_WINDOW_ID: u32 = 0;
    /// kCGWindowImageDefault
    const CG_WINDOW_IMAGE_DEFAULT: u32 = 0;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct CFRange {
        location: CFIndex,
        length: CFIndex,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct CGPoint {
        x: f64,
        y: f64,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct CGSize {
        width: f64,
        height: f64,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct CGRect {
        origin: CGPoint,
        size: CGSize,
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRelease(cf: CFTypeRef);
        fn CFStringCreateWithCString(
            alloc: CFAllocatorRef,
            c_str: *const c_char,
            encoding: u32,
        ) -> CFStringRef;
        fn CFStringGetLength(string: CFStringRef) -> CFIndex;
        fn CFStringGetCString(
            string: CFStringRef,
            buffer: *mut c_char,
            buffer_size: CFIndex,
            encoding: u32,
        ) -> bool;
        fn CFDataCreateMutable(alloc: CFAllocatorRef, capacity: CFIndex) -> *mut c_void;
        fn CFDataGetLength(data: CFTypeRef) -> CFIndex;
        fn CFDataGetBytePtr(data: CFTypeRef) -> *const u8;
    }

    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXIsProcessTrusted() -> bool;
        fn AXUIElementCreateSystemWide() -> AXUIElementRef;
        fn AXUIElementCopyAttributeValue(
            element: AXUIElementRef,
            attribute: CFStringRef,
            value: *mut CFTypeRef,
        ) -> i32;
        fn AXUIElementCopyParameterizedAttributeValue(
            element: AXUIElementRef,
            attribute: CFStringRef,
            parameter: CFTypeRef,
            value: *mut CFTypeRef,
        ) -> i32;
        fn AXValueCreate(value_type: u32, value_ptr: *const c_void) -> AXValueRef;
        fn AXValueGetValue(value: AXValueRef, value_type: u32, value_ptr: *mut c_void) -> bool;
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGWindowListCreateImage(
            screen_bounds: CGRect,
            list_option: u32,
            window_id: u32,
            image_option: u32,
        ) -> CGImageRef;
        fn CGImageRelease(image: CGImageRef);
    }

    #[link(name = "ImageIO", kind = "framework")]
    unsafe extern "C" {
        fn CGImageDestinationCreateWithData(
            data: *mut c_void,
            uti_type: CFStringRef,
            count: CFIndex,
            options: *const c_void,
        ) -> *mut c_void;
        fn CGImageDestinationAddImage(
            destination: *mut c_void,
            image: CGImageRef,
            properties: *const c_void,
        );
        fn CGImageDestinationFinalize(destination: *mut c_void) -> bool;
    }

    /// Owns a CoreFoundation reference and releases it on drop, so the many
    /// early returns below cannot leak.
    struct CFRef(CFTypeRef);

    impl CFRef {
        fn new(value: CFTypeRef) -> Option<Self> {
            if value.is_null() {
                None
            } else {
                Some(CFRef(value))
            }
        }

        fn get(&self) -> CFTypeRef {
            self.0
        }
    }

    impl Drop for CFRef {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { CFRelease(self.0) };
            }
        }
    }

    /// Build a CFString from a static ASCII attribute name. Returns `None` only
    /// if CoreFoundation refuses the allocation.
    fn cfstring(value: &str) -> Option<CFRef> {
        let c_string = CString::new(value).ok()?;
        let raw = unsafe {
            CFStringCreateWithCString(
                std::ptr::null(),
                c_string.as_ptr(),
                K_CF_STRING_ENCODING_UTF8,
            )
        };
        CFRef::new(raw)
    }

    fn cfstring_to_string(value: CFStringRef) -> Option<String> {
        if value.is_null() {
            return None;
        }
        // Worst case UTF-8 is 3 bytes per UTF-16 unit (surrogate pairs cost 4
        // bytes across 2 units), plus the terminator.
        let length = unsafe { CFStringGetLength(value) };
        let capacity = (length * 3 + 1).max(1) as usize;
        let mut buffer = vec![0i8; capacity];
        let ok = unsafe {
            CFStringGetCString(
                value,
                buffer.as_mut_ptr(),
                capacity as CFIndex,
                K_CF_STRING_ENCODING_UTF8,
            )
        };
        if !ok {
            return None;
        }
        let bytes: Vec<u8> = buffer
            .iter()
            .take_while(|byte| **byte != 0)
            .map(|byte| *byte as u8)
            .collect();
        String::from_utf8(bytes).ok()
    }

    fn copy_attribute(element: AXUIElementRef, attribute: &str) -> Option<CFRef> {
        let name = cfstring(attribute)?;
        let mut value: CFTypeRef = std::ptr::null();
        let result = unsafe { AXUIElementCopyAttributeValue(element, name.get(), &mut value) };
        if result != AX_ERROR_SUCCESS {
            return None;
        }
        CFRef::new(value)
    }

    fn copy_parameterized_attribute(
        element: AXUIElementRef,
        attribute: &str,
        parameter: CFTypeRef,
    ) -> Option<CFRef> {
        let name = cfstring(attribute)?;
        let mut value: CFTypeRef = std::ptr::null();
        let result = unsafe {
            AXUIElementCopyParameterizedAttributeValue(element, name.get(), parameter, &mut value)
        };
        if result != AX_ERROR_SUCCESS {
            return None;
        }
        CFRef::new(value)
    }

    fn copy_string_attribute(element: AXUIElementRef, attribute: &str) -> Option<String> {
        let value = copy_attribute(element, attribute)?;
        cfstring_to_string(value.get())
    }

    /// Read the caret position from the focused element's selected range.
    /// A collapsed selection (length 0) is the common case; when the user has
    /// text selected we treat the selection start as the caret and skip over
    /// the selection for the trailing context.
    fn selected_range(element: AXUIElementRef) -> Option<CFRange> {
        let value = copy_attribute(element, "AXSelectedTextRange")?;
        let mut range = CFRange::default();
        let ok = unsafe {
            AXValueGetValue(
                value.get(),
                AX_VALUE_TYPE_CF_RANGE,
                &mut range as *mut CFRange as *mut c_void,
            )
        };
        if ok {
            Some(range)
        } else {
            None
        }
    }

    /// Ask the element for a specific substring. Preferred over reading the
    /// whole field, which can be a multi-megabyte document.
    fn string_for_range(
        element: AXUIElementRef,
        location: CFIndex,
        length: CFIndex,
    ) -> Option<String> {
        if length <= 0 {
            return None;
        }
        let range = CFRange { location, length };
        let range_value = CFRef::new(unsafe {
            AXValueCreate(
                AX_VALUE_TYPE_CF_RANGE,
                &range as *const CFRange as *const c_void,
            )
        })?;
        let value = copy_parameterized_attribute(element, "AXStringForRange", range_value.get())?;
        cfstring_to_string(value.get())
    }

    /// Fallback for elements that expose `AXValue` but not `AXStringForRange`.
    /// Offsets are UTF-16 units, so slice in UTF-16 space to stay correct for
    /// emoji and other astral characters.
    fn slice_full_value(element: AXUIElementRef, caret: CFIndex) -> Option<(String, String)> {
        let full = copy_string_attribute(element, "AXValue")?;
        let units: Vec<u16> = full.encode_utf16().collect();
        let caret = (caret.max(0) as usize).min(units.len());
        let start = caret.saturating_sub(TEXT_WINDOW);
        let end = (caret + TEXT_WINDOW).min(units.len());
        Some((
            String::from_utf16_lossy(&units[start..caret]),
            String::from_utf16_lossy(&units[caret..end]),
        ))
    }

    fn focused_app_name(system_wide: AXUIElementRef) -> Option<String> {
        let app = copy_attribute(system_wide, "AXFocusedApplication")?;
        copy_string_attribute(app.get(), "AXTitle")
    }

    /// Screen bounds of the caret, used to centre the screenshot.
    fn caret_bounds(element: AXUIElementRef, caret: CFIndex) -> Option<CGRect> {
        let range = CFRange {
            location: caret,
            length: 1,
        };
        let range_value = CFRef::new(unsafe {
            AXValueCreate(
                AX_VALUE_TYPE_CF_RANGE,
                &range as *const CFRange as *const c_void,
            )
        })?;
        let value = copy_parameterized_attribute(element, "AXBoundsForRange", range_value.get())?;
        let mut rect = CGRect::default();
        let ok = unsafe {
            AXValueGetValue(
                value.get(),
                AX_VALUE_TYPE_CG_RECT,
                &mut rect as *mut CGRect as *mut c_void,
            )
        };
        if ok {
            Some(rect)
        } else {
            None
        }
    }

    /// Bounds of the focused element itself — the fallback anchor when the
    /// element cannot report a caret rectangle.
    fn element_bounds(element: AXUIElementRef) -> Option<CGRect> {
        let position_value = copy_attribute(element, "AXPosition")?;
        let size_value = copy_attribute(element, "AXSize")?;
        let mut origin = CGPoint::default();
        let mut size = CGSize::default();
        let got_origin = unsafe {
            AXValueGetValue(
                position_value.get(),
                1, // kAXValueTypeCGPoint
                &mut origin as *mut CGPoint as *mut c_void,
            )
        };
        let got_size = unsafe {
            AXValueGetValue(
                size_value.get(),
                2, // kAXValueTypeCGSize
                &mut size as *mut CGSize as *mut c_void,
            )
        };
        if got_origin && got_size {
            Some(CGRect { origin, size })
        } else {
            None
        }
    }

    fn encode_png(image: CGImageRef) -> Option<String> {
        let data = CFRef::new(unsafe { CFDataCreateMutable(std::ptr::null(), 0) } as CFTypeRef)?;
        let png_type = cfstring("public.png")?;
        let destination = unsafe {
            CGImageDestinationCreateWithData(
                data.get() as *mut c_void,
                png_type.get(),
                1,
                std::ptr::null(),
            )
        };
        let destination = CFRef::new(destination as CFTypeRef)?;
        unsafe {
            CGImageDestinationAddImage(destination.get() as *mut c_void, image, std::ptr::null());
            if !CGImageDestinationFinalize(destination.get() as *mut c_void) {
                return None;
            }
        }
        let length = unsafe { CFDataGetLength(data.get()) };
        if length <= 0 {
            return None;
        }
        let bytes =
            unsafe { std::slice::from_raw_parts(CFDataGetBytePtr(data.get()), length as usize) };
        Some(base64_encode(bytes))
    }

    /// Grab a [`SCREENSHOT_SIDE`]-point square centred on `anchor`.
    ///
    /// Both capture paths take global screen coordinates, so this works across
    /// multiple displays without any per-display conversion.
    fn capture_square(anchor: CGRect) -> Option<String> {
        // Rate-limited preflight: the TCC APIs are WindowServer RPCs and this
        // process holds an active CGEventTap, so they are never called in a
        // loop. See `crate::screen_permission`.
        if !crate::screen_permission::is_granted() {
            debug!("Screenshot context skipped: Screen Recording permission not granted");
            return None;
        }

        let center_x = anchor.origin.x + anchor.size.width / 2.0;
        let center_y = anchor.origin.y + anchor.size.height / 2.0;
        let rect = CGRect {
            origin: CGPoint {
                x: center_x - SCREENSHOT_SIDE / 2.0,
                y: center_y - SCREENSHOT_SIDE / 2.0,
            },
            size: CGSize {
                width: SCREENSHOT_SIDE,
                height: SCREENSHOT_SIDE,
            },
        };

        // ScreenCaptureKit first: on macOS 15+ it is the only API that still
        // returns real pixels. The CGWindowList path stays for 13/14, where
        // SCScreenshotManager has no rect-based entry point.
        if let Some(encoded) = screen_capture_kit::capture_rect(rect) {
            return Some(encoded);
        }

        capture_square_legacy(rect)
    }

    /// Pre-macOS 15 capture through `CGWindowListCreateImage`.
    ///
    /// Deprecated since macOS 14 and gutted in 15 (it returns a null or empty
    /// image, and calling it re-triggers the weekly Screen Recording nag), so
    /// it only runs when ScreenCaptureKit is unavailable or fails.
    fn capture_square_legacy(rect: CGRect) -> Option<String> {
        let image = unsafe {
            CGWindowListCreateImage(
                rect,
                CG_WINDOW_LIST_ON_SCREEN_ONLY,
                CG_NULL_WINDOW_ID,
                CG_WINDOW_IMAGE_DEFAULT,
            )
        };
        if image.is_null() {
            debug!("Screenshot context skipped: display capture returned no image");
            return None;
        }
        let encoded = encode_png(image);
        unsafe { CGImageRelease(image) };
        encoded
    }

    /// macOS 15+ capture through `SCScreenshotManager`.
    mod screen_capture_kit {
        use super::{encode_png, CGRect};
        use block2::RcBlock;
        use log::debug;
        use objc2::runtime::AnyClass;
        use objc2::sel;
        use objc2_core_graphics::CGImage;
        use objc2_foundation::NSError;
        use objc2_screen_capture_kit::SCScreenshotManager;
        use std::os::raw::c_void;
        use std::sync::mpsc;
        use std::time::Duration;

        /// How long to wait for the capture callback before giving up. The
        /// call is on the dictation's critical path, so a wedged WindowServer
        /// must not hold up the transcription.
        const CAPTURE_TIMEOUT: Duration = Duration::from_secs(2);

        /// True when `captureImageInRect:completionHandler:` exists, i.e. this
        /// is macOS 15 or newer. Checked through the runtime rather than a
        /// version string so a missing class degrades instead of crashing.
        fn is_available() -> bool {
            let Some(class) = AnyClass::get(c"SCScreenshotManager") else {
                return false;
            };
            class
                .class_method(sel!(captureImageInRect:completionHandler:))
                .is_some()
        }

        /// Capture `rect` (global display coordinates, top-left origin) and
        /// return it as base64-encoded PNG.
        pub fn capture_rect(rect: CGRect) -> Option<String> {
            if !is_available() {
                return None;
            }

            // Rebuild the rect in the objc2 crate's own type rather than
            // transmuting our local declaration of it.
            let rect = objc2_core_foundation::CGRect::new(
                objc2_core_foundation::CGPoint::new(rect.origin.x, rect.origin.y),
                objc2_core_foundation::CGSize::new(rect.size.width, rect.size.height),
            );

            let (tx, rx) = mpsc::channel::<Option<String>>();
            let handler = RcBlock::new(move |image: *mut CGImage, error: *mut NSError| {
                // Encode inside the callback: the image is only guaranteed to
                // be alive for the duration of this call.
                let encoded = if image.is_null() {
                    if !error.is_null() {
                        debug!("ScreenCaptureKit capture failed: {}", unsafe {
                            (*error).localizedDescription()
                        });
                    }
                    None
                } else {
                    encode_png(image as *const c_void)
                };
                let _ = tx.send(encoded);
            });

            unsafe {
                SCScreenshotManager::captureImageInRect_completionHandler(rect, Some(&handler));
            }

            match rx.recv_timeout(CAPTURE_TIMEOUT) {
                Ok(encoded) => encoded,
                Err(_) => {
                    debug!("ScreenCaptureKit capture timed out");
                    None
                }
            }
        }
    }

    pub fn capture(want_text: bool, want_screenshot: bool) -> Option<CaretContext> {
        if !unsafe { AXIsProcessTrusted() } {
            debug!("Caret context capture skipped: Accessibility permission not granted");
            return None;
        }

        let system_wide = CFRef::new(unsafe { AXUIElementCreateSystemWide() })?;
        let focused = copy_attribute(system_wide.get(), "AXFocusedUIElement")?;

        // Password fields and anything else marked secure are never read, not
        // even for the screenshot path.
        if let Some(subrole) = copy_string_attribute(focused.get(), "AXSubrole") {
            if subrole == "AXSecureTextField" {
                debug!("Caret context capture skipped: focused element is a secure text field");
                return None;
            }
        }

        let mut context = CaretContext {
            app_name: focused_app_name(system_wide.get()),
            ..Default::default()
        };

        let range = selected_range(focused.get());

        if want_text {
            if let Some(range) = range {
                let caret = range.location.max(0);
                let before_start = (caret - TEXT_WINDOW as CFIndex).max(0);
                let before = string_for_range(focused.get(), before_start, caret - before_start);
                let after = string_for_range(
                    focused.get(),
                    caret + range.length.max(0),
                    TEXT_WINDOW as CFIndex,
                );

                match (before, after) {
                    (None, None) => {
                        // Element has a caret but no AXStringForRange support.
                        if let Some((before, after)) = slice_full_value(focused.get(), caret) {
                            context.before = tail(&before, TEXT_WINDOW);
                            context.after = head(&after, TEXT_WINDOW);
                        }
                    }
                    (before, after) => {
                        context.before = tail(&before.unwrap_or_default(), TEXT_WINDOW);
                        context.after = head(&after.unwrap_or_default(), TEXT_WINDOW);
                    }
                }
            }
        }

        // The screenshot is a fallback, not an addition: it only runs when the
        // accessibility text came up empty.
        if want_screenshot && !context.has_text() {
            let anchor = range
                .and_then(|range| caret_bounds(focused.get(), range.location.max(0)))
                .filter(|rect| rect.size.width.is_finite() && rect.size.height.is_finite())
                .or_else(|| element_bounds(focused.get()));

            if let Some(anchor) = anchor {
                context.screenshot_png_base64 = capture_square(anchor);
            } else {
                debug!("Screenshot context skipped: no caret or element bounds available");
            }
        }

        if context.is_empty() {
            None
        } else {
            Some(context)
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use super::CaretContext;

    /// Windows (UI Automation) and Linux (AT-SPI) are not implemented yet, so
    /// the feature degrades to "no context" rather than failing the dictation.
    pub fn capture(_want_text: bool, _want_screenshot: bool) -> Option<CaretContext> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_keeps_the_end_nearest_the_caret() {
        assert_eq!(tail("hello world", 5), "world");
        assert_eq!(tail("hi", 5), "hi");
    }

    #[test]
    fn head_keeps_the_start_nearest_the_caret() {
        assert_eq!(head("hello world", 5), "hello");
        assert_eq!(head("hi", 5), "hi");
    }

    #[test]
    fn truncation_is_character_aware() {
        // Byte slicing would panic or split these; character slicing must not.
        assert_eq!(tail("añodèé", 3), "dèé");
        assert_eq!(head("añodèé", 3), "año");
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn empty_context_is_detected() {
        assert!(CaretContext::default().is_empty());
        assert!(CaretContext {
            before: "   ".to_string(),
            ..Default::default()
        }
        .is_empty());
        assert!(!CaretContext {
            before: "Hello".to_string(),
            ..Default::default()
        }
        .is_empty());
        assert!(!CaretContext {
            screenshot_png_base64: Some("abc".to_string()),
            ..Default::default()
        }
        .is_empty());
    }

    #[test]
    fn pending_context_is_taken_only_once() {
        clear();
        if let Ok(mut pending) = PENDING.lock() {
            *pending = Some(CaretContext {
                before: "Hello ".to_string(),
                ..Default::default()
            });
        }
        assert!(take().is_some());
        assert!(take().is_none());
    }
}
