//! Вставка текста туда, где стоит курсор.
//!
//! Работает так же, как это делают все диктовки под macOS: текст кладётся в буфер
//! обмена, отправляется синтетическое ⌘V, затем прежнее содержимое буфера
//! возвращается на место. Требует разрешения «Универсальный доступ».

use anyhow::Result;
use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use std::time::Duration;

const KEYCODE_V: u16 = 9;
const KEYCODE_C: u16 = 8;

/// Что лежало в буфере до вставки — чтобы показать в истории и вернуть обратно.
pub fn read_clipboard() -> Option<String> {
    arboard::Clipboard::new().ok()?.get_text().ok()
}

pub fn write_clipboard(text: &str) -> Result<()> {
    arboard::Clipboard::new()?.set_text(text.to_string())?;
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
                let text = read_clipboard().unwrap_or_default();
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
    let previous = read_clipboard();
    insert_restoring(
        text,
        if restore_clipboard {
            previous.clone()
        } else {
            None
        },
    )?;
    Ok(previous)
}

/// То же, но возвращает в буфер заданное значение, а не то, что там было
/// перед вставкой.
///
/// Нужно для замены выделенного: к этому моменту в буфере лежит само
/// выделение, скопированное нами же, а вернуть надо то, что было у
/// пользователя до всей операции.
pub fn insert_restoring(text: &str, restore: Option<String>) -> Result<()> {
    write_clipboard(text)?;

    // Пастборд обновляется асинхронно, без паузы приложение-получатель
    // успевает вставить старое содержимое.
    std::thread::sleep(Duration::from_millis(60));
    press_cmd_v()?;

    if let Some(prev) = restore {
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(400));
            let _ = write_clipboard(&prev);
        });
    }
    Ok(())
}

pub fn play_sound(name: &str) {
    let path = format!("/System/Library/Sounds/{name}.aiff");
    let _ = std::process::Command::new("/usr/bin/afplay")
        .arg(path)
        .spawn();
}
