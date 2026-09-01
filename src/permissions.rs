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

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOHIDCheckAccess(request: u32) -> u32;
    fn IOHIDRequestAccess(request: u32) -> bool;
}

/// kIOHIDRequestTypeListenEvent — чтение событий клавиатуры.
const HID_REQUEST_LISTEN: u32 = 1;

const BUNDLE_ID: &str = "com.davnozdu.llm-dict";

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

/// «Мониторинг ввода» — отдельное от «Универсального доступа» разрешение.
///
/// Перехватчик работает на HID-уровне, чтобы получать события раньше других
/// программ, а для этого macOS спрашивает именно его.
pub fn input_monitoring() -> Status {
    // 0 — выдан, 1 — отказано, 2 — не спрашивали.
    match unsafe { IOHIDCheckAccess(HID_REQUEST_LISTEN) } {
        0 => Status::Granted,
        2 => Status::NotAsked,
        _ => Status::Denied,
    }
}

pub fn prompt_input_monitoring() {
    let granted = unsafe { IOHIDRequestAccess(HID_REQUEST_LISTEN) };
    log::info!(
        "мониторинг ввода: {}",
        if granted {
            "выдан"
        } else {
            "отклонён"
        }
    );
}

pub fn open_input_monitoring_settings() {
    let _ =
        open::that("x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent");
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

/// Стирает записи TCC для нашего идентификатора.
///
/// macOS помнит выданный доступ вместе с подписью приложения. Когда подпись
/// сменилась — а при ad-hoc сборках она меняется каждый раз — в списке
/// остаётся запись от прежней сборки: тумблер выглядит включённым, но к
/// текущему процессу отношения не имеет и переключение ничего не даёт.
/// Сброс убирает старые записи, после чего доступ выдаётся заново уже начисто.
pub fn reset_accessibility() -> Result<(), String> {
    let out = std::process::Command::new("/usr/bin/tccutil")
        .args(["reset", "Accessibility", BUNDLE_ID])
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

pub fn reset_microphone() -> Result<(), String> {
    let out = std::process::Command::new("/usr/bin/tccutil")
        .args(["reset", "Microphone", BUNDLE_ID])
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Другие копии приложения с тем же идентификатором.
///
/// Два бандла с одинаковым CFBundleIdentifier, но разной подписью — верный
/// способ получить «доступ выдан, но не работает»: система заводит на них
/// отдельные записи, а пользователь видит один пункт в списке.
pub fn duplicate_bundles() -> Vec<String> {
    let out = std::process::Command::new("/usr/bin/mdfind")
        .arg(format!("kMDItemCFBundleIdentifier == '{BUNDLE_ID}'"))
        .output();
    let Ok(out) = out else { return Vec::new() };
    let current = std::env::current_exe()
        .ok()
        .and_then(|p| {
            p.parent()
                .and_then(|p| p.parent())
                .and_then(|p| p.parent())
                .map(|p| p.to_path_buf())
        })
        .unwrap_or_default();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter(|l| std::path::Path::new(l) != current)
        .map(|l| l.to_string())
        .collect()
}

pub fn open_accessibility_settings() {
    let _ =
        open::that("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility");
}

pub fn open_microphone_settings() {
    let _ =
        open::that("x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone");
}
