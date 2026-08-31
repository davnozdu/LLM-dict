//! Проверка, свободно ли сочетание клавиш.
//!
//! Системные сочетания macOS лежат в com.apple.symbolichotkeys: для каждого
//! записаны код клавиши и маска модификаторов. Читаем их через plutil, чтобы
//! не тащить парсер plist, и сверяем с тем, что набрал пользователь.

use crate::binding::{self, Binding};
use std::collections::HashMap;

/// Человеческие названия для системных сочетаний, которые чаще всего мешают.
/// Остальные показываем по номеру — их десятки, и большинство выключено.
fn symbolic_name(id: i64) -> Option<&'static str> {
    Some(match id {
        7..=10 => "управление фокусом клавиатуры",
        11 => "показать все окна",
        15 => "масштаб экрана",
        27 => "снимок области в буфер",
        28 => "снимок экрана в файл",
        29 => "снимок экрана в буфер",
        30 => "снимок области в файл",
        31 => "снимок области",
        32 => "Mission Control",
        33 => "показать окна программы",
        36 => "показать рабочий стол",
        52 => "инверсия цветов",
        57 => "Launchpad",
        59 => "показать Dock",
        60 => "предыдущий источник ввода",
        61 => "следующий источник ввода",
        64 => "Spotlight",
        65 => "поиск в Finder",
        70 | 71 => "переключение раскладки",
        79..=82 => "переход между рабочими столами",
        98 => "Справка",
        160 => "снимок и запись экрана",
        162 => "быстрые заметки",
        175 => "Notification Center",
        179 => "эмодзи и символы",
        184 => "быстрый переключатель",
        222 => "быстрые заметки",
        _ => return None,
    })
}

/// Сочетания, которые формально свободны в настройках системы, но заняты
/// почти в любом приложении — предупредить о них полезнее, чем промолчать.
fn common_app_shortcut(main_key: u16, mask: u64) -> Option<&'static str> {
    const CMD: u64 = 0x0010_0000;
    const SHIFT: u64 = 0x0002_0000;
    const OPT: u64 = 0x0008_0000;

    let cmd_only = mask == CMD;
    Some(match (main_key, mask) {
        (8, m) if m == CMD => "копировать (⌘C)",
        (9, m) if m == CMD => "вставить (⌘V)",
        (7, m) if m == CMD => "вырезать (⌘X)",
        (6, m) if m == CMD => "отменить (⌘Z)",
        (0, m) if m == CMD => "выделить всё (⌘A)",
        (1, m) if m == CMD => "сохранить (⌘S)",
        (12, m) if m == CMD => "завершить программу (⌘Q)",
        (13, m) if m == CMD => "закрыть окно (⌘W)",
        (17, m) if m == CMD => "новая вкладка (⌘T)",
        (45, m) if m == CMD => "новое окно (⌘N)",
        (3, m) if m == CMD => "поиск (⌘F)",
        (35, m) if m == CMD => "печать (⌘P)",
        (48, m) if m == CMD => "переключение программ (⌘Tab)",
        (49, m) if m == CMD => "Spotlight (⌘Пробел)",
        (20, m) if m == CMD | SHIFT => "снимок экрана (⌘⇧3)",
        (21, m) if m == CMD | SHIFT => "снимок области (⌘⇧4)",
        (23, m) if m == CMD | SHIFT => "снимок и запись (⌘⇧5)",
        (49, m) if m == CMD | OPT => "смена источника ввода",
        (_, _) if cmd_only && (18..=29).contains(&main_key) => "переключение вкладок",
        _ => return None,
    })
}

