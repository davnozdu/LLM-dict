//! Глобальное сочетание клавиш через CGEventTap.
//!
//! Тап живёт в отдельном потоке с собственным CFRunLoop и только слушает события
//! (ListenOnly), ничего не проглатывая — поэтому обычная работа клавиш не ломается.
//! Требует разрешения «Универсальный доступ».

use crate::binding::{self, Binding};
use crate::config::HotKeyMode;
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
use core_graphics::event::{
    CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
    CallbackResult, EventField,
};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotKeyEvent {
    StartRecording,
    StopRecording,
    /// Запись прервана более длинным сочетанием — выбросить, не распознавая.
    CancelRecording,
    /// Сработало действие над текстом; внутри его идентификатор.
    Action(String),
    /// Пользователь набирает новое сочетание в настройках.
    Captured(Vec<u16>),
}

/// Настройки, которые поток тапа перечитывает на лету, без перезапуска.
pub struct HotKeyState {
    binding: Mutex<Binding>,
    /// Сочетания действий над текстом: идентификатор действия и его клавиши.
    actions: Mutex<Vec<(String, Binding)>>,
    /// 0 = Hold, 1 = Toggle
    mode: AtomicU8,
    /// Идёт ли запись прямо сейчас.
    recording: AtomicBool,
    /// Режим набора сочетания: события уходят в UI, диктовка не запускается.
    capturing: AtomicBool,
    /// Проглатывать события клавиш сочетания, не пропуская их в систему.
    swallow: AtomicBool,
    /// Система отключила тап — обычно за слишком долгий обработчик.
    /// Дальше горячая клавиша молча не работает, поэтому это надо показать.
    disabled_by_system: AtomicBool,
}

impl HotKeyState {
    pub fn new(binding: Binding, mode: HotKeyMode) -> Self {
        let s = Self {
            binding: Mutex::new(binding),
            actions: Mutex::new(Vec::new()),
            mode: AtomicU8::new(0),
            recording: AtomicBool::new(false),
            capturing: AtomicBool::new(false),
            swallow: AtomicBool::new(false),
            disabled_by_system: AtomicBool::new(false),
        };
        s.set_mode(mode);
        s
    }

    pub fn set_binding(&self, b: Binding) {
        *self.binding.lock().unwrap() = b;
    }

    pub fn set_actions(&self, actions: Vec<(String, Binding)>) {
        *self.actions.lock().unwrap() = actions;
    }

    pub fn set_mode(&self, mode: HotKeyMode) {
        self.mode
            .store(matches!(mode, HotKeyMode::Toggle) as u8, Ordering::Relaxed);
    }

    pub fn set_recording(&self, v: bool) {
        self.recording.store(v, Ordering::Relaxed);
    }

    pub fn set_capturing(&self, v: bool) {
        self.capturing.store(v, Ordering::Relaxed);
    }

    pub fn is_capturing(&self) -> bool {
        self.capturing.load(Ordering::Relaxed)
    }

    pub fn set_swallow(&self, v: bool) {
        self.swallow.store(v, Ordering::Relaxed);
    }

    pub fn is_disabled_by_system(&self) -> bool {
        self.disabled_by_system.load(Ordering::Relaxed)
    }
}

/// Набор сочетания: копит самый полный вариант из тех, что пользователь
/// удерживал. Вынесено из замыкания тапа, чтобы поведение можно было проверить
/// тестами — именно здесь терялась третья клавиша.
#[derive(Default)]
pub struct Capture {
    best: Vec<u16>,
}

impl Capture {
    pub fn reset(&mut self) {
        self.best.clear();
    }

    /// Возвращает набор, который надо показать пользователю, если он изменился
    /// или его стоит подтвердить повторно.
    pub fn update(&mut self, held: &[u16], is_down: bool) -> Option<Vec<u16>> {
        if held.is_empty() {
            // Всё отпущено. Набранное не стираем — повторяем его, чтобы окно
            // наверняка получило полный вариант, а не огрызок от
            // неодновременного отпускания клавиш.
            return (!self.best.is_empty()).then(|| self.best.clone());
        }
        // Первое нажатие после полного отпускания начинает набор заново: иначе
        // нельзя было бы сменить сочетание из трёх клавиш на сочетание из двух.
        if is_down && held.len() == 1 {
            self.best.clear();
        }
        if held.len() >= self.best.len() {
            self.best = held.to_vec();
            return Some(self.best.clone());
        }
        None
    }
}

/// Разбирает событие в «код клавиши + нажата ли она».
/// `None` — событие не про клавишу, которую мы умеем отслеживать.
fn decode(event_type: CGEventType, event: &core_graphics::event::CGEvent) -> Option<(u16, bool)> {
    let code = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
    match event_type {
        CGEventType::FlagsChanged => {
            let mask = binding::modifier_flag_mask(code);
            if mask == 0 {
                return None;
            }
            Some((code, (event.get_flags().bits() & mask) != 0))
        }
        CGEventType::KeyDown => Some((code, true)),
        CGEventType::KeyUp => Some((code, false)),
        _ => None,
    }
}

