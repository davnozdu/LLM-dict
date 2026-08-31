//! Проверка и запрос системных разрешений macOS.
//!
//! Приложению нужны два: «Универсальный доступ» (чтение горячей клавиши через
//! CGEventTap и отправка синтетического ⌘V) и «Микрофон».

use core_foundation::base::TCFType;
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::string::{CFString, CFStringRef};
use objc2::runtime::AnyClass;
use objc2::{class, msg_send};
use objc2_foundation::NSString;

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> bool;
    static kAXTrustedCheckOptionPrompt: CFStringRef;
}

#[link(name = "AVFoundation", kind = "framework")]
extern "C" {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Granted,
    Denied,
    NotAsked,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Granted => "выдан",
            Status::Denied => "запрещён",
            Status::NotAsked => "не запрашивался",
        }
    }

    pub fn is_ok(self) -> bool {
        matches!(self, Status::Granted)
    }
}

/// Есть ли доступ к «Универсальному доступу». Дёшево, можно звать каждый кадр.
pub fn accessibility() -> Status {
    if unsafe { AXIsProcessTrusted() } {
        Status::Granted
    } else {
        Status::Denied
    }
}

/// Просит систему показать диалог с предложением открыть настройки.
/// Диалог появляется один раз на подпись приложения — дальше только вручную.
pub fn prompt_accessibility() {
    unsafe {
        let key = CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt);
        let value = CFBoolean::true_value();
        let options = CFDictionary::from_CFType_pairs(&[(key, value)]);
        AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef());
    }
}

/// AVAuthorizationStatus: 0 notDetermined, 1 restricted, 2 denied, 3 authorized.
pub fn microphone() -> Status {
    let cls: &AnyClass = class!(AVCaptureDevice);
    // AVMediaTypeAudio — константа со значением "soun".
    let media = NSString::from_str("soun");
    let status: i64 = unsafe { msg_send![cls, authorizationStatusForMediaType: &*media] };
    match status {
        3 => Status::Granted,
        0 => Status::NotAsked,
        _ => Status::Denied,
    }
}

/// Системный диалог микрофона появляется сам при первом открытии устройства,
/// поэтому «запрос» — это короткая пустая запись.
pub fn prompt_microphone() {
    std::thread::spawn(|| {
        let level = std::sync::Arc::new(crate::audio::Level::default());
        if let Ok(rec) = crate::audio::start(level) {
            std::thread::sleep(std::time::Duration::from_millis(300));
            let _ = rec.finish();
        }
    });
}

pub fn open_accessibility_settings() {
    let _ =
        open::that("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility");
}

pub fn open_microphone_settings() {
    let _ =
        open::that("x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone");
}
