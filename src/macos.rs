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