/// Запускает поток с event tap. Возвращает `false`, если тап создать не удалось
/// (почти всегда — нет разрешения «Универсальный доступ»).
pub fn spawn(state: Arc<HotKeyState>, tx: Sender<HotKeyEvent>) -> std::thread::JoinHandle<bool> {
    std::thread::spawn(move || {
        // Множество зажатых клавиш. Тап отдаёт события по одной, а нам нужно
        // знать, зажато ли сочетание целиком.
        let pressed: Mutex<BTreeSet<u16>> = Mutex::new(BTreeSet::new());
        let capture: Mutex<Capture> = Mutex::new(Capture::default());

        let cb_state = state.clone();
        let cb_tx = tx.clone();

        let tap = CGEventTap::new(
            CGEventTapLocation::Session,
            CGEventTapPlacement::HeadInsertEventTap,
            // Default, а не ListenOnly: только так можно проглотить событие,
            // когда включён перехват. Без перехвата всё возвращается как есть.
            CGEventTapOptions::Default,
            vec![
                CGEventType::FlagsChanged,
                CGEventType::KeyDown,
                CGEventType::KeyUp,
            ],
            move |_proxy, event_type, event| {
                if matches!(
                    event_type,
                    CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput
                ) {
                    log::error!("система отключила event tap: {event_type:?}");
                    cb_state.disabled_by_system.store(true, Ordering::Relaxed);
                    return CallbackResult::Keep;
                }

                let Some((code, is_down)) = decode(event_type, event) else {
                    return CallbackResult::Keep;
                };

                let held: Vec<u16> = {
                    let mut set = pressed.lock().unwrap();
                    if is_down {
                        set.insert(code);
                    } else {
                        set.remove(&code);
                    }
                    set.iter().copied().collect()
                };

                if cb_state.is_capturing() {
                    if let Some(keys) = capture.lock().unwrap().update(&held, is_down) {
                        log::info!("набрано сочетание: {keys:?}");
                        let _ = cb_tx.send(HotKeyEvent::Captured(keys));
                    }
                    return CallbackResult::Keep;
                }
                capture.lock().unwrap().reset();

                let binding = cb_state.binding.lock().unwrap().clone();
                let actions = cb_state.actions.lock().unwrap().clone();

                let matches =
                    |b: &Binding| !b.is_empty() && b.keys.iter().all(|k| held.contains(k));

                // Сочетания могут вкладываться друг в друга: правый ⌘ под
                // диктовку и правый ⌘ + T под перевод. Побеждает самое длинное
                // подходящее — иначе действие никогда бы не сработало.
                let dictation_len = if matches(&binding) {
                    binding.keys.len()
                } else {
                    0
                };
                let best_action = actions
                    .iter()
                    .filter(|(_, b)| matches(b))
                    .max_by_key(|(_, b)| b.keys.len());
                let action_len = best_action.map(|(_, b)| b.keys.len()).unwrap_or(0);

                // Клавиша из любого нашего сочетания — кандидат на проглатывание.
                // Остальная клавиатура не затрагивается.
                let swallow = cb_state.swallow.load(Ordering::Relaxed)
                    && (binding.keys.contains(&code)
                        || actions.iter().any(|(_, b)| b.keys.contains(&code)));

                let recording = cb_state.recording.load(Ordering::Relaxed);
                let toggle = cb_state.mode.load(Ordering::Relaxed) == 1;

                if action_len > 0 && action_len >= dictation_len {
                    if is_down {
                        // Диктовка успела начаться по более короткому сочетанию —
                        // выбрасываем запись, пользователь метил в действие.
                        if recording {
                            let _ = cb_tx.send(HotKeyEvent::CancelRecording);
                        }
                        if let Some((id, _)) = best_action {
                            let _ = cb_tx.send(HotKeyEvent::Action(id.clone()));
                        }
                    }
                } else {
                    let active = dictation_len > 0;
                    if toggle {
                        // В Toggle реагируем только на момент сборки сочетания.
                        if active && is_down {
                            let _ = cb_tx.send(if recording {
                                HotKeyEvent::StopRecording
                            } else {
                                HotKeyEvent::StartRecording
                            });
                        }
                    } else if active && !recording {
                        let _ = cb_tx.send(HotKeyEvent::StartRecording);
                    } else if !active && recording {
                        let _ = cb_tx.send(HotKeyEvent::StopRecording);
                    }
                }

                if swallow {
                    CallbackResult::Drop
                } else {
                    CallbackResult::Keep
                }
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


#[cfg(test)]
mod tests {
    use super::Capture;

    /// Три клавиши, нажатые по очереди, должны дойти все.
    #[test]
    fn ловит_три_клавиши() {
        let mut c = Capture::default();
        assert_eq!(c.update(&[54], true), Some(vec![54]));
        assert_eq!(c.update(&[54, 60], true), Some(vec![54, 60]));
        assert_eq!(c.update(&[14, 54, 60], true), Some(vec![14, 54, 60]));
    }

    /// Клавиши отпускаются вразнобой — итог не должен рассыпаться.
    #[test]
    fn отпускание_не_обрезает_набор() {
        let mut c = Capture::default();
        c.update(&[54], true);
        c.update(&[54, 60], true);
        c.update(&[14, 54, 60], true);

        assert_eq!(c.update(&[54, 60], false), None);
        assert_eq!(c.update(&[54], false), None);
        // Финальное подтверждение полного набора.
        assert_eq!(c.update(&[], false), Some(vec![14, 54, 60]));
    }

    /// После полного отпускания новый набор начинается с нуля,
    /// иначе с трёх клавиш нельзя было бы вернуться к двум.
    #[test]
    fn новый_набор_может_быть_короче() {
        let mut c = Capture::default();
        c.update(&[54], true);
        c.update(&[54, 60], true);
        c.update(&[14, 54, 60], true);
        c.update(&[], false);

        assert_eq!(c.update(&[61], true), Some(vec![61]));
        assert_eq!(c.update(&[17, 61], true), Some(vec![17, 61]));
        assert_eq!(c.update(&[], false), Some(vec![17, 61]));
    }

    #[test]
    fn сброс_очищает_набор() {
        let mut c = Capture::default();
        c.update(&[54, 60], true);
        c.reset();
        assert_eq!(c.update(&[], false), None);
    }
}
