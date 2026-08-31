//! Проверка и запрос системных разрешений macOS.
//!
//! Приложению нужны два: «Универсальный доступ» (чтение горячей клавиши через
//! CGEventTap и отправка синтетического ⌘V) и «Микрофон».

use block2::RcBlock;
use core_foundation::base::TCFType;
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::string::{CFString, CFStringRef};
use objc2::runtime::{AnyClass, Bool};
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

/// Просит систему показать диалог доступа к микрофону.
///
/// Раньше здесь открывалась короткая запись в расчёте на то, что система
/// спросит сама. Диалог асинхронный, поток успевал закрыться раньше ответа,
/// и разрешение срабатывало только со второго раза. Штатный вызов
/// AVCaptureDevice дожидается ответа сам.
pub fn prompt_microphone() {
    let media = NSString::from_str("soun");
    let handler = RcBlock::new(|granted: Bool| {
        log::info!(
            "доступ к микрофону: {}",
            if granted.as_bool() {
                "выдан"
            } else {
                "отклонён"
            }
        );
    });
    unsafe {
        let _: () = msg_send![
            class!(AVCaptureDevice),
            requestAccessForMediaType: &*media,
            completionHandler: &*handler,
        ];
    }
}

pub fn open_accessibility_settings() {
    let _ =
        open::that("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility");
}

pub fn open_microphone_settings() {
    let _ =
        open::that("x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone");
}
