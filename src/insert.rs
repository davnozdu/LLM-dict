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

/// Что лежало в буфере до вставки — чтобы показать в истории и вернуть обратно.
pub fn read_clipboard() -> Option<String> {
    arboard::Clipboard::new().ok()?.get_text().ok()
}

pub fn write_clipboard(text: &str) -> Result<()> {
    arboard::Clipboard::new()?.set_text(text.to_string())?;
    Ok(())
}

fn press_cmd_v() -> Result<()> {
    let source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState)
        .map_err(|_| anyhow::anyhow!("не создать CGEventSource"))?;

    let down = CGEvent::new_keyboard_event(source.clone(), KEYCODE_V, true)
        .map_err(|_| anyhow::anyhow!("не создать событие нажатия"))?;
    // Явно ставим только Command: иначе к вставке прилипнут модификаторы,
    // которые пользователь ещё держит.
    down.set_flags(CGEventFlags::CGEventFlagCommand);
    down.post(CGEventTapLocation::HID);

    std::thread::sleep(Duration::from_millis(12));

    let up = CGEvent::new_keyboard_event(source, KEYCODE_V, false)
        .map_err(|_| anyhow::anyhow!("не создать событие отпускания"))?;
    up.set_flags(CGEventFlags::CGEventFlagCommand);
    up.post(CGEventTapLocation::HID);
    Ok(())
}

/// Вставляет текст под курсором. Возвращает прежнее содержимое буфера.
pub fn insert(text: &str, restore_clipboard: bool) -> Result<Option<String>> {
    let previous = read_clipboard();
    write_clipboard(text)?;

    // Пастборд обновляется асинхронно, без паузы приложение-получатель
    // успевает вставить старое содержимое.
    std::thread::sleep(Duration::from_millis(60));
    press_cmd_v()?;

    if restore_clipboard {
        if let Some(prev) = previous.clone() {
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(400));
                let _ = write_clipboard(&prev);
            });
        }
    }
    Ok(previous)
}

pub fn play_sound(name: &str) {
    let path = format!("/System/Library/Sounds/{name}.aiff");
    let _ = std::process::Command::new("/usr/bin/afplay")
        .arg(path)
        .spawn();
}
