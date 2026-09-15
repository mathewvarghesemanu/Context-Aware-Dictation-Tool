//! Capture of the text surrounding the caret in the focused application, used
//! to give the post-processing LLM enough context to decide casing,
//! capitalization and whether the dictated text continues an existing sentence.
//!
//! Reads the characters immediately before and after the insertion point from
//! the focused UI element. Cheap, exact, works with any text model, and needs
//! only the Accessibility permission Handy already requires for its shortcuts.
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

/// A snapshot of what surrounds the insertion point when dictation ended.
#[derive(Debug, Clone, Default)]
pub struct CaretContext {
    /// Text immediately before the caret (up to [`TEXT_WINDOW`] characters).
    pub before: String,
    /// Text immediately after the caret (up to [`TEXT_WINDOW`] characters).
    pub after: String,
    /// Human-readable name of the focused application, when it can be resolved.
    pub app_name: Option<String>,
}

impl CaretContext {
    /// True when nothing usable was captured, so the context block should be
    /// left out of the prompt entirely.
    pub fn is_empty(&self) -> bool {
        self.before.trim().is_empty() && self.after.trim().is_empty()
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
/// Runs synchronously on the caller's thread; the Accessibility calls are fast
/// enough not to be noticeable next to the transcription that follows.
pub fn capture_for_dictation() {
    // Secure input means a password field (or Terminal's secure entry) has the
    // keyboard. Never read text or pixels in that state.
    if crate::secure_input::is_enabled_now() {
        debug!("Caret context capture skipped: secure input is active");
        clear();
        return;
    }

    let captured = platform::capture();

    match &captured {
        Some(context) => debug!(
            "Captured caret context (app: {:?}, before: '{}', after: '{}')",
            context.app_name,
            crate::utils::redact_text(&context.before),
            crate::utils::redact_text(&context.after)
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

#[cfg(target_os = "macos")]
mod platform {
    use super::{head, tail, CaretContext, TEXT_WINDOW};
    use log::debug;
    use std::ffi::CString;
    use std::os::raw::{c_char, c_void};

    type CFTypeRef = *const c_void;
    type CFStringRef = *const c_void;
    type CFAllocatorRef = *const c_void;
    type AXUIElementRef = *const c_void;
    type AXValueRef = *const c_void;
    type CFIndex = isize;

    const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
    const AX_VALUE_TYPE_CF_RANGE: u32 = 4;
    const AX_ERROR_SUCCESS: i32 = 0;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct CFRange {
        location: CFIndex,
        length: CFIndex,
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

    pub fn capture() -> Option<CaretContext> {
        if !unsafe { AXIsProcessTrusted() } {
            debug!("Caret context capture skipped: Accessibility permission not granted");
            return None;
        }

        let system_wide = CFRef::new(unsafe { AXUIElementCreateSystemWide() })?;
        let focused = copy_attribute(system_wide.get(), "AXFocusedUIElement")?;

        // Password fields and anything else marked secure are never read.
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

        if let Some(range) = selected_range(focused.get()) {
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
    pub fn capture() -> Option<CaretContext> {
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
