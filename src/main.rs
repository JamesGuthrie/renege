use anyhow::Result;
use core_foundation::base::TCFType;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use core_foundation_sys::base::{CFGetTypeID, CFRetain, kCFAllocatorDefault};
use core_foundation_sys::dictionary::{CFDictionarySetValue, CFMutableDictionaryRef};
use core_foundation_sys::number::{CFNumberGetTypeID, CFNumberRef};
use core_foundation_sys::runloop::{CFRunLoopAddSource, CFRunLoopGetMain, kCFRunLoopCommonModes};
use io_kit_sys::keys::{kIOFirstMatchNotification, kIOTerminatedNotification};
use io_kit_sys::types::{io_iterator_t, io_service_t};
use io_kit_sys::{
    IOIteratorNext, IONotificationPortCreate, IONotificationPortGetRunLoopSource, IOObjectRelease,
    IORegistryEntryCreateCFProperty, IOServiceAddMatchingNotification, IOServiceMatching,
    kIOMasterPortDefault,
};
use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSColor, NSImage, NSImageSymbolConfiguration,
    NSImageSymbolScale, NSMenu, NSMenuItem, NSStatusBar, NSStatusBarButton,
    NSVariableStatusItemLength,
};
use objc2_foundation::NSString;
use std::ffi::c_void;

// Vendor ID for CamLink 4k
const CAM_LINK_VID: i32 = 0x0fd9;
// Product ID for CamLink 4k
const CAM_LINK_PID: i32 = 0x0066;

unsafe extern "C" fn device_added(refcon: *mut c_void, iterator: io_iterator_t) {
    drain_added(refcon, iterator);
}

unsafe extern "C" fn device_removed(refcon: *mut c_void, iterator: io_iterator_t) {
    drain_removed(refcon, iterator);
}

fn drain_added(refcon: *mut c_void, iterator: io_iterator_t) {
    // SAFETY:
    //  - refcon is the pointer registered with IOServiceAddMatchingNotification
    //  - that pointer was obtained from Box::into_raw(AppState) and leaked for
    //    the process lifetime, so it is always valid and correctly typed.
    //  - Callbacks run only on the main run loop and take only shared
    //    references, so there is no aliasing or data race.
    let state = unsafe { &*(refcon as *const AppState) };
    let mut latest: Option<USBStatus> = None;
    loop {
        // SAFETY: we got this iterator handle from IOKit
        let service = unsafe { IOIteratorNext(iterator) };
        if service == 0 {
            break;
        }
        latest = match device_speed(service) {
            Some(s) if s >= 3 => Some(USBStatus::Negotiated),
            _ => Some(USBStatus::Misnegotiated),
        };
        // SAFETY: we got this object handle from IOKit, and it's valid
        unsafe { IOObjectRelease(service) };
    }
    if let Some(l) = latest {
        apply_status(&state.button, &state.image, l);
    }
}

// MUST run to completion: this both processes the devices AND re-arms the
// notification. Stop draining early and the callback never fires again.
fn drain_removed(refcon: *mut c_void, iterator: io_iterator_t) {
    // SAFETY:
    //  - refcon is the pointer registered with IOServiceAddMatchingNotification
    //  - that pointer was obtained from Box::into_raw(AppState) and leaked for
    //    the process lifetime, so it is always valid and correctly typed.
    //  - Callbacks run only on the main run loop and take only shared
    //    references, so there is no aliasing or data race.
    let state = unsafe { &*(refcon as *const AppState) };
    let mut removed = false;
    loop {
        // SAFETY: we got this iterator handle from IOKit
        let service = unsafe { IOIteratorNext(iterator) };
        if service == 0 {
            break;
        }
        removed = true;
        // SAFETY: we got this object handle from IOKit, and it's valid
        unsafe {
            IOObjectRelease(service);
        }
    }
    if removed {
        apply_status(&state.button, &state.image, USBStatus::Disconnected);
    }
}

struct AppState {
    button: Retained<NSStatusBarButton>,
    image: Retained<NSImage>,
}

fn main() -> Result<()> {
    let mtm = MainThreadMarker::new().unwrap();
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let (button, image) = build_menu_bar(mtm);
    let state = Box::new(AppState { button, image });
    let state_ptr = Box::into_raw(state);

    register_usb_notification(state_ptr)?;

    app.run();
    Ok(())
}