/// На что назначена клавиша 🌐 в настройках клавиатуры.
enum FnUsage {
    /// «Ничего не делать» — клавиша свободна.
    Nothing,
    Assigned(&'static str),
    /// Ключ не записан, действует умолчание системы.
    Unknown,
}

/// AppleFnUsageType: 0 — ничего, 1 — смена источника ввода,
/// 2 — панель эмодзи, 3 — диктовка Apple.
fn fn_key_usage() -> FnUsage {
    let out = std::process::Command::new("/usr/bin/defaults")
        .args(["read", "com.apple.HIToolbox", "AppleFnUsageType"])
        .output();
    let Ok(out) = out else {
        return FnUsage::Unknown;
    };
    if !out.status.success() {
        return FnUsage::Unknown;
    }
    match String::from_utf8_lossy(&out.stdout).trim().parse::<i32>() {
        Ok(0) => FnUsage::Nothing,
        Ok(1) => FnUsage::Assigned("смену источника ввода"),
        Ok(2) => FnUsage::Assigned("панель эмодзи и символов"),
        Ok(3) => FnUsage::Assigned("диктовку Apple"),
        _ => FnUsage::Unknown,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Ничего похожего не нашлось.
    Free,
    /// Сочетание занято — с описанием, чем именно.
    Taken(String),
    /// Формально свободно, но выбор неудачный.
    Risky(String),
}

/// Читает включённые системные сочетания: id → (keycode, маска модификаторов).
fn system_hotkeys() -> HashMap<i64, (u16, u64)> {
    let mut out = HashMap::new();
    let Some(home) = std::env::var_os("HOME") else {
        return out;
    };
    let path =
        std::path::Path::new(&home).join("Library/Preferences/com.apple.symbolichotkeys.plist");

    let Ok(output) = std::process::Command::new("/usr/bin/plutil")
        .args(["-convert", "json", "-o", "-"])
        .arg(&path)
        .output()
    else {
        return out;
    };
    if !output.status.success() {
        return out;
    }

    let Ok(json) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
        return out;
    };
    let Some(map) = json.get("AppleSymbolicHotKeys").and_then(|v| v.as_object()) else {
        return out;
    };

    for (id, entry) in map {
        if entry.get("enabled").and_then(|v| v.as_bool()) != Some(true) {
            continue;
        }
        let Some(params) = entry
            .pointer("/value/parameters")
            .and_then(|v| v.as_array())
        else {
            continue;
        };
        if params.len() < 3 {
            continue;
        }
        let (Some(keycode), Some(mask)) = (params[1].as_i64(), params[2].as_i64()) else {
            continue;
        };
        // 65535 означает «клавиша не задана, сочетание только из модификаторов».
        if keycode < 0 || keycode > u16::MAX as i64 {
            continue;
        }
        if let Ok(id) = id.parse::<i64>() {
            out.insert(id, (keycode as u16, mask as u64));
        }
    }
    out
}

pub fn check(b: &Binding) -> Verdict {
    if b.is_empty() {
        return Verdict::Taken("сочетание не задано".into());
    }

    // Fn — это глобус. Занята она или нет, зависит от того, что на неё
    // назначено в настройках клавиатуры, а не от самого факта её наличия.
    if b.keys == vec![binding::K_FN] {
        return match fn_key_usage() {
            FnUsage::Nothing => Verdict::Free,
            FnUsage::Unknown => Verdict::Risky(
                "по умолчанию Fn (🌐) переключает источник ввода. Чтобы освободить её, \
                 выберите «Ничего не делать» в Системных настройках → Клавиатура → \
                 «При нажатии 🌐». Либо включите перехват клавиши ниже."
                    .into(),
            ),
            FnUsage::Assigned(what) => Verdict::Risky(format!(
                "Fn (🌐) сейчас назначена на «{what}». Освободить: Системные настройки → \
                 Клавиатура → «При нажатии 🌐» → «Ничего не делать». Либо включите \
                 перехват клавиши ниже."
            )),
        };
    }
    if b.keys == vec![57] {
        return Verdict::Taken(
            "Caps Lock — переключатель, а не удерживаемая клавиша: как часть \
             сочетания он не работает"
                .into(),
        );
    }

    let mask = b.carbon_mask();

    if let Some(main) = b.main_key() {
        for (id, (code, sys_mask)) in system_hotkeys() {
            if code == main && sys_mask == mask {
                let what = symbolic_name(id)
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| format!("системное сочетание #{id}"));
                return Verdict::Taken(format!("занято системой: {what}"));
            }
        }
        if let Some(what) = common_app_shortcut(main, mask) {
            return Verdict::Taken(format!("занято в большинстве программ: {what}"));
        }
        if mask == 0 {
            return Verdict::Taken(
                "одна обычная клавиша без модификаторов — будет срабатывать при наборе текста"
                    .into(),
            );
        }
    } else {
        // Только модификаторы: системных сочетаний из одних модификаторов
        // почти нет, но левые клавиши мешают обычным сочетаниям.
        let uses_left = b.keys.iter().any(|k| {
            matches!(
                *k,
                binding::K_LEFT_COMMAND
                    | binding::K_LEFT_OPTION
                    | binding::K_LEFT_CONTROL
                    | binding::K_LEFT_SHIFT
            )
        });
        if uses_left {
            return Verdict::Risky(
                "левые модификаторы участвуют в обычных сочетаниях — диктовка будет \
                 включаться при ⌘C и подобных. Лучше взять правые."
                    .into(),
            );
        }
    }

    Verdict::Free
}
