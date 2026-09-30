//! macOS: the AX trust reads and the distributed-notification observer.

#![expect(
    unsafe_code,
    reason = "AXIsProcessTrusted[WithOptions] and CFNotificationCenter are C APIs; every call has a SAFETY note"
)]

use std::ffi::c_void;
use std::io;
use std::sync::{Mutex, PoisonError};

use objc2_application_services::{
    AXIsProcessTrusted, AXIsProcessTrustedWithOptions, kAXTrustedCheckOptionPrompt,
};
use objc2_core_foundation::{
    CFDictionary, CFNotificationCenter, CFNotificationName, CFNotificationSuspensionBehavior,
    CFRetained, CFString, kCFBooleanTrue,
};

use crate::{Handler, NOTIFICATION, Wake};

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

/// What the notification callback finds at the observer address: the handler,
/// or `None` once the [`Observer`] is gone.
///
/// The cell is leaked on purpose. Notifications are delivered on the main
/// run loop, and removing the observer from another thread does not wait for
/// a delivery already in flight; keeping the address valid forever and
/// emptying the cell instead makes that delivery a no-op. The handler itself
/// — the user's closure and everything it captured — is dropped.
struct Cell(Mutex<Option<Handler>>);

/// The registered observer. Dropping it unregisters and drops the handler.
pub(crate) struct Observer {
    center: CFRetained<CFNotificationCenter>,
    name: CFRetained<CFString>,
    cell: &'static Cell,
}

impl Observer {
    pub(crate) fn install(handler: Handler) -> io::Result<Self> {
        let center = CFNotificationCenter::distributed_center()
            .ok_or_else(|| io::Error::other("no distributed notification center"))?;
        let name = CFString::from_str(NOTIFICATION);
        let cell: &'static Cell = Box::leak(Box::new(Cell(Mutex::new(Some(handler)))));
        // SAFETY: `notified` matches `CFNotificationCallback`; the observer
        // address is `cell`, which is never freed, so the callback can read it
        // for as long as the registration exists and after. `DeliverImmediately`
        // bypasses the suspension AppKit applies to inactive applications.
        unsafe {
            center.add_observer(
                std::ptr::from_ref(cell).cast(),
                Some(notified),
                Some(&name),
                std::ptr::null(),
                CFNotificationSuspensionBehavior::DeliverImmediately,
            );
        }
        Ok(Self { center, name, cell })
    }
}

impl Drop for Observer {
    fn drop(&mut self) {
        // SAFETY: the same observer address, name and (null) object the
        // registration used.
        unsafe {
            self.center.remove_observer(
                std::ptr::from_ref(self.cell).cast(),
                Some(&self.name),
                std::ptr::null(),
            );
        }
        // Waits for a delivery in progress, then drops the handler.
        self.cell
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
    }
}

// SAFETY: `CFNotificationCenter` and immutable `CFString` are thread-safe
// Core Foundation objects, and the cell is a `Mutex` behind a `'static`
// reference; nothing here is tied to the thread that registered.
unsafe impl Send for Observer {}
// SAFETY: as above; `&Observer` exposes no operation at all.
unsafe impl Sync for Observer {}

unsafe extern "C-unwind" fn notified(
    _center: *mut CFNotificationCenter,
    observer: *mut c_void,
    _name: *const CFNotificationName,
    _object: *const c_void,
    _user_info: *const CFDictionary,
) {
    // SAFETY: `observer` is the address of the leaked `Cell` registered in
    // `Observer::install`, valid for the life of the process.
    let cell = unsafe { &*observer.cast::<Cell>() };
    // Held across the call so `Observer::drop` cannot free the handler under
    // it; a delivery after the drop finds the cell empty.
    if let Some(handler) = &*cell.0.lock().unwrap_or_else(PoisonError::into_inner) {
        handler(Wake::Changed);
    }
}
