//! Мелочи, специфичные для macOS: иконка в доке и активация окна.

use core_graphics::event::CGEvent;
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};

const POLICY_REGULAR: i64 = 0;
const POLICY_ACCESSORY: i64 = 1;

/// `false` — приложение живёт только в верхней панели, без иконки в доке.
pub fn set_dock_visible(visible: bool) {
    unsafe {
        let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        if app.is_null() {
            return;
        }
        let policy = if visible {
            POLICY_REGULAR
        } else {
            POLICY_ACCESSORY
        };
        let _: bool = msg_send![app, setActivationPolicy: policy];
    }
}

/// Поднять окно поверх остальных при открытии из трея.
pub fn activate() {
    unsafe {
        let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        if app.is_null() {
            return;
        }
        let _: () = msg_send![app, activateIgnoringOtherApps: true];
    }
}

/// Положение указателя мыши в глобальных координатах экрана.
/// Нужно, чтобы поставить индикатор диктовки рядом с курсором.
pub fn cursor_position() -> Option<(f32, f32)> {
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState).ok()?;
    let event = CGEvent::new(source).ok()?;
    let p = event.location();
    Some((p.x as f32, p.y as f32))
}

/// Размер основного экрана в точках. Нужен, чтобы список из буфера целиком
/// помещался на экране, а не уезжал за край вслед за курсором.
pub fn screen_size() -> Option<(f32, f32)> {
    unsafe {
        let screens: *mut AnyObject = msg_send![class!(NSScreen), screens];
        if screens.is_null() {
            return None;
        }
        let count: usize = msg_send![screens, count];
        if count == 0 {
            return None;
        }
        let first: *mut AnyObject = msg_send![screens, objectAtIndex: 0usize];
        let frame: objc2_foundation::NSRect = msg_send![first, frame];
        Some((frame.size.width as f32, frame.size.height as f32))
    }
}

/// Имя программы, которая сейчас впереди. Нужно, чтобы в истории буфера
/// было видно, откуда взят кусок.
pub fn frontmost_app_name() -> Option<String> {
    unsafe {
        let workspace: *mut AnyObject = msg_send![class!(NSWorkspace), sharedWorkspace];
        if workspace.is_null() {
            return None;
        }
        let app: *mut AnyObject = msg_send![workspace, frontmostApplication];
        if app.is_null() {
            return None;
        }
        let name: *mut AnyObject = msg_send![app, localizedName];
        if name.is_null() {
            return None;
        }
        let utf8: *const std::os::raw::c_char = msg_send![name, UTF8String];
        if utf8.is_null() {
            return None;
        }
        Some(
            std::ffi::CStr::from_ptr(utf8)
                .to_string_lossy()
                .into_owned(),
        )
    }
}

/// Идентификатор процесса программы, которая сейчас впереди.
pub fn frontmost_app_pid() -> Option<i32> {
    unsafe {
        let workspace: *mut AnyObject = msg_send![class!(NSWorkspace), sharedWorkspace];
        if workspace.is_null() {
            return None;
        }
        let app: *mut AnyObject = msg_send![workspace, frontmostApplication];
        if app.is_null() {
            return None;
        }
        let pid: i32 = msg_send![app, processIdentifier];
        Some(pid)
    }
}

/// Возвращает фокус программе по её идентификатору процесса.
///
/// Нужно после окна выбора: вставлять надо туда, откуда пользователь пришёл,
/// а не в наше окно.
pub fn activate_app(pid: i32) -> bool {
    unsafe {
        let cls = class!(NSRunningApplication);
        let app: *mut AnyObject = msg_send![cls, runningApplicationWithProcessIdentifier: pid];
        if app.is_null() {
            return false;
        }
        // 0 — активировать обычным порядком, не поднимая все окна программы.
        msg_send![app, activateWithOptions: 0usize]
    }
}
