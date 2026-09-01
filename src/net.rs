//! Есть ли сеть.
//!
//! Проверяется системным механизмом доступности, а не запросом: он отвечает
//! мгновенно и не создаёт трафика. Нужно, чтобы не ждать таймаута там, где
//! заранее известно, что облако недоступно, — при пропавшей сети диктовка
//! иначе висит секунды на каждом запросе.

use std::ffi::CString;
use std::os::raw::{c_char, c_void};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[link(name = "SystemConfiguration", kind = "framework")]
extern "C" {
    fn SCNetworkReachabilityCreateWithName(
        allocator: *const c_void,
        nodename: *const c_char,
    ) -> *mut c_void;
    fn SCNetworkReachabilityGetFlags(target: *mut c_void, flags: *mut u32) -> bool;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(cf: *mut c_void);
}

const REACHABLE: u32 = 1 << 1;
const CONNECTION_REQUIRED: u32 = 1 << 2;

/// Узел для проверки. Конкретный адрес неважен: механизм отвечает по
/// состоянию сетевых интерфейсов и маршрутов, а не по доступности хоста.
const PROBE_HOST: &str = "api.groq.com";

static ONLINE: AtomicBool = AtomicBool::new(true);
static CHECKED_AT: AtomicU64 = AtomicU64::new(0);

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn probe() -> bool {
    let Ok(host) = CString::new(PROBE_HOST) else {
        return true;
    };
    unsafe {
        let target = SCNetworkReachabilityCreateWithName(std::ptr::null(), host.as_ptr());
        if target.is_null() {
            // Не смогли спросить — считаем, что сеть есть: лучше попробовать
            // и получить внятную ошибку, чем запретить работу на догадке.
            return true;
        }
        let mut flags: u32 = 0;
        let ok = SCNetworkReachabilityGetFlags(target, &mut flags);
        CFRelease(target);
        if !ok {
            return true;
        }
        flags & REACHABLE != 0 && flags & CONNECTION_REQUIRED == 0
    }
}

/// Есть ли сеть. Ответ кешируется на пару секунд: проверка дешёвая, но
/// вызывается на каждый запрос.
pub fn is_online() -> bool {
    let now = now_secs();
    if now.saturating_sub(CHECKED_AT.load(Ordering::Relaxed)) >= 2 {
        let online = probe();
        let was = ONLINE.swap(online, Ordering::Relaxed);
        CHECKED_AT.store(now, Ordering::Relaxed);
        if was != online {
            log::info!(
                "сеть {}",
                if online {
                    "появилась"
                } else {
                    "пропала"
                }
            );
        }
    }
    ONLINE.load(Ordering::Relaxed)
}

/// Помечает сеть недоступной, не дожидаясь следующей проверки.
///
/// Вызывается, когда запрос упал по таймауту или отказу соединения: система
/// может считать сеть доступной, а до облака всё равно не достучаться.
pub fn mark_offline() {
    if ONLINE.swap(false, Ordering::Relaxed) {
        log::info!("сеть помечена недоступной после отказа запроса");
    }
    CHECKED_AT.store(now_secs(), Ordering::Relaxed);
}

/// Локальный адрес работает и без сети.
pub fn is_local_url(url: &str) -> bool {
    url.contains("localhost") || url.contains("127.0.0.1") || url.contains("0.0.0.0")
}