fn build_menu_bar(mtm: MainThreadMarker) -> (Retained<NSStatusBarButton>, Retained<NSImage>) {
    let status_bar = NSStatusBar::systemStatusBar();
    let status_item = status_bar.statusItemWithLength(NSVariableStatusItemLength);
    let menu = NSMenu::new(mtm);
    // SAFETY: selector is valid
    let quit_item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str("Quit"),
            Some(sel!(terminate:)),
            &NSString::from_str("q"),
        )
    };

    menu.addItem(&quit_item);
    status_item.setMenu(Some(&menu));
    let button = status_item.button(mtm).unwrap();
    let image = NSImage::imageWithSystemSymbolName_accessibilityDescription(
        &NSString::from_str("circle.fill"),
        Some(&NSString::from_str("Display USB status")),
    )
    .expect("circle.fill symbol should exist on macOS 11+");
    apply_status(&button, &image, USBStatus::Disconnected);
    (button, image)
}

fn register_usb_notification(state_ptr: *mut AppState) -> Result<()> {
    // Note:
    // - the resulting ref is not documented to be NULL, and Apple's example doesn't check
    // - we're not manually cleaning this up with IONotificationPortDestroy, so it leaks.
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

    // 3. Register for arrival and removal — two separate notifications.
    let mut iter_added: io_iterator_t = 0;
    let mut iter_removed: io_iterator_t = 0;

    // SAFETY:
    //  - notify_port is valid
    //  - matching carries the reference this call consumes (IOServiceMatching +1, plus the CFRetain above for the two calls)
    //  - the callback has the required `unsafe extern "C"` ABI
    //  - state_ptr is the leaked AppState used as refcon
    //  - iter pointer is a valid out-param.
    unsafe {
        IOServiceAddMatchingNotification(
            notify_port,
            kIOFirstMatchNotification,
            matching,
            device_added,
            state_ptr.cast::<c_void>(),
            &raw mut iter_added,
        );
        IOServiceAddMatchingNotification(
            notify_port,
            kIOTerminatedNotification,
            matching,
            device_removed,
            state_ptr.cast::<c_void>(),
            &raw mut iter_removed,
        );
    }

    // 4. Arm both: drain the initial iterators once. The "added" drain also
    //    reports the Cam Link if it's ALREADY plugged in at launch.
    drain_added(state_ptr.cast::<c_void>(), iter_added);
    drain_removed(state_ptr.cast::<c_void>(), iter_removed);
    Ok(())
}

fn device_speed(service: io_service_t) -> Option<i64> {
    let key = CFString::from_static_string("Device Speed");
    // SAFETY:
    //  - service is an io_service_t yielded by a matching-notification iterator.
    //  - Every IOService is an IORegistry node, so it is a valid io_registry_entry_t
    //    for IORegistryEntryCreateCFProperty.
    let prop = unsafe {
        IORegistryEntryCreateCFProperty(service, key.as_concrete_TypeRef(), kCFAllocatorDefault, 0)
    };
    if prop.is_null() {
        return None;
    }
    // Ensure that the thing we got back is actually a CFNumber
    // SAFETY: prop is a valid pointer that we got from IOKit
    if unsafe { CFGetTypeID(prop) != CFNumberGetTypeID() } {
        return None;
    }
    // SAFETY: we know it's a CFNumber, and was produced under the create rule
    unsafe { CFNumber::wrap_under_create_rule(prop as CFNumberRef).to_i64() }
}

#[derive(Clone, Copy, PartialEq)]
enum USBStatus {
    Disconnected,
    Negotiated,
    Misnegotiated,
}

fn apply_status(button: &NSStatusBarButton, base: &NSImage, status: USBStatus) {
    let color = match status {
        USBStatus::Disconnected => NSColor::systemGrayColor(),
        USBStatus::Negotiated => NSColor::systemGreenColor(),
        USBStatus::Misnegotiated => NSColor::systemRedColor(),
    };

    let colour_config = NSImageSymbolConfiguration::configurationWithHierarchicalColor(&color);
    let size_config = NSImageSymbolConfiguration::configurationWithPointSize_weight_scale(
        10.0,
        0.0,
        NSImageSymbolScale::Medium,
    );
    let config = size_config.configurationByApplyingConfiguration(&colour_config);
    let colored = base
        .imageWithSymbolConfiguration(&config)
        .expect("applying symbol configuration");
    button.setImage(Some(&colored));
}
