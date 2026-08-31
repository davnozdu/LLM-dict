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
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
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
    /// Клавиши, зажатые прямо сейчас. Нужны, чтобы показать в настройках,
    /// что именно доходит до приложения: без этого «не срабатывает» не
    /// отличить от «не доходит».
    held_now: Mutex<Vec<u16>>,
    /// Событий от клавиатуры получено всего. Ноль означает, что тап молчит.
    events_seen: AtomicU64,
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
            held_now: Mutex::new(Vec::new()),
            events_seen: AtomicU64::new(0),
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

    /// Что зажато сейчас и сколько событий тап видел за всё время.
    pub fn diagnostics(&self) -> (Vec<u16>, u64) {
        (
            self.held_now.lock().unwrap().clone(),
            self.events_seen.load(Ordering::Relaxed),
        )
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

/// Решает, что делать по текущему набору зажатых клавиш.
///
/// Вынесено из замыкания тапа отдельно от него: без разрешения
/// «Универсальный доступ» тап не поднимается, и проверить решения вручную
/// нельзя. Здесь же они проверяются тестами.
#[derive(Default)]
pub struct Matcher {
    /// Действие, уже сработавшее на текущем удержании. KeyDown у обычных
    /// клавиш автоповторяется, и без этого действие запускалось бы очередью.
    fired: Option<String>,
}

impl Matcher {
    pub fn decide(
        &mut self,
        held: &[u16],
        is_down: bool,
        dictation: &Binding,
        actions: &[(String, Binding)],
        recording: bool,
        toggle: bool,
    ) -> Vec<HotKeyEvent> {
        let matches = |b: &Binding| !b.is_empty() && b.keys.iter().all(|k| held.contains(k));

        // Сочетания могут вкладываться друг в друга: правый ⌘ под диктовку и
        // правый ⌘ + T под перевод. Побеждает самое длинное подходящее —
        // иначе действие никогда бы не сработало.
        let dictation_len = if matches(dictation) {
            dictation.keys.len()
        } else {
            0
        };
        let best_action = actions
            .iter()
            .filter(|(_, b)| matches(b))
            .max_by_key(|(_, b)| b.keys.len());
        let action_len = best_action.map(|(_, b)| b.keys.len()).unwrap_or(0);

        let mut out = Vec::new();

        if action_len > 0 && action_len >= dictation_len {
            if is_down {
                if let Some((id, _)) = best_action {
                    if self.fired.as_deref() != Some(id.as_str()) {
                        // Диктовка успела начаться по более короткому сочетанию —
                        // выбрасываем запись, пользователь метил в действие.
                        if recording {
                            out.push(HotKeyEvent::CancelRecording);
                        }
                        self.fired = Some(id.clone());
                        out.push(HotKeyEvent::Action(id.clone()));
                    }
                }
            }
            return out;
        }

        // Сочетание разобрано — следующее нажатие сработает снова.
        self.fired = None;

        let active = dictation_len > 0;
        if toggle {
            // В Toggle реагируем только на момент сборки сочетания.
            if active && is_down {
                out.push(if recording {
                    HotKeyEvent::StopRecording
                } else {
                    HotKeyEvent::StartRecording
                });
            }
        } else if active && !recording {
            out.push(HotKeyEvent::StartRecording);
        } else if !active && recording {
            out.push(HotKeyEvent::StopRecording);
        }
        out
    }
}

/// Вид события клавиатуры. Отдельно от CGEventType, чтобы разбор можно было
/// проверить тестами, не поднимая перехватчик.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawKind {
    FlagsChanged,
    KeyDown,
    KeyUp,
    Other,
}

/// Как событие меняет набор зажатых клавиш.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyUpdate {
    /// Полное состояние модификаторов, вычисленное из флагов события.
    Modifiers(Vec<u16>),
    Down(u16),
    Up(u16),
    Ignore,
}

