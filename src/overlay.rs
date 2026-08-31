//! Плашка у курсора: показывает, что идёт диктовка или что результат готов.
//!
//! Сделана нативным NSWindow, а не окном egui. Причина простая: eframe не
//! выполняет кадры, пока главное окно скрыто, — а скрыто оно почти всегда.
//! Плашка на окне egui в таком состоянии застывала на экране навсегда.
//!
//! Все вызовы обязаны идти с главного потока.

use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
// Геометрия берётся из objc2: у core-graphics свои типы, собранные под другую
// версию objc2, и через msg_send они не проходят.
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

/// Поверх обычных окон, но ниже системных панелей.
const WINDOW_LEVEL: i64 = 25; // NSStatusWindowLevel
const STYLE_BORDERLESS: usize = 0;
const BACKING_BUFFERED: usize = 2;
/// canJoinAllSpaces | stationary | fullScreenAuxiliary — плашка должна быть
/// видна на любом рабочем столе и поверх полноэкранных программ.
const COLLECTION_BEHAVIOR: usize = (1 << 0) | (1 << 4) | (1 << 8);
const ALIGN_CENTER: i64 = 1;

pub struct Overlay {
    window: *mut AnyObject,
    label: *mut AnyObject,
    visible: bool,
    last_text: String,
}

impl Default for Overlay {
    fn default() -> Self {
        Self::new()
    }
}

impl Overlay {
    pub fn new() -> Self {
        Self {
            window: std::ptr::null_mut(),
            label: std::ptr::null_mut(),
            visible: false,
            last_text: String::new(),
        }
    }

    /// Высота основного экрана: CGEvent отдаёт координаты от верхнего края,
    /// а окна живут в системе координат от нижнего.
    fn primary_screen_height() -> f64 {
        unsafe {
            let screens: *mut AnyObject = msg_send![class!(NSScreen), screens];
            if screens.is_null() {
                return 0.0;
            }
            let count: usize = msg_send![screens, count];
            if count == 0 {
                return 0.0;
            }
            let first: *mut AnyObject = msg_send![screens, objectAtIndex: 0usize];
            let frame: NSRect = msg_send![first, frame];
            frame.size.height
        }
    }

    fn ensure_window(&mut self) {
        if !self.window.is_null() {
            return;
        }
        unsafe {
            let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(180.0, 34.0));
            let alloc: *mut AnyObject = msg_send![class!(NSWindow), alloc];
            let window: *mut AnyObject = msg_send![
                alloc,
                initWithContentRect: rect,
                styleMask: STYLE_BORDERLESS,
                backing: BACKING_BUFFERED,
                defer: false,
            ];
            if window.is_null() {
                return;
            }

            let _: () = msg_send![window, setLevel: WINDOW_LEVEL];
            let _: () = msg_send![window, setOpaque: false];
            let _: () = msg_send![window, setHasShadow: false];
            // Плашка не должна перехватывать мышь: под ней обычная работа,
            // и клики должны проходить насквозь.
            let _: () = msg_send![window, setIgnoresMouseEvents: true];
            let _: () = msg_send![window, setCollectionBehavior: COLLECTION_BEHAVIOR];
            let clear: *mut AnyObject = msg_send![class!(NSColor), clearColor];
            let _: () = msg_send![window, setBackgroundColor: clear];

            // Подпись заодно рисует фон: отдельный вид ради скруглённого
            // прямоугольника заводить незачем.
            let label_alloc: *mut AnyObject = msg_send![class!(NSTextField), alloc];
            let label: *mut AnyObject = msg_send![label_alloc, initWithFrame: rect];
            let _: () = msg_send![label, setBezeled: false];
            let _: () = msg_send![label, setEditable: false];
            let _: () = msg_send![label, setSelectable: false];
            let _: () = msg_send![label, setDrawsBackground: true];
            let _: () = msg_send![label, setAlignment: ALIGN_CENTER];

            let bg: *mut AnyObject = msg_send![
                class!(NSColor),
                colorWithSRGBRed: 0.11f64, green: 0.11f64, blue: 0.13f64, alpha: 1.0f64
            ];
            let fg: *mut AnyObject = msg_send![
                class!(NSColor),
                colorWithSRGBRed: 0.93f64, green: 0.93f64, blue: 0.95f64, alpha: 1.0f64
            ];
            let _: () = msg_send![label, setBackgroundColor: bg];
            let _: () = msg_send![label, setTextColor: fg];
            let font: *mut AnyObject = msg_send![class!(NSFont), systemFontOfSize: 13.0f64];
            let _: () = msg_send![label, setFont: font];

            let _: () = msg_send![label, setWantsLayer: true];
            let layer: *mut AnyObject = msg_send![label, layer];
            if !layer.is_null() {
                let _: () = msg_send![layer, setCornerRadius: 10.0f64];
                let _: () = msg_send![layer, setMasksToBounds: true];
            }

            let _: () = msg_send![window, setContentView: label];

            self.window = window;
            self.label = label;
            log::info!("плашка у курсора создана");
        }
    }

    /// Показывает плашку с текстом рядом с курсором.
    /// `opacity` — от 0 до 1, чтобы плашка гасла плавно.
    pub fn show(&mut self, text: &str, cursor: (f32, f32), opacity: f32) {
        self.ensure_window();
        if self.window.is_null() {
            return;
        }

        unsafe {
            if text != self.last_text {
                let ns = NSString::from_str(text);
                let _: () = msg_send![self.label, setStringValue: &*ns];
                self.last_text = text.to_string();
            }

            // Ширина по длине надписи: обрезанный текст хуже широкой плашки.
            let width = (text.chars().count() as f64 * 7.6 + 34.0).clamp(120.0, 460.0);
            let height = 30.0;
            let flip = Self::primary_screen_height();
            let x = cursor.0 as f64 - width / 2.0;
            let y = flip - cursor.1 as f64 + 22.0;

            let frame = NSRect::new(NSPoint::new(x, y), NSSize::new(width, height));
            let _: () = msg_send![self.window, setFrame: frame, display: true];
            let _: () = msg_send![self.window, setAlphaValue: opacity.clamp(0.0, 1.0) as f64];

            if !self.visible {
                // orderFrontRegardless, а не makeKeyAndOrderFront: фокус должен
                // остаться в программе, куда мы собираемся вставлять текст.
                let _: () = msg_send![self.window, orderFrontRegardless];
                self.visible = true;
            }
        }
    }

    pub fn hide(&mut self) {
        if self.window.is_null() || !self.visible {
            return;
        }
        unsafe {
            let nil: *mut AnyObject = std::ptr::null_mut();
            let _: () = msg_send![self.window, orderOut: nil];
        }
        self.visible = false;
    }
}
