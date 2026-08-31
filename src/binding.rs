//! Сочетание клавиш: хранение, подписи и разбор виртуальных кодов macOS.

use serde::{Deserialize, Serialize};

/// Модификаторы различаются по сторонам: правый ⌘ можно держать под диктовку,
/// не мешая обычным сочетаниям с левым.
pub const K_LEFT_COMMAND: u16 = 55;
pub const K_RIGHT_COMMAND: u16 = 54;
pub const K_LEFT_SHIFT: u16 = 56;
pub const K_RIGHT_SHIFT: u16 = 60;
pub const K_LEFT_OPTION: u16 = 58;
pub const K_RIGHT_OPTION: u16 = 61;
pub const K_LEFT_CONTROL: u16 = 59;
pub const K_RIGHT_CONTROL: u16 = 62;
pub const K_FN: u16 = 63;

/// Переключатель, а не удерживаемая клавиша: в сочетаниях бесполезен.
const CAPS_LOCK: u16 = 57;

/// Все модификаторы, которые можно удерживать. Caps Lock сюда не входит:
/// он переключатель, а не удерживаемая клавиша. Пока он включён, флаг
/// AlphaShift выставлен постоянно, и его код навсегда застревал в наборе
/// зажатых клавиш, приклеиваясь к любому сочетанию.
pub const HOLDABLE_MODIFIERS: [u16; 9] = [
    K_LEFT_COMMAND,
    K_RIGHT_COMMAND,
    K_LEFT_SHIFT,
    K_RIGHT_SHIFT,
    K_LEFT_OPTION,
    K_RIGHT_OPTION,
    K_LEFT_CONTROL,
    K_RIGHT_CONTROL,
    K_FN,
];

pub fn is_modifier(code: u16) -> bool {
    matches!(
        code,
        K_LEFT_COMMAND
            | K_RIGHT_COMMAND
            | K_LEFT_SHIFT
            | K_RIGHT_SHIFT
            | K_LEFT_OPTION
            | K_RIGHT_OPTION
            | K_LEFT_CONTROL
            | K_RIGHT_CONTROL
            | K_FN
    )
}

/// Device-dependent биты CGEventFlags. Общие маски (kCGEventFlagMaskCommand и
/// прочие) не различают стороны, поэтому левый ⌘ выглядел бы как правый.
pub fn modifier_flag_mask(code: u16) -> u64 {
    match code {
        K_LEFT_COMMAND => 0x0000_0008,  // NX_DEVICELCMDKEYMASK
        K_RIGHT_COMMAND => 0x0000_0010, // NX_DEVICERCMDKEYMASK
        K_LEFT_SHIFT => 0x0000_0002,    // NX_DEVICELSHIFTKEYMASK
        K_RIGHT_SHIFT => 0x0000_0004,   // NX_DEVICERSHIFTKEYMASK
        K_LEFT_OPTION => 0x0000_0020,   // NX_DEVICELALTKEYMASK
        K_RIGHT_OPTION => 0x0000_0040,  // NX_DEVICERALTKEYMASK
        K_LEFT_CONTROL => 0x0000_0001,  // NX_DEVICELCTLKEYMASK
        K_RIGHT_CONTROL => 0x0000_2000, // NX_DEVICERCTLKEYMASK
        K_FN => 0x0080_0000,            // kCGEventFlagMaskSecondaryFn
        _ => 0,
    }
}

/// Маска в том виде, в каком её пишет сама macOS в com.apple.symbolichotkeys —
/// стороны там не различаются. Нужна для поиска конфликтов.
pub fn carbon_modifier_mask(code: u16) -> u64 {
    match code {
        K_LEFT_COMMAND | K_RIGHT_COMMAND => 0x0010_0000,
        K_LEFT_SHIFT | K_RIGHT_SHIFT => 0x0002_0000,
        K_LEFT_OPTION | K_RIGHT_OPTION => 0x0008_0000,
        K_LEFT_CONTROL | K_RIGHT_CONTROL => 0x0004_0000,
        K_FN => 0x0080_0000,
        _ => 0,
    }
}

/// Порядок вывода как принято в macOS: ⌃ ⌥ ⇧ ⌘, обычная клавиша последней.
fn sort_rank(code: u16) -> u8 {
    match code {
        K_FN => 0,
        K_LEFT_CONTROL | K_RIGHT_CONTROL => 1,
        K_LEFT_OPTION | K_RIGHT_OPTION => 2,
        K_LEFT_SHIFT | K_RIGHT_SHIFT => 3,
        K_LEFT_COMMAND | K_RIGHT_COMMAND => 5,
        _ => 6,
    }
}

