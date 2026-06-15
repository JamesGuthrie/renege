use crate::{CAM_LINK_PID, CAM_LINK_VID};
use core_foundation::base::TCFType;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use core_foundation_sys::base::CFRetain;
use core_foundation_sys::dictionary::{CFDictionarySetValue, CFMutableDictionaryRef};
use core_foundation_sys::runloop::{CFRunLoopAddSource, CFRunLoopGetMain, kCFRunLoopCommonModes};
use io_kit_sys::keys::{kIOFirstMatchNotification, kIOTerminatedNotification};
use io_kit_sys::ret::kIOReturnSuccess;
use io_kit_sys::types::{io_iterator_t, io_object_t};
use io_kit_sys::{
    IOIteratorNext, IONotificationPortCreate, IONotificationPortDestroy,
    IONotificationPortGetRunLoopSource, IONotificationPortRef, IOObjectRelease,
    IOServiceAddMatchingNotification, IOServiceMatching, kIOMasterPortDefault,
};
use objc2::MainThreadMarker;
use std::ffi::c_void;

/// Callback is a boxed fn which receives an `io_object_t`
pub type Callback = Box<dyn Fn(io_object_t)>;

/// `IOIterator` wraps an `io_iterator_t` providing the ability
/// apply a callback on every item in the iterator.
/// The iterator is always processed to completion.
struct IOIterator {
    iterator: io_iterator_t,
}

impl IOIterator {
    /// # Safety
    /// iterator must be a valid iterator given to us by `IOKit`
    unsafe fn new(iterator: io_iterator_t) -> Self {
        IOIterator { iterator }
    }

    /// calls the provided callback for every item in the iterator
    fn apply_callback(&self, cb: &Callback) {
        // Note: we must process the iterator until empty to re-arm the notification.
        loop {
            // SAFETY: this IOIterator was constructed with a valid iterator
            let service = unsafe { IOIteratorNext(self.iterator) };
            if service == 0 {
                break;
            }
            cb(service);
            // SAFETY: we got this object handle from IOKit, and it's valid
            unsafe { IOObjectRelease(service) };
        }
    }
}

/// # Safety
/// Only valid as an `IOKit` notification callback. `IOKit` must invoke it with:
/// - `refcon`: the `*const Callback` registered in `IONotificationHandler::new`
/// - `iterator`: the notification's iterator, which we do not release
unsafe extern "C" fn handle_device(refcon: *mut c_void, iterator: io_iterator_t) {
    // SAFETY: refcon is a pointer obtained from AnchoredCallback::as_refcon,
    // which we registered with the IOServiceAddMatchingNotification call
    let cb = unsafe { AnchoredCallback::callback_from_refcon(refcon) };
    // SAFETY: iterator is a valid iterator given to us by IOKit
    unsafe { IOIterator::new(iterator) }.apply_callback(cb);
}

/// A stable reference to a `Callback`
/// `IONotificationHandler` requires a stable, thin pointer in order to pass
/// the callback into `IOKit` (as `refcon`).
struct AnchoredCallback(Box<Callback>);

impl AnchoredCallback {
    fn new(cb: Callback) -> Self {
        AnchoredCallback(Box::new(cb))
    }

    fn callback(&self) -> &Callback {
        self.0.as_ref()
    }

    fn as_refcon(&self) -> *mut c_void {
        std::ptr::from_ref(self.0.as_ref()) as *mut c_void
    }

    /// # Safety
    /// - refcon must be a pointer created by a call to `as_refcon`
    /// - the originating `AnchoredCallback` must outlive `'a` (it is not borrow-checked).
    unsafe fn callback_from_refcon<'a>(refcon: *mut c_void) -> &'a Callback {
        // SAFETY: the caller has given us a pointer created by as_refcon on an
        // AnchoredCallback which is still live, so we know it is valid.
        unsafe { &*refcon.cast::<Callback>() }
    }
}

pub struct IONotificationHandler {
    port: IONotificationPortRef,
    /// the Callback to call when the device is registered.
    added: AnchoredCallback,
    /// the Callback to call when the device is unregistered.
    removed: AnchoredCallback,
    iter_added: io_iterator_t,
    iter_removed: io_iterator_t,
}

impl Drop for IONotificationHandler {
    fn drop(&mut self) {
        // SAFETY: iter_added, iter_removed, and port were created, and are
        // exclusively owned, by us
        // NOTE: it is critical that this.added and this.removed are dropped
        // _after_ the port is destroyed, to prevent the notification handler
        // from calling invalid callbacks.
        unsafe {
            IOObjectRelease(self.iter_added);
            IOObjectRelease(self.iter_removed);
            IONotificationPortDestroy(self.port);
        };
    }
}

