//! Мелочи, специфичные для macOS: иконка в доке и активация окна.

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
