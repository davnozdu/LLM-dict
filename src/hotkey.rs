//! Глобальная горячая клавиша через CGEventTap.
//!
//! Тап живёт в отдельном потоке с собственным CFRunLoop и только слушает события
//! (ListenOnly), ничего не проглатывая — поэтому обычная работа клавиши не ломается.
//! Требует разрешения «Универсальный доступ» (Accessibility).

use crate::config::{HotKey, HotKeyMode};
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
use core_graphics::event::{
    CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
    CallbackResult, EventField,
};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotKeyEvent {
    StartRecording,
    StopRecording,
}

/// Настройки, которые поток тапа перечитывает на лету, без перезапуска.
pub struct HotKeyState {
    keycode: AtomicI64,
    flag_mask: AtomicI64,
    is_modifier: AtomicBool,
    /// 0 = Hold, 1 = Toggle
    mode: AtomicU8,
    /// Идёт ли запись прямо сейчас (нужно для Toggle и для защиты от автоповтора).
    recording: AtomicBool,
}

impl HotKeyState {
    pub fn new(key: HotKey, mode: HotKeyMode) -> Self {
        let s = Self {
            keycode: AtomicI64::new(key.keycode()),
            flag_mask: AtomicI64::new(key.flag_mask() as i64),
            is_modifier: AtomicBool::new(key.is_modifier()),
            mode: AtomicU8::new(0),
            recording: AtomicBool::new(false),
        };
        s.set_mode(mode);
        s
    }

    pub fn set_key(&self, key: HotKey) {
        self.keycode.store(key.keycode(), Ordering::Relaxed);
        self.flag_mask
            .store(key.flag_mask() as i64, Ordering::Relaxed);
        self.is_modifier.store(key.is_modifier(), Ordering::Relaxed);
    }

    pub fn set_mode(&self, mode: HotKeyMode) {
        self.mode
            .store(matches!(mode, HotKeyMode::Toggle) as u8, Ordering::Relaxed);
    }

    pub fn set_recording(&self, v: bool) {
        self.recording.store(v, Ordering::Relaxed);
    }
}

/// Запускает поток с event tap. Возвращает `false`, если тап создать не удалось
/// (почти всегда — нет разрешения Accessibility).
pub fn spawn(state: Arc<HotKeyState>, tx: Sender<HotKeyEvent>) -> std::thread::JoinHandle<bool> {
    std::thread::spawn(move || {
        let cb_state = state.clone();
        let cb_tx = tx.clone();

        let tap = CGEventTap::new(
            CGEventTapLocation::Session,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::ListenOnly,
            vec![
                CGEventType::FlagsChanged,
                CGEventType::KeyDown,
                CGEventType::KeyUp,
            ],
            move |_proxy, event_type, event| {
                let code = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
                let want = cb_state.keycode.load(Ordering::Relaxed);
                if code != want {
                    return CallbackResult::Keep;
                }

                let pressed = if cb_state.is_modifier.load(Ordering::Relaxed) {
                    if !matches!(event_type, CGEventType::FlagsChanged) {
                        return CallbackResult::Keep;
                    }
                    let mask = cb_state.flag_mask.load(Ordering::Relaxed) as u64;
                    (event.get_flags().bits() & mask) != 0
                } else {
                    match event_type {
                        CGEventType::KeyDown => true,
                        CGEventType::KeyUp => false,
                        _ => return CallbackResult::Keep,
                    }
                };

                let toggle = cb_state.mode.load(Ordering::Relaxed) == 1;
                let recording = cb_state.recording.load(Ordering::Relaxed);

                if toggle {
                    // В Toggle реагируем только на нажатие, отпускание игнорируем.
                    if pressed {
                        let _ = cb_tx.send(if recording {
                            HotKeyEvent::StopRecording
                        } else {
                            HotKeyEvent::StartRecording
                        });
                    }
                } else if pressed {
                    // KeyDown у функциональных клавиш автоповторяется — фильтруем.
                    if !recording {
                        let _ = cb_tx.send(HotKeyEvent::StartRecording);
                    }
                } else if recording {
                    let _ = cb_tx.send(HotKeyEvent::StopRecording);
                }

                CallbackResult::Keep
            },
        );

        let tap = match tap {
            Ok(t) => t,
            Err(_) => {
                log::error!("не удалось создать CGEventTap — нет разрешения Accessibility?");
                return false;
            }
        };

        let loop_source = match tap.mach_port().create_runloop_source(0) {
            Ok(s) => s,
            Err(_) => {
                log::error!("не удалось создать runloop source для тапа");
                return false;
            }
        };

        let run_loop = CFRunLoop::get_current();
        unsafe {
            run_loop.add_source(&loop_source, kCFRunLoopCommonModes);
        }
        tap.enable();
        log::info!("event tap запущен");
        CFRunLoop::run_current();
        true
    })
}
