//! Screen Recording (TCC) permission handling for the screenshot context
//! fallback.
//!
//! This lives here rather than going through `tauri-plugin-macos-permissions`
//! because the plugin's commands are `async fn`s that call straight into
//! CoreGraphics from a tokio worker, and getting this wrong on macOS does not
//! fail quietly — it takes the whole machine's input down with it.
//!
//! Two rules the rest of the app depends on:
//!
//! 1. **Never poll the TCC APIs.** `CGPreflightScreenCaptureAccess` is a
//!    WindowServer RPC. Handy holds an active, head-inserted CGEventTap for
//!    global shortcuts, so this process sits in the delivery path of every key
//!    and mouse event in the session; a stream of WindowServer RPCs alongside
//!    that path stalls input system-wide and leaks kernel IPC vouchers
//!    (cjpais/Handy#1827 is the same failure from a different call site).
//!    [`check_screen_capture_permission`] is therefore single-flight and
//!    rate-limited, and the UI calls it on discrete events only.
//! 2. **Prompt on the main thread with the tap removed.**
//!    `CGRequestScreenCaptureAccess` blocks its caller until the user answers
//!    the system prompt and wants the main run loop. It runs on the main thread
//!    here, bracketed by a full teardown/rebuild of the event tap so that a
//!    blocked WindowServer cannot take the keyboard down with it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use log::{debug, warn};
use tauri::AppHandle;

/// Shortest gap between two live preflight calls. Repeats inside the window
/// are answered from the cache below, so a chatty UI cannot turn into a
/// WindowServer RPC storm.
const PREFLIGHT_MIN_INTERVAL: Duration = Duration::from_millis(750);

/// Last live preflight result and when it was taken.
static PREFLIGHT_CACHE: Mutex<Option<(Instant, bool)>> = Mutex::new(None);

/// Set while a prompt is in flight, so a second click cannot raise a second
/// prompt (or a second tap teardown) on top of the first.
static REQUEST_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

/// Live, uncached preflight. macOS only; everywhere else there is nothing to
/// ask for and the screenshot path is not implemented anyway.
fn preflight() -> bool {
    #[cfg(target_os = "macos")]
    {
        unsafe { CGPreflightScreenCaptureAccess() }
    }

    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// Rate-limited preflight shared by the commands and the capture path.
pub fn is_granted() -> bool {
    let mut cache = match PREFLIGHT_CACHE.lock() {
        Ok(cache) => cache,
        // A poisoned lock must not become a reason to hammer TCC.
        Err(poisoned) => poisoned.into_inner(),
    };

    if let Some((taken_at, granted)) = *cache {
        if taken_at.elapsed() < PREFLIGHT_MIN_INTERVAL {
            return granted;
        }
    }

    let granted = preflight();
    *cache = Some((Instant::now(), granted));
    granted
}

/// Drop the cached answer so the next check is live. Used right after the
/// prompt closes, where the stale value is guaranteed to be wrong.
fn invalidate_cache() {
    if let Ok(mut cache) = PREFLIGHT_CACHE.lock() {
        *cache = None;
    }
}

/// Is the Screen Recording permission granted right now?
///
/// Cheap and safe to call on discrete UI events (mount, window focus, a
/// "check again" button). Not safe to call on a timer — see the module docs.
#[tauri::command]
#[specta::specta]
pub fn check_screen_capture_permission() -> bool {
    is_granted()
}

/// Raise the system Screen Recording prompt, then report whether the
/// permission ended up granted.
///
/// Returns immediately with `true` when the permission is already held, and
/// `false` when a prompt is already in flight.
#[tauri::command]
#[specta::specta]
pub async fn request_screen_capture_permission(app: AppHandle) -> Result<bool, String> {
    if is_granted() {
        return Ok(true);
    }

    if REQUEST_IN_FLIGHT.swap(true, Ordering::SeqCst) {
        debug!("Screen Recording prompt already in flight; ignoring duplicate request");
        return Ok(false);
    }

    let result = prompt(&app);
    REQUEST_IN_FLIGHT.store(false, Ordering::SeqCst);
    result
}

#[cfg(target_os = "macos")]
fn prompt(app: &AppHandle) -> Result<bool, String> {
    // Take the keyboard hook out of the system event path first: the prompt
    // blocks on the WindowServer, and an active CGEventTap that cannot be
    // serviced freezes input for every app on the machine, not just Handy.
    crate::shortcut::suspend_event_tap(app);

    let (tx, rx) = std::sync::mpsc::channel();
    let dispatched = app.run_on_main_thread(move || {
        // Blocks until the user answers. Its return value only reports whether
        // the prompt could be raised, so the real answer comes from the
        // preflight below.
        let raised = unsafe { CGRequestScreenCaptureAccess() };
        let _ = tx.send(raised);
    });

    let outcome = match dispatched {
        Ok(()) => match rx.recv() {
            Ok(raised) => {
                if !raised {
                    debug!("CGRequestScreenCaptureAccess reported the prompt was not raised");
                }
                invalidate_cache();
                Ok(is_granted())
            }
            Err(_) => Err("The Screen Recording prompt did not complete".to_string()),
        },
        Err(e) => Err(format!("Could not reach the main thread to prompt: {}", e)),
    };

    crate::shortcut::resume_event_tap(app);

    if outcome.is_err() {
        warn!("Screen Recording prompt failed: {:?}", outcome);
    }

    outcome
}

#[cfg(not(target_os = "macos"))]
fn prompt(_app: &AppHandle) -> Result<bool, String> {
    Ok(false)
}
