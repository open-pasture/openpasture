//! Keep the Mac awake while the server has work a sleeping Mac would let
//! down: collars reporting, a move sweeping, an escape bringing an animal
//! back (`op_server::work`). An IOKit assertion that stops idle sleep; it
//! doesn't stop a closed lid, which is why the field server belongs on an
//! always-on box. App Nap is off for the whole app (`Info.plist`), so the
//! engine's timers run on time while the window is hidden.

use std::time::Duration;

use op_core::Ctx;

/// How often the server's work is looked at.
const EVERY: Duration = Duration::from_secs(30);

/// Hold the assertion while [`op_server::work::Work::any`], until the server stops.
pub async fn watch(ctx: Ctx) {
    let mut held: Option<Assertion> = None;
    let stop = ctx.on_shutdown();
    tokio::pin!(stop);
    loop {
        let busy = match op_server::work::current(&ctx).await {
            Ok(w) => w.any(),
            Err(e) => {
                tracing::warn!(error = format!("{e:#}"), "reading the server's work");
                held.is_some()
            }
        };
        match (busy, held.is_some()) {
            (true, false) => {
                held = Assertion::take("openpasture is running the fence");
                tracing::info!(held = held.is_some(), "keeping the Mac awake");
            }
            (false, true) => {
                held = None;
                tracing::info!("letting the Mac sleep");
            }
            _ => {}
        }
        tokio::select! {
            _ = &mut stop => break,
            _ = tokio::time::sleep(EVERY) => {}
        }
    }
}

#[cfg(target_os = "macos")]
mod iokit {
    use std::ffi::{CString, c_char, c_void};

    pub type CFStringRef = *const c_void;
    const UTF8: u32 = 0x0800_0100;
    const LEVEL_ON: u32 = 255;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithCString(alloc: *const c_void, s: *const c_char, encoding: u32) -> CFStringRef;
        fn CFRelease(cf: *const c_void);
    }

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOPMAssertionCreateWithName(kind: CFStringRef, level: u32, name: CFStringRef, id: *mut u32) -> i32;
        fn IOPMAssertionRelease(id: u32) -> i32;
    }

    fn cfstr(s: &str) -> Option<CFStringRef> {
        let c = CString::new(s).ok()?;
        // SAFETY: a valid NUL-terminated UTF-8 string; the result is released by the caller.
        let r = unsafe { CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), UTF8) };
        (!r.is_null()).then_some(r)
    }

    pub fn create(reason: &str) -> Option<u32> {
        let kind = cfstr("PreventUserIdleSystemSleep")?;
        let Some(name) = cfstr(reason) else {
            // SAFETY: created above and not used after.
            unsafe { CFRelease(kind) };
            return None;
        };
        let mut id = 0u32;
        // SAFETY: both strings are live CFStrings; `id` is written on success.
        let rc = unsafe { IOPMAssertionCreateWithName(kind, LEVEL_ON, name, &mut id) };
        // SAFETY: IOKit keeps its own references.
        unsafe {
            CFRelease(kind);
            CFRelease(name);
        }
        (rc == 0).then_some(id)
    }

    pub fn release(id: u32) {
        // SAFETY: `id` came from a successful create and is released once.
        unsafe { IOPMAssertionRelease(id) };
    }
}

/// A held "don't idle-sleep" assertion, released on drop.
struct Assertion(#[allow(dead_code)] u32);

impl Assertion {
    #[cfg(target_os = "macos")]
    fn take(reason: &str) -> Option<Self> {
        iokit::create(reason).map(Self)
    }

    #[cfg(not(target_os = "macos"))]
    fn take(_reason: &str) -> Option<Self> {
        None
    }
}

impl Drop for Assertion {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        iokit::release(self.0);
    }
}
