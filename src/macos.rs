//! macOS: the AX trust reads and the two notification registrations.

#![expect(
    unsafe_code,
    reason = "AXIsProcessTrusted[WithOptions] and CFNotificationCenter are C APIs; every call has a SAFETY note"
)]

use std::ffi::c_void;
use std::io;
use std::ptr;
use std::sync::{Arc, Mutex, PoisonError};

use objc2_application_services::{
    AXIsProcessTrusted, AXIsProcessTrustedWithOptions, kAXTrustedCheckOptionPrompt,
};
use objc2_core_foundation::{
    CFDictionary, CFNotificationCenter, CFNotificationName, CFNotificationSuspensionBehavior,
    CFRetained, CFString, kCFBooleanTrue,
};

use crate::{ACCESSIBILITY_NOTIFICATION, Handler, PRIVACY_NOTIFICATION, Wake};

pub(crate) fn is_trusted() -> bool {
    // SAFETY: takes no arguments and only reads the current trust state.
    unsafe { AXIsProcessTrusted() }
}

pub(crate) fn request() {
    // SAFETY: both are framework-provided constants that live for the
    // process; reading them copies a `&'static` reference.
    let (key, value) = unsafe { (kAXTrustedCheckOptionPrompt, kCFBooleanTrue) };
    let Some(value) = value else {
        return;
    };
    let options = CFDictionary::from_slices(&[key], &[value]);
    // SAFETY: the dictionary holds exactly the documented key/value pair
    // (`kAXTrustedCheckOptionPrompt` → `CFBoolean`). The pre-prompt trust it
    // returns is not the answer callers want; they read `is_trusted` later.
    let _trusted = unsafe { AXIsProcessTrustedWithOptions(Some(options.as_opaque())) };
}

/// Both registrations: the distributed notification for the Accessibility
/// list and the Darwin notification for privacy records. Dropping it
/// unregisters both and drops the handler.
pub(crate) struct Observer {
    _accessibility: Registration,
    _privacy: Registration,
}

impl Observer {
    pub(crate) fn install(handler: Handler) -> io::Result<Self> {
        let distributed = CFNotificationCenter::distributed_center()
            .ok_or_else(|| io::Error::other("no distributed notification center"))?;
        let darwin = CFNotificationCenter::darwin_notify_center()
            .ok_or_else(|| io::Error::other("no Darwin notification center"))?;
        Ok(Self {
            _accessibility: Registration::add(
                distributed,
                ACCESSIBILITY_NOTIFICATION,
                // Bypasses the suspension AppKit applies to inactive applications.
                CFNotificationSuspensionBehavior::DeliverImmediately,
                Wake::AccessibilityChanged,
                Arc::clone(&handler),
            ),
            _privacy: Registration::add(
                darwin,
                PRIVACY_NOTIFICATION,
                // The Darwin center always delivers immediately, ignores this
                // argument and asks for 0 in its place (CFNotificationCenter.h).
                CFNotificationSuspensionBehavior(0),
                Wake::PrivacyChanged,
                handler,
            ),
        })
    }
}

/// What a notification callback finds at its observer address: the wake this
/// registration reports, and the handler — `None` once the registration is
/// gone.
///
/// The cell is leaked on purpose. Notifications are delivered on the main
/// run loop, and removing the observer from another thread does not wait for
/// a delivery already in flight; keeping the address valid forever and
/// emptying the cell instead makes that delivery a no-op. The handler itself
/// — the user's closure and everything it captured — is dropped.
struct Cell {
    wake: Wake,
    handler: Mutex<Option<Handler>>,
}

/// One observer registration on one notification center.
struct Registration {
    center: CFRetained<CFNotificationCenter>,
    name: CFRetained<CFString>,
    cell: &'static Cell,
}

impl Registration {
    fn add(
        center: CFRetained<CFNotificationCenter>,
        name: &str,
        suspension: CFNotificationSuspensionBehavior,
        wake: Wake,
        handler: Handler,
    ) -> Self {
        let name = CFString::from_str(name);
        let cell: &'static Cell = Box::leak(Box::new(Cell {
            wake,
            handler: Mutex::new(Some(handler)),
        }));
        // SAFETY: `notified` matches `CFNotificationCallback`; the observer
        // address is `cell`, which is never freed, so the callback can read it
        // for as long as the registration exists and after. Neither center
        // wants an object: the distributed notification is posted without
        // one, and the Darwin center has no objects at all.
        unsafe {
            center.add_observer(
                ptr::from_ref(cell).cast(),
                Some(notified),
                Some(&name),
                ptr::null(),
                suspension,
            );
        }
        Self { center, name, cell }
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        // SAFETY: the same observer address, name and (null) object the
        // registration used.
        unsafe {
            self.center.remove_observer(
                ptr::from_ref(self.cell).cast(),
                Some(&self.name),
                ptr::null(),
            );
        }
        // Waits for a delivery in progress, then drops the handler.
        self.cell
            .handler
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
    }
}

// SAFETY: `CFNotificationCenter` and immutable `CFString` are thread-safe
// Core Foundation objects, and the cell is a `Mutex` behind a `'static`
// reference; nothing here is tied to the thread that registered.
unsafe impl Send for Registration {}
// SAFETY: as above; `&Registration` exposes no operation at all.
unsafe impl Sync for Registration {}

unsafe extern "C-unwind" fn notified(
    _center: *mut CFNotificationCenter,
    observer: *mut c_void,
    _name: *const CFNotificationName,
    _object: *const c_void,
    _user_info: *const CFDictionary,
) {
    // SAFETY: `observer` is the address of the leaked `Cell` registered in
    // `Registration::add`, valid for the life of the process.
    let cell = unsafe { &*observer.cast::<Cell>() };
    // Held across the call so `Registration::drop` cannot free the handler
    // under it; a delivery after the drop finds the cell empty.
    if let Some(handler) = &*cell.handler.lock().unwrap_or_else(PoisonError::into_inner) {
        handler(cell.wake);
    }
}
