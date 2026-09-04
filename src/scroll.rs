//! Scroll-activity monitor for the focus limit.
//!
//! A listen-only `CGEventTap` on `scrollWheel` events lets the focus limit
//! distinguish *actively scrolling* from other input (reading while
//! highlighting text, or writing an email) that the idle timer alone counts
//! as "active". The tap needs the Accessibility grant; if it isn't granted
//! the tap won't create and `take_scrolled` returns None, so the focus
//! limit falls back to idle-based activity (see `earned.rs`).
//!
//! The callback does the minimum (one atomic increment) so the tap is never
//! disabled for being slow; it also re-enables the tap if macOS ever
//! disables it. The monitor runs its own CFRunLoop on a dedicated thread.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};

use objc2_core_foundation::{kCFRunLoopCommonModes, CFMachPort, CFRunLoop};
use objc2_core_graphics::{
    CGEvent, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventTapProxy,
    CGEventType,
};

/// Scroll events observed since the last `take_scrolled`.
static SCROLL_COUNT: AtomicU64 = AtomicU64::new(0);
/// True once the tap is live (Accessibility granted).
static TAP_ACTIVE: AtomicBool = AtomicBool::new(false);
/// Raw CFMachPort for re-enabling the tap from the callback if the OS
/// disables it. The port stays alive for the process's life via the monitor
/// thread's CFRunLoop.
static TAP_PORT: AtomicPtr<CFMachPort> = AtomicPtr::new(std::ptr::null_mut());

unsafe extern "C-unwind" fn callback(
    _proxy: CGEventTapProxy,
    etype: CGEventType,
    event: NonNull<CGEvent>,
    _user: *mut c_void,
) -> *mut CGEvent {
    if etype == CGEventType::ScrollWheel {
        SCROLL_COUNT.fetch_add(1, Ordering::Relaxed);
    } else if etype == CGEventType::TapDisabledByTimeout
        || etype == CGEventType::TapDisabledByUserInput
    {
        let port = TAP_PORT.load(Ordering::Relaxed);
        if !port.is_null() {
            CGEvent::tap_enable(unsafe { &*port }, true);
        }
    }
    // Listen-only: pass the event through unmodified.
    event.as_ptr()
}

/// Did any scrolling happen since the last call? None means the tap isn't
/// active (Accessibility not granted) — caller should fall back to idle.
pub fn take_scrolled() -> Option<bool> {
    if TAP_ACTIVE.load(Ordering::Relaxed) {
        Some(SCROLL_COUNT.swap(0, Ordering::Relaxed) > 0)
    } else {
        None
    }
}

/// Start the scroll monitor on a dedicated CFRunLoop thread. Safe to call
/// once at startup; without the Accessibility grant it just leaves the focus
/// limit on its idle-based fallback.
pub fn spawn() {
    std::thread::spawn(|| {
        let mask: u64 = 1 << CGEventType::ScrollWheel.0;
        // SAFETY: standard CGEventTap setup; `callback` is a 'static fn.
        let port = unsafe {
            CGEvent::tap_create(
                CGEventTapLocation::SessionEventTap,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::ListenOnly,
                mask,
                Some(callback),
                std::ptr::null_mut(),
            )
        };
        let Some(port) = port else {
            tracing::warn!(
                "scroll monitor: could not create event tap — grant Accessibility to \
                 betamacs for scroll-based focus detection; falling back to idle"
            );
            return;
        };
        TAP_PORT.store((&*port as *const CFMachPort) as *mut CFMachPort, Ordering::Relaxed);
        let source = CFMachPort::new_run_loop_source(None, Some(&port), 0);
        let Some(run_loop) = CFRunLoop::current() else {
            tracing::error!("scroll monitor: no current run loop");
            return;
        };
        run_loop.add_source(source.as_deref(), unsafe { kCFRunLoopCommonModes });
        CGEvent::tap_enable(&port, true);
        TAP_ACTIVE.store(true, Ordering::Relaxed);
        tracing::info!("scroll monitor: event tap active");
        // Keep `port`/`source` alive and pump events forever.
        CFRunLoop::run();
        drop((port, source));
    });
}