/// Состояние модификаторов берётся целиком из флагов события, а не копится
/// по одному нажатию.
///
/// Так набор не рассыпается, если событие потерялось, и в него не попадает
/// Caps Lock: он переключатель, и пока включён, его флаг выставлен постоянно —
/// код клавиши навсегда застревал в наборе и приклеивался к любому сочетанию.
pub fn classify(kind: RawKind, code: u16, flags: u64) -> KeyUpdate {
    match kind {
        RawKind::FlagsChanged => KeyUpdate::Modifiers(
            binding::HOLDABLE_MODIFIERS
                .iter()
                .copied()
                .filter(|m| flags & binding::modifier_flag_mask(*m) != 0)
                .collect(),
        ),
        RawKind::KeyDown if !binding::is_modifier(code) => KeyUpdate::Down(code),
        RawKind::KeyUp if !binding::is_modifier(code) => KeyUpdate::Up(code),
        _ => KeyUpdate::Ignore,
    }
}

/// Зажатые клавиши: модификаторы и всё остальное учитываются по-разному.
#[derive(Default)]
pub struct HeldKeys {
    modifiers: Vec<u16>,
    regular: BTreeSet<u16>,
}

impl HeldKeys {
    /// Применяет событие. Возвращает набор зажатых клавиш и было ли это
    /// нажатием, либо `None`, если событие нас не касается.
    pub fn apply(&mut self, update: KeyUpdate) -> Option<(Vec<u16>, bool)> {
        let is_down = match update {
            KeyUpdate::Modifiers(mods) => {
                let pressed = mods.len() > self.modifiers.len();
                self.modifiers = mods;
                pressed
            }
            KeyUpdate::Down(code) => {
                self.regular.insert(code);
                true
            }
            KeyUpdate::Up(code) => {
                self.regular.remove(&code);
                false
            }
            KeyUpdate::Ignore => return None,
        };

        let mut all: Vec<u16> = self.modifiers.clone();
        all.extend(self.regular.iter().copied());
        all.sort_unstable();
        Some((all, is_down))
    }
}

