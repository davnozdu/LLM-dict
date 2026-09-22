//! Проверка исходного поля и выделения перед отложенной вставкой.
use core_foundation::base::{CFType, CFTypeRef, TCFType};
use core_foundation::string::{CFString, CFStringRef};

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXUIElementCreateApplication(pid: i32) -> CFTypeRef;
    fn AXUIElementCopyAttributeValue(
        element: CFTypeRef,
        name: CFStringRef,
        value: *mut CFTypeRef,
    ) -> i32;
    fn AXUIElementSetMessagingTimeout(element: CFTypeRef, timeout: f32) -> i32;
    fn AXValueGetValue(value: CFTypeRef, kind: u32, result: *mut std::ffi::c_void) -> bool;
}

fn attribute(element: &CFType, name: &str) -> Option<CFType> {
    let name = CFString::new(name);
    let mut value = std::ptr::null();
    let result = unsafe {
        AXUIElementCopyAttributeValue(
            element.as_CFTypeRef(),
            name.as_concrete_TypeRef(),
            &mut value,
        )
    };
    if result != 0 || value.is_null() {
        return None;
    }
    Some(unsafe { CFType::wrap_under_create_rule(value) })
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Range {
    location: isize,
    length: isize,
}

pub struct Target {
    pub pid: i32,
    window: CFType,
    element: CFType,
    range: Range,
    value: CFType,
    title: Option<CFType>,
}

// AX handles are retained IPC references, and the copied attribute values are
// immutable CF values. Ownership moves to the paste worker, never shared mutably.
unsafe impl Send for Target {}

impl Target {
    pub fn capture() -> Option<Self> {
        let pid = crate::macos::frontmost_app_pid()?;
        let raw = unsafe { AXUIElementCreateApplication(pid) };
        if raw.is_null() {
            return None;
        }
        let app = unsafe { CFType::wrap_under_create_rule(raw) };
        unsafe {
            AXUIElementSetMessagingTimeout(app.as_CFTypeRef(), 0.25);
        }
        let window = attribute(&app, "AXFocusedWindow")?;
        let element = attribute(&app, "AXFocusedUIElement")?;
        let selected = attribute(&element, "AXSelectedTextRange")?;
        let mut range = Range::default();
        if !unsafe {
            AXValueGetValue(
                selected.as_CFTypeRef(),
                4,
                (&mut range as *mut Range).cast(),
            )
        } {
            return None;
        }
        let value = attribute(&element, "AXValue")?;
        let title = attribute(&window, "AXTitle");
        (crate::macos::frontmost_app_pid() == Some(pid)).then_some(Self {
            pid,
            window,
            element,
            range,
            value,
            title,
        })
    }

    pub fn is_current(&self) -> bool {
        Self::capture().is_some_and(|now| {
            now.pid == self.pid
                && now.window == self.window
                && now.element == self.element
                && now.range == self.range
                && now.value == self.value
                && now.title == self.title
        })
    }
}
