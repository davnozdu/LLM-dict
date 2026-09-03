//! Мелочи, специфичные для macOS: иконка в доке и активация окна.

use core_graphics::event::CGEvent;
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

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

/// Читает NSString в обычную строку. Пустую считаем отсутствующей: в истории
/// от неё столько же пользы, сколько от `None`, а веток меньше.
unsafe fn read_nsstring(ptr: *mut AnyObject) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    let utf8: *const std::os::raw::c_char = msg_send![ptr, UTF8String];
    if utf8.is_null() {
        return None;
    }
    let text = std::ffi::CStr::from_ptr(utf8)
        .to_string_lossy()
        .into_owned();
    (!text.is_empty()).then_some(text)
}

/// Программа, которая сейчас впереди.
#[derive(Debug, Clone, Default)]
pub struct FrontmostApp {
    /// Как она называется на языке системы — это и показываем в списке.
    pub name: Option<String>,
    /// Идентификатор бандла: по нему разбираем, браузер это или терминал.
    /// Имя для этого не годится — оно переводится и меняется.
    pub id: Option<String>,
    /// Путь к бандлу: по нему система отдаёт значок.
    pub path: Option<String>,
}

/// Кто впереди — имя, идентификатор и путь разом.
///
/// Одним обращением, а не тремя: спрашивается это на каждое попадание в
/// буфер, а NSWorkspace на каждый вызов заглядывает в чужой процесс.
pub fn frontmost_app() -> FrontmostApp {
    unsafe {
        let mut out = FrontmostApp::default();
        let workspace: *mut AnyObject = msg_send![class!(NSWorkspace), sharedWorkspace];
        if workspace.is_null() {
            return out;
        }
        let app: *mut AnyObject = msg_send![workspace, frontmostApplication];
        if app.is_null() {
            return out;
        }
        let name: *mut AnyObject = msg_send![app, localizedName];
        out.name = read_nsstring(name);
        let id: *mut AnyObject = msg_send![app, bundleIdentifier];
        out.id = read_nsstring(id);
        let url: *mut AnyObject = msg_send![app, bundleURL];
        if !url.is_null() {
            let path: *mut AnyObject = msg_send![url, path];
            out.path = read_nsstring(path);
        }
        out
    }
}

/// Где лежит программа с таким идентификатором. Нужно для старых записей
/// истории: там сохранено только имя и идентификатор, пути ещё нет.
pub fn app_path_for_id(bundle_id: &str) -> Option<String> {
    unsafe {
        let workspace: *mut AnyObject = msg_send![class!(NSWorkspace), sharedWorkspace];
        if workspace.is_null() {
            return None;
        }
        let ns = NSString::from_str(bundle_id);
        let url: *mut AnyObject = msg_send![workspace, URLForApplicationWithBundleIdentifier: &*ns];
        if url.is_null() {
            return None;
        }
        let path: *mut AnyObject = msg_send![url, path];
        read_nsstring(path)
    }
}

/// Значок программы в виде RGBA с домноженной альфой — ровно в том виде, в
/// каком его принимает egui.
///
/// Система отдаёт значок как NSImage с набором представлений разного размера,
/// и вытащить из него пиксели можно только нарисовав. Рисуем в свой растр
/// заданного размера: так не нужно гадать, какое представление досталось и в
/// каком оно формате.
pub fn app_icon_rgba(app_path: &str, size: usize) -> Option<Vec<u8>> {
    objc2::rc::autoreleasepool(|_| unsafe {
        let workspace: *mut AnyObject = msg_send![class!(NSWorkspace), sharedWorkspace];
        if workspace.is_null() {
            return None;
        }
        let ns_path = NSString::from_str(app_path);
        let image: *mut AnyObject = msg_send![workspace, iconForFile: &*ns_path];
        if image.is_null() {
            return None;
        }

        let side = size as isize;
        let space = NSString::from_str("NSDeviceRGBColorSpace");
        let alloc: *mut AnyObject = msg_send![class!(NSBitmapImageRep), alloc];
        let rep: *mut AnyObject = msg_send![
            alloc,
            initWithBitmapDataPlanes: std::ptr::null_mut::<*mut u8>(),
            pixelsWide: side,
            pixelsHigh: side,
            bitsPerSample: 8isize,
            samplesPerPixel: 4isize,
            hasAlpha: true,
            isPlanar: false,
            colorSpaceName: &*space,
            bytesPerRow: 0isize,
            bitsPerPixel: 0isize,
        ];
        // Растр наш, и освободить его надо нам: Retained сделает это на выходе.
        let _owned = objc2::rc::Retained::from_raw(rep)?;

        let context: *mut AnyObject =
            msg_send![class!(NSGraphicsContext), graphicsContextWithBitmapImageRep: rep];
        if context.is_null() {
            return None;
        }
        let _: () = msg_send![class!(NSGraphicsContext), saveGraphicsState];
        let _: () = msg_send![class!(NSGraphicsContext), setCurrentContext: context];
        let dst = NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(size as f64, size as f64),
        );
        // Пустой исходный прямоугольник означает «весь значок целиком».
        let src = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0));
        // 2 — NSCompositingOperationSourceOver.
        let _: () =
            msg_send![image, drawInRect: dst, fromRect: src, operation: 2usize, fraction: 1.0f64];
        let _: () = msg_send![class!(NSGraphicsContext), restoreGraphicsState];

        let data: *const u8 = msg_send![rep, bitmapData];
        if data.is_null() {
            return None;
        }
        // Строка растра бывает длиннее ширины: система вправе её дополнить.
        let stride: isize = msg_send![rep, bytesPerRow];
        if stride < side * 4 {
            return None;
        }
        let mut out = Vec::with_capacity(size * size * 4);
        for row in 0..size {
            let start = data.add(row * stride as usize);
            out.extend_from_slice(std::slice::from_raw_parts(start, size * 4));
        }
        Some(out)
    })
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
