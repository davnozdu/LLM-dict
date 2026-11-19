//! Проверка исходного окна перед отложенной вставкой.
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

fn focused_window(pid: i32) -> Option<CFType> {
    let raw = unsafe { AXUIElementCreateApplication(pid) };
    if raw.is_null() {
        return None;
    }
    let app = unsafe { CFType::wrap_under_create_rule(raw) };
    unsafe {
        AXUIElementSetMessagingTimeout(app.as_CFTypeRef(), 0.25);
    }
    attribute(&app, "AXFocusedWindow")
}

pub struct Target {
    pub pid: i32,
    window: Option<CFType>,
}

// AX handles are retained IPC references, and the copied attribute values are
// immutable CF values. Ownership moves to the paste worker, never shared mutably.
unsafe impl Send for Target {}

impl Target {
    pub fn capture() -> Option<Self> {
        let pid = crate::macos::frontmost_app_pid()?;
        let window = focused_window(pid);
        (crate::macos::frontmost_app_pid() == Some(pid)).then_some(Self { pid, window })
    }

    /// Проверяет, что впереди та же программа и (если её окна видны через
    /// Accessibility) то же окно.
    ///
    /// Заголовок окна, AXValue и AXSelectedTextRange намеренно не сравниваются:
    /// в браузерах, Electron, мессенджерах и терминалах заголовок и содержимое
    /// меняются на лету (счётчики непрочитанных, вывод в консоли, перерисовка
    /// DOM), из-за чего вставка ложно отказывалась работать.
    pub fn is_current(&self) -> bool {
        if crate::macos::frontmost_app_pid() != Some(self.pid) {
            return false;
        }
        match &self.window {
            Some(expected) => focused_window(self.pid).is_some_and(|now| now == *expected),
            None => true,
        }
    }
}
