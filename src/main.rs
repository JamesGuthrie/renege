mod iokit;

use crate::iokit::{Callback, IONotificationHandler};
use anyhow::Result;
use core_foundation::base::TCFType;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use core_foundation_sys::base::{CFGetTypeID, CFRelease, kCFAllocatorDefault};
use core_foundation_sys::number::{CFNumberGetTypeID, CFNumberRef};
use io_kit_sys::IORegistryEntryCreateCFProperty;
use io_kit_sys::types::{io_object_t, io_service_t};
use objc2::rc::{Retained, autoreleasepool};
use objc2::{MainThreadMarker, MainThreadOnly, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSColor, NSImage, NSImageSymbolConfiguration,
    NSImageSymbolScale, NSMenu, NSMenuItem, NSStatusBar, NSStatusItem, NSVariableStatusItemLength,
};
use objc2_foundation::NSString;
use std::fmt::{Display, Formatter};

// Vendor ID for CamLink 4k
const CAM_LINK_VID: i32 = 0x0fd9;
// Product ID for CamLink 4k
const CAM_LINK_PID: i32 = 0x0066;

struct AppState {
    status_icon: StatusIcon,
}

#[derive(Clone, Copy, PartialEq)]
enum USBStatus {
    Disconnected,
    Negotiated,
    Misnegotiated,
}

impl Display for USBStatus {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            USBStatus::Disconnected => write!(f, "Disconnected"),
            USBStatus::Negotiated => write!(f, "Negotiated"),
            USBStatus::Misnegotiated => write!(f, "Misnegotiated"),
        }
    }
}

fn main() -> Result<()> {
    let (app, _handler) = autoreleasepool(|_pool| {
        let mtm = MainThreadMarker::new().unwrap();
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

        let status_icon = StatusIcon::new(mtm);
        let state = Box::new(AppState { status_icon });
        let state = Box::leak(state);

        let added: Callback = Box::new(|service: io_object_t| {
            let speed = device_speed(service);
            let status = match speed {
                Some(s) if s >= 3 => USBStatus::Negotiated,
                _ => USBStatus::Misnegotiated,
            };
            state.status_icon.set_status(status);
        });
        let removed = Box::new(|_: io_object_t| {
            state.status_icon.set_status(USBStatus::Disconnected);
        });

        let handler = IONotificationHandler::new(mtm, added, removed)?;
        Ok::<(Retained<NSApplication>, IONotificationHandler), anyhow::Error>((app, handler))
    })?;

    app.run();
    Ok(())
}

struct StatusIcon {
    mtm: MainThreadMarker,
    status_item: Retained<NSStatusItem>,
    image: Retained<NSImage>,
}

impl StatusIcon {
    fn new(mtm: MainThreadMarker) -> StatusIcon {
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
        let image = NSImage::imageWithSystemSymbolName_accessibilityDescription(
            &NSString::from_str("circle.fill"),
            Some(&NSString::from_str("Display USB status")),
        )
        .expect("circle.fill symbol should exist on macOS 11+");

        StatusIcon {
            mtm,
            status_item,
            image,
        }
    }

    pub fn set_status(&self, status: USBStatus) {
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
        let colored = self
            .image
            .imageWithSymbolConfiguration(&config)
            .expect("applying symbol configuration");
        self.status_item
            .button(self.mtm)
            .unwrap()
            .setImage(Some(&colored));
    }
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
        // SAFETY: prop is a valid pointer that we got from IOKit, and non-null
        unsafe { CFRelease(prop) };
        return None;
    }
    // SAFETY: we know it's a CFNumber, and was produced under the create rule
    unsafe { CFNumber::wrap_under_create_rule(prop as CFNumberRef).to_i64() }
}