pub fn key_label(code: u16) -> String {
    let named = match code {
        K_LEFT_COMMAND => "левый ⌘",
        K_RIGHT_COMMAND => "правый ⌘",
        K_LEFT_SHIFT => "левый ⇧",
        K_RIGHT_SHIFT => "правый ⇧",
        K_LEFT_OPTION => "левый ⌥",
        K_RIGHT_OPTION => "правый ⌥",
        K_LEFT_CONTROL => "левый ⌃",
        K_RIGHT_CONTROL => "правый ⌃",
        K_FN => "Fn",
        CAPS_LOCK => "Caps Lock",

        0 => "A",
        1 => "S",
        2 => "D",
        3 => "F",
        4 => "H",
        5 => "G",
        6 => "Z",
        7 => "X",
        8 => "C",
        9 => "V",
        11 => "B",
        12 => "Q",
        13 => "W",
        14 => "E",
        15 => "R",
        16 => "Y",
        17 => "T",
        31 => "O",
        32 => "U",
        34 => "I",
        35 => "P",
        37 => "L",
        38 => "J",
        40 => "K",
        45 => "N",
        46 => "M",

        18 => "1",
        19 => "2",
        20 => "3",
        21 => "4",
        22 => "6",
        23 => "5",
        25 => "9",
        26 => "7",
        28 => "8",
        29 => "0",

        24 => "=",
        27 => "-",
        30 => "]",
        33 => "[",
        39 => "'",
        41 => ";",
        42 => "\\",
        43 => ",",
        44 => "/",
        47 => ".",
        50 => "`",

        36 => "Return",
        48 => "Tab",
        49 => "Пробел",
        51 => "Delete",
        53 => "Esc",
        76 => "Enter",
        117 => "Fwd Delete",
        115 => "Home",
        119 => "End",
        116 => "Page Up",
        121 => "Page Down",

        123 => "←",
        124 => "→",
        125 => "↓",
        126 => "↑",

        122 => "F1",
        120 => "F2",
        99 => "F3",
        118 => "F4",
        96 => "F5",
        97 => "F6",
        98 => "F7",
        100 => "F8",
        101 => "F9",
        109 => "F10",
        103 => "F11",
        111 => "F12",
        105 => "F13",
        107 => "F14",
        113 => "F15",
        106 => "F16",
        64 => "F17",
        79 => "F18",
        80 => "F19",
        90 => "F20",

        _ => "",
    };
    if named.is_empty() {
        format!("клавиша #{code}")
    } else {
        named.to_string()
    }
}

/// Сочетание клавиш, которые должны быть зажаты одновременно.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Binding {
    pub keys: Vec<u16>,
}

/// В первых версиях клавиша хранилась одной строкой вроде "RightCommand".
/// Читаем оба формата, иначе старый конфиг не разобрался бы целиком и
/// пользователь молча потерял бы все остальные настройки.
impl<'de> Deserialize<'de> for Binding {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Legacy(String),
            Modern { keys: Vec<u16> },
        }

        Ok(match Raw::deserialize(d)? {
            Raw::Modern { keys } => Binding::new(keys),
            Raw::Legacy(name) => {
                let code = match name.as_str() {
                    "RightCommand" => K_RIGHT_COMMAND,
                    "RightOption" => K_RIGHT_OPTION,
                    "RightControl" => K_RIGHT_CONTROL,
                    "RightShift" => K_RIGHT_SHIFT,
                    "Fn" => K_FN,
                    "F13" => 105,
                    "F14" => 107,
                    "F15" => 113,
                    "F16" => 106,
                    "F17" => 64,
                    "F18" => 79,
                    "F19" => 80,
                    other => {
                        log::warn!("неизвестная клавиша в конфиге: {other}, беру умолчание");
                        return Ok(Binding::default());
                    }
                };
                Binding::new(vec![code])
            }
        })
    }
}

impl Default for Binding {
    fn default() -> Self {
        // Правый ⌘ сам по себе ничего не делает и есть на любой клавиатуре.
        // Fn занят переключением языка ввода, поэтому в умолчания не годится.
        Self {
            keys: vec![K_RIGHT_COMMAND],
        }
    }
}

impl Binding {
    pub fn new(mut keys: Vec<u16>) -> Self {
        // Caps Lock мог попасть в сочетание из-за прежней ошибки разбора.
        // Удерживать его нельзя, поэтому такое сочетание не сработало бы
        // никогда — вычищаем и при чтении настроек, и при наборе.
        keys.retain(|k| *k != CAPS_LOCK);
        keys.sort_by_key(|k| (sort_rank(*k), *k));
        keys.dedup();
        Self { keys }
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn label(&self) -> String {
        if self.keys.is_empty() {
            return "не задано".to_string();
        }
        self.keys
            .iter()
            .map(|k| key_label(*k))
            .collect::<Vec<_>>()
            .join(" + ")
    }

    /// Обычная (не модификатор) клавиша сочетания, если она есть.
    pub fn main_key(&self) -> Option<u16> {
        self.keys.iter().copied().find(|k| !is_modifier(*k))
    }

    /// Суммарная маска модификаторов в формате macOS.
    pub fn carbon_mask(&self) -> u64 {
        self.keys.iter().map(|k| carbon_modifier_mask(*k)).sum()
    }
}