impl IONotificationHandler {
    /// create a new `IONotificationHandler` with callbacks for an item being added/removed.
    /// Must be called from the main thread.
    pub fn new(_mtm: MainThreadMarker, added: Callback, removed: Callback) -> anyhow::Result<Self> {
        // Note:
        // - the resulting ref is not documented to be NULL, and Apple's example doesn't check
        // SAFETY: we're passing the default-port constant and no other caller preconditions exist.
        let notify_port = unsafe { IONotificationPortCreate(kIOMasterPortDefault) };
        // Ditto re: ref nullability
        // SAFETY: we're passing a value we received from IONotificationPortCreate, which is not
        // documented to fail, and Apple's sample doesn't check it.
        let run_source = unsafe { IONotificationPortGetRunLoopSource(notify_port) };
        // Run the notify port on the main thread
        // SAFETY:
        //  - CFRunLoopGetMain returns the main thread's run loop (lazily created, non-null
        //    on a real thread; we're on the main thread per the MainThreadMarker and
        //    post-NSApplication setup)
        //  - run_source came from notify_port
        //  - The mode is a valid CF constant.
        unsafe { CFRunLoopAddSource(CFRunLoopGetMain(), run_source, kCFRunLoopCommonModes) };

        // Build a matching dictionary for *only* the CamLink.
        // SAFETY: we're passing a valid c-string
        let matching: CFMutableDictionaryRef =
            unsafe { IOServiceMatching(c"IOUSBHostDevice".as_ptr()) };
        if matching.is_null() {
            anyhow::bail!("failed to create matching dictionary");
        }
        let vid = CFNumber::from(CAM_LINK_VID);
        let pid = CFNumber::from(CAM_LINK_PID);
        let vid_key = CFString::from_static_string("idVendor");
        let pid_key = CFString::from_static_string("idProduct");

        // SAFETY:
        //  - matching is a valid mutable dict
        //  - the key/value CFString/CFNumber are live (their Rust owners outlive the call).
        unsafe {
            CFDictionarySetValue(
                matching,
                vid_key.as_concrete_TypeRef().cast::<c_void>(),
                vid.as_concrete_TypeRef().cast::<c_void>(),
            );
            CFDictionarySetValue(
                matching,
                pid_key.as_concrete_TypeRef().cast::<c_void>(),
                pid.as_concrete_TypeRef().cast::<c_void>(),
            );
        }

        // Each AddMatchingNotification call CONSUMES one reference to `matching`.
        // We register twice, so add one extra retain to balance.
        // SAFETY: IOServiceMatching returns a singly-retained obj
        unsafe {
            CFRetain(matching.cast::<c_void>());
        }

        let mut this = IONotificationHandler {
            port: notify_port,
            added: AnchoredCallback::new(added),
            removed: AnchoredCallback::new(removed),
            iter_added: 0,
            iter_removed: 0,
        };

        // SAFETY:
        //  - notify_port is valid
        //  - matching carries the reference this call consumes (IOServiceMatching +1, plus the CFRetain above for the two calls)
        //  - the callback has the required `unsafe extern "C"` ABI
        //  - refcon is obtained from AnchoredCallback and is a thin pointer which lives as long as the notification is active
        //  - iter pointer is a valid out-param.
        unsafe {
            let result = IOServiceAddMatchingNotification(
                notify_port,
                kIOFirstMatchNotification,
                matching,
                handle_device,
                this.added.as_refcon(),
                &raw mut this.iter_added,
            );
            if result != kIOReturnSuccess {
                anyhow::bail!("failed to add device_added notification");
            }
            let result = IOServiceAddMatchingNotification(
                notify_port,
                kIOTerminatedNotification,
                matching,
                handle_device,
                this.removed.as_refcon(),
                &raw mut this.iter_removed,
            );
            if result != kIOReturnSuccess {
                anyhow::bail!("failed to add device_removed notification");
            }
        }

        // Arm both: drain the initial iterators once.
        // SAFETY: iterator is a valid iterator given to us by `IOKit`
        unsafe { IOIterator::new(this.iter_added) }.apply_callback(this.added.callback());
        // SAFETY: iterator is a valid iterator given to us by `IOKit`
        unsafe { IOIterator::new(this.iter_removed) }.apply_callback(this.removed.callback());

        Ok(this)
    }
}
