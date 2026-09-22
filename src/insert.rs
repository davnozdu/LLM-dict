//! Вставка текста туда, где стоит курсор.
//!
//! Работает так же, как это делают все диктовки под macOS: текст кладётся в буфер
//! обмена, отправляется синтетическое ⌘V, затем прежнее содержимое буфера
//! возвращается на место. Требует разрешения «Универсальный доступ».

use crate::pasteboard::Snapshot;
use anyhow::Result;
use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use std::sync::{
    atomic::{AtomicI64, Ordering},
    Mutex,
};
use std::time::Duration;

const KEYCODE_V: u16 = 9;
const KEYCODE_C: u16 = 8;
static PASTEBOARD: Mutex<()> = Mutex::new(());
static OUR_CHANGE: AtomicI64 = AtomicI64::new(-1);

pub fn is_our_change(change: i64) -> bool {
    owns_change(OUR_CHANGE.load(Ordering::Relaxed), change)
}

fn owns_change(owned: i64, current: i64) -> bool {
    owned >= 0 && owned == current
}

fn write_unlocked(text: &str) -> Result<i64> {
    arboard::Clipboard::new()?.set_text(text.to_string())?;
    let change = pasteboard_change_count();
    OUR_CHANGE.store(change, Ordering::Relaxed);
    Ok(change)
}

fn restore_unlocked(previous: &Snapshot, expected: i64) -> Result<()> {
    if owns_change(expected, pasteboard_change_count()) {
        if let Some(change) = previous.restore(expected)? {
            OUR_CHANGE.store(change, Ordering::Relaxed);
        }
    }
    Ok(())
}

/// Что лежало в буфере до вставки — чтобы показать в истории и вернуть обратно.
pub fn read_clipboard() -> Option<String> {
    arboard::Clipboard::new().ok()?.get_text().ok()
}

pub fn write_clipboard(text: &str) -> Result<()> {
    let _lock = PASTEBOARD.lock().unwrap();
    write_unlocked(text)?;
    Ok(())
}

/// Счётчик изменений пастборда. По нему видно, что копирование сработало,
/// — сравнивать тексты ненадёжно: пользователь мог скопировать то же самое.
pub fn pasteboard_change_count() -> i64 {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    unsafe {
        let pb: *mut AnyObject = msg_send![class!(NSPasteboard), generalPasteboard];
        if pb.is_null() {
            return -1;
        }
        msg_send![pb, changeCount]
    }
}

fn press_cmd(keycode: u16) -> Result<()> {
    let source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState)
        .map_err(|_| anyhow::anyhow!("не создать CGEventSource"))?;

    let down = CGEvent::new_keyboard_event(source.clone(), keycode, true)
        .map_err(|_| anyhow::anyhow!("не создать событие нажатия"))?;
    down.set_flags(CGEventFlags::CGEventFlagCommand);
    down.post(CGEventTapLocation::HID);

    std::thread::sleep(Duration::from_millis(12));

    let up = CGEvent::new_keyboard_event(source, keycode, false)
        .map_err(|_| anyhow::anyhow!("не создать событие отпускания"))?;
    up.set_flags(CGEventFlags::CGEventFlagCommand);
    up.post(CGEventTapLocation::HID);
    Ok(())
}

/// Забирает выделенный в активной программе текст через ⌘C.
///
/// Своего API для «дай выделенное» в macOS нет, поэтому нажатие копирования
/// приходится изображать. Прежнее содержимое буфера возвращается вызывающим:
/// он сам решает, что положить туда в итоге.
pub fn copy_selection() -> Result<(String, Option<String>)> {
    let _lock = PASTEBOARD.lock().unwrap();
    let snapshot = Snapshot::capture()?;
    let previous = read_clipboard();

    // Две попытки: первая может уйти в момент, когда программа-получатель ещё
    // разбирается с отпущенными модификаторами и копирование пропускает.
    for attempt in 0..2 {
        let before = pasteboard_change_count();
        press_cmd(KEYCODE_C)?;

        // Пастборд обновляется асинхронно: ждём, пока счётчик сдвинется.
        let deadline = std::time::Instant::now() + Duration::from_millis(700);
        while std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
            if pasteboard_change_count() != before {
                let copied = pasteboard_change_count();
                let text = read_clipboard().unwrap_or_default();
                // Cmd+C — временная операция. Возвращаем буфер до запроса к
                // модели, поэтому любой последующий отказ уже не теряет его.
                restore_unlocked(&snapshot, copied)?;
                if text.trim().is_empty() {
                    anyhow::bail!("выделенный текст пустой");
                }
                return Ok((text, previous));
            }
        }
        log::warn!(
            "копирование выделенного не сработало, попытка {}",
            attempt + 1
        );
    }
    anyhow::bail!(
        "не удалось получить выделенный текст. Проверьте, что текст выделен, \
         а приложению выдан «Универсальный доступ»"
    )
}

fn press_cmd_v() -> Result<()> {
    // Явно ставим только Command: иначе к вставке прилипнут модификаторы,
    // которые пользователь ещё держит.
    press_cmd(KEYCODE_V)
}

/// Вставляет текст под курсором. Возвращает прежнее содержимое буфера.
pub fn insert(text: &str, restore_clipboard: bool) -> Result<Option<String>> {
    let _lock = PASTEBOARD.lock().unwrap();
    let snapshot = restore_clipboard.then(Snapshot::capture).transpose()?;
    let previous = read_clipboard();
    paste_unlocked(text, snapshot, None)?;
    Ok(previous)
}

/// При смене приложения результат остаётся в буфере; чужое окно не меняем.
pub fn insert_into(text: &str, restore: bool, target: &crate::focus::Target) -> Result<bool> {
    let _lock = PASTEBOARD.lock().unwrap();
    let previous = restore.then(Snapshot::capture).transpose()?;
    paste_unlocked(text, previous, Some(target))
}

/// То же, но возвращает в буфер заданное значение, а не то, что там было
/// перед вставкой.
///
/// Нужно для замены выделенного: к этому моменту в буфере лежит само
/// выделение, скопированное нами же, а вернуть надо то, что было у
/// пользователя до всей операции.
fn paste_unlocked(
    text: &str,
    restore: Option<Snapshot>,
    target: Option<&crate::focus::Target>,
) -> Result<bool> {
    let change = write_unlocked(text)?;

    // Пастборд обновляется асинхронно, без паузы приложение-получатель
    // успевает вставить старое содержимое.
    std::thread::sleep(Duration::from_millis(60));
    if !owns_change(change, pasteboard_change_count()) {
        anyhow::bail!("буфер изменился до вставки; текст не вставлен");
    }
    if target.is_some_and(|target| !target.is_current()) {
        return Ok(false);
    }
    if let Err(e) = press_cmd_v() {
        if let Some(previous) = &restore {
            let _ = restore_unlocked(previous, change);
        }
        return Err(e);
    }

    if let Some(prev) = restore {
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(400));
            let _lock = PASTEBOARD.lock().unwrap();
            let _ = restore_unlocked(&prev, change);
        });
    }
    Ok(true)
}

pub fn play_sound(name: &str) {
    let path = format!("/System/Library/Sounds/{name}.aiff");
    let _ = std::process::Command::new("/usr/bin/afplay")
        .arg(path)
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::owns_change;

    #[test]
    fn restore_only_our_unchanged_pasteboard() {
        assert!(owns_change(10, 10));
        assert!(!owns_change(10, 11)); // В том числе повторное копирование того же текста.
        assert!(!owns_change(-1, -1));
    }
}