/// Запускает поток с event tap. Возвращает `false`, если тап создать не удалось
/// (почти всегда — нет разрешения «Универсальный доступ»).
pub fn spawn(state: Arc<HotKeyState>, tx: Sender<HotKeyEvent>) -> std::thread::JoinHandle<bool> {
    std::thread::spawn(move || {
        // Тап отдаёт события по одному, а нам нужно знать, зажато ли
        // сочетание целиком.
        let pressed: Mutex<HeldKeys> = Mutex::new(HeldKeys::default());
        let capture: Mutex<Capture> = Mutex::new(Capture::default());
        let matcher: Mutex<Matcher> = Mutex::new(Matcher::default());

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

                let kind = match event_type {
                    CGEventType::FlagsChanged => RawKind::FlagsChanged,
                    CGEventType::KeyDown => RawKind::KeyDown,
                    CGEventType::KeyUp => RawKind::KeyUp,
                    _ => RawKind::Other,
                };
                let code = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
                let update = classify(kind, code, event.get_flags().bits());

                let Some((held, is_down)) = pressed.lock().unwrap().apply(update) else {
                    return CallbackResult::Keep;
                };
                cb_state.events_seen.fetch_add(1, Ordering::Relaxed);
                *cb_state.held_now.lock().unwrap() = held.clone();

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

                // Клавиша из любого нашего сочетания — кандидат на
                // проглатывание. Остальная клавиатура не затрагивается.
                let swallow = cb_state.swallow.load(Ordering::Relaxed)
                    && (binding.keys.contains(&code)
                        || actions.iter().any(|(_, b)| b.keys.contains(&code)));

                let recording = cb_state.recording.load(Ordering::Relaxed);
                let toggle = cb_state.mode.load(Ordering::Relaxed) == 1;

                let events = matcher
                    .lock()
                    .unwrap()
                    .decide(&held, is_down, &binding, &actions, recording, toggle);
                for event in events {
                    if let HotKeyEvent::Action(id) = &event {
                        log::info!("сработало действие {id}");
                    }
                    let _ = cb_tx.send(event);
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
    use super::{classify, Binding, Capture, HeldKeys, HotKeyEvent, KeyUpdate, Matcher, RawKind};
    use crate::binding as b;

    /// Флаги события: device-dependent биты нужных модификаторов.
    fn flags(codes: &[u16]) -> u64 {
        codes.iter().map(|c| b::modifier_flag_mask(*c)).sum()
    }

    const CAPS_LOCK_FLAG: u64 = 0x0001_0000;

    const CAPS: u16 = 57;
    const L_CMD: u16 = 55;
    const L_OPT: u16 = 58;
    const R_CMD: u16 = 54;
    const KEY_C: u16 = 8;
    const SPACE: u16 = 49;

    fn actions(pairs: &[(&str, &[u16])]) -> Vec<(String, Binding)> {
        pairs
            .iter()
            .map(|(id, keys)| (id.to_string(), Binding::new(keys.to_vec())))
            .collect()
    }

    /// Сочетание, сохранённое со сломанным Caps Lock, должно чиститься при
    /// чтении: иначе оно не сработало бы уже никогда.
    #[test]
    fn caps_lock_вычищается_из_сочетания() {
        let saved = Binding::new(vec![CAPS, L_OPT, L_CMD, KEY_C]);
        assert_eq!(saved.keys, Binding::new(vec![L_OPT, L_CMD, KEY_C]).keys);
        assert!(!saved.keys.contains(&CAPS));
    }

    /// Caps Lock — переключатель: пока он включён, его флаг выставлен всегда.
    /// В набор зажатых клавиш он попадать не должен, иначе приклеивается
    /// к каждому сочетанию.
    #[test]
    fn caps_lock_не_попадает_в_набор() {
        let mut held = HeldKeys::default();

        // Caps Lock включён, пользователь жмёт ⌥ и ⌘.
        let (keys, _) = held
            .apply(classify(RawKind::FlagsChanged, CAPS, CAPS_LOCK_FLAG))
            .unwrap();
        assert!(keys.is_empty(), "Caps Lock не удерживают, он переключается");

        let (keys, _) = held
            .apply(classify(
                RawKind::FlagsChanged,
                L_OPT,
                CAPS_LOCK_FLAG | flags(&[L_OPT]),
            ))
            .unwrap();
        assert_eq!(keys, vec![L_OPT]);

        let (keys, _) = held
            .apply(classify(
                RawKind::FlagsChanged,
                L_CMD,
                CAPS_LOCK_FLAG | flags(&[L_OPT, L_CMD]),
            ))
            .unwrap();
        assert_eq!(keys, vec![L_CMD, L_OPT]);

        let (keys, is_down) = held
            .apply(classify(
                RawKind::KeyDown,
                KEY_C,
                CAPS_LOCK_FLAG | flags(&[L_OPT, L_CMD]),
            ))
            .unwrap();
        assert_eq!(keys, vec![KEY_C, L_CMD, L_OPT]);
        assert!(is_down);
    }

    /// Потерянное событие отпускания не должно оставлять модификатор навсегда:
    /// состояние берётся из флагов целиком и восстанавливается само.
    #[test]
    fn потерянное_отпускание_восстанавливается() {
        let mut held = HeldKeys::default();
        held.apply(classify(RawKind::FlagsChanged, L_CMD, flags(&[L_CMD])));
        held.apply(classify(
            RawKind::FlagsChanged,
            L_OPT,
            flags(&[L_CMD, L_OPT]),
        ));

        // Событие отпускания ⌘ до нас не дошло, но следующее событие несёт
        // полное состояние — лишний модификатор исчезает сам.
        let (keys, _) = held
            .apply(classify(RawKind::FlagsChanged, L_OPT, flags(&[L_OPT])))
            .unwrap();
        assert_eq!(keys, vec![L_OPT]);
    }

    /// Модификаторы не должны попадать в набор как обычные клавиши.
    #[test]
    fn модификаторы_не_считаются_обычными_клавишами() {
        assert_eq!(classify(RawKind::KeyDown, L_CMD, 0), KeyUpdate::Ignore);
        assert_eq!(classify(RawKind::KeyUp, L_OPT, 0), KeyUpdate::Ignore);
        assert_eq!(classify(RawKind::Other, KEY_C, 0), KeyUpdate::Ignore);
    }

    /// ⌘ + ⌥ + C — сочетание из трёх клавиш, оканчивающееся обычной.
    /// Именно на нём приложение молчало.
    #[test]
    fn действие_из_трёх_клавиш_срабатывает() {
        let mut m = Matcher::default();
        let dict = Binding::new(vec![R_CMD]);
        let acts = actions(&[("перевод", &[L_CMD, L_OPT, KEY_C])]);

        assert!(m
            .decide(&[L_CMD], true, &dict, &acts, false, false)
            .is_empty());
        assert!(m
            .decide(&[L_CMD, L_OPT], true, &dict, &acts, false, false)
            .is_empty());
        assert_eq!(
            m.decide(&[KEY_C, L_CMD, L_OPT], true, &dict, &acts, false, false),
            vec![HotKeyEvent::Action("перевод".into())]
        );
    }

    /// ⌥ + пробел — второе сочетание, на котором ничего не происходило.
    #[test]
    fn действие_из_двух_клавиш_срабатывает() {
        let mut m = Matcher::default();
        let dict = Binding::new(vec![R_CMD]);
        let acts = actions(&[("правка", &[L_OPT, SPACE])]);

        assert!(m
            .decide(&[L_OPT], true, &dict, &acts, false, false)
            .is_empty());
        assert_eq!(
            m.decide(&[L_OPT, SPACE], true, &dict, &acts, false, false),
            vec![HotKeyEvent::Action("правка".into())]
        );
    }

    /// Автоповтор KeyDown не должен запускать действие очередью.
    #[test]
    fn автоповтор_не_повторяет_действие() {
        let mut m = Matcher::default();
        let dict = Binding::new(vec![R_CMD]);
        let acts = actions(&[("перевод", &[L_OPT, SPACE])]);

        assert_eq!(
            m.decide(&[L_OPT, SPACE], true, &dict, &acts, false, false)
                .len(),
            1
        );
        assert!(m
            .decide(&[L_OPT, SPACE], true, &dict, &acts, false, false)
            .is_empty());
        assert!(m
            .decide(&[L_OPT, SPACE], true, &dict, &acts, false, false)
            .is_empty());

        // Отпустили и нажали снова — срабатывает опять.
        m.decide(&[], false, &dict, &acts, false, false);
        assert_eq!(
            m.decide(&[L_OPT, SPACE], true, &dict, &acts, false, false)
                .len(),
            1
        );
    }

    /// Вложенные сочетания: правый ⌘ под диктовку, правый ⌘ + C под действие.
    #[test]
    fn длинное_сочетание_побеждает_короткое() {
        let mut m = Matcher::default();
        let dict = Binding::new(vec![R_CMD]);
        let acts = actions(&[("перевод", &[R_CMD, KEY_C])]);

        assert_eq!(
            m.decide(&[R_CMD], true, &dict, &acts, false, false),
            vec![HotKeyEvent::StartRecording]
        );
        // Запись уже идёт — её надо отменить, пользователь метил в действие.
        assert_eq!(
            m.decide(&[KEY_C, R_CMD], true, &dict, &acts, true, false),
            vec![
                HotKeyEvent::CancelRecording,
                HotKeyEvent::Action("перевод".into())
            ]
        );
    }

    /// Диктовка удержанием продолжает работать как раньше.
    #[test]
    fn диктовка_удержанием_работает() {
        let mut m = Matcher::default();
        let dict = Binding::new(vec![R_CMD]);
        let acts: Vec<(String, Binding)> = Vec::new();

        assert_eq!(
            m.decide(&[R_CMD], true, &dict, &acts, false, false),
            vec![HotKeyEvent::StartRecording]
        );
        assert_eq!(
            m.decide(&[], false, &dict, &acts, true, false),
            vec![HotKeyEvent::StopRecording]
        );
    }

    /// Посторонние клавиши не должны ничего запускать.
    #[test]
    fn чужие_клавиши_игнорируются() {
        let mut m = Matcher::default();
        let dict = Binding::new(vec![R_CMD]);
        let acts = actions(&[("перевод", &[L_CMD, L_OPT, KEY_C])]);

        assert!(m
            .decide(&[L_CMD, KEY_C], true, &dict, &acts, false, false)
            .is_empty());
        assert!(m
            .decide(&[SPACE], true, &dict, &acts, false, false)
            .is_empty());
    }

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
