//! Конфигурация приложения. Хранится в ~/Library/Application Support/LLM-dict/config.toml
//! API-ключ в конфиг НЕ пишется — он живёт в Keychain (см. `secrets`).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const KEYCHAIN_SERVICE: &str = "com.davnozdu.llm-dict";

/// Клавиша, удержание которой запускает диктовку.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HotKey {
    RightCommand,
    RightOption,
    RightControl,
    RightShift,
    Fn,
    F13,
    F14,
    F15,
    F16,
    F17,
    F18,
    F19,
}

impl HotKey {
    /// Виртуальный keycode macOS.
    pub fn keycode(self) -> i64 {
        match self {
            HotKey::RightCommand => 54,
            HotKey::RightOption => 61,
            HotKey::RightControl => 62,
            HotKey::RightShift => 60,
            HotKey::Fn => 63,
            HotKey::F13 => 105,
            HotKey::F14 => 107,
            HotKey::F15 => 113,
            HotKey::F16 => 106,
            HotKey::F17 => 64,
            HotKey::F18 => 79,
            HotKey::F19 => 80,
        }
    }

    /// Модификаторы приходят как FlagsChanged, функциональные клавиши — как KeyDown/KeyUp.
    pub fn is_modifier(self) -> bool {
        !matches!(
            self,
            HotKey::F13
                | HotKey::F14
                | HotKey::F15
                | HotKey::F16
                | HotKey::F17
                | HotKey::F18
                | HotKey::F19
        )
    }

    /// Бит в CGEventFlags, по которому определяется нажатие модификатора.
    ///
    /// Берутся device-dependent маски (NX_DEVICE*), а не общие
    /// kCGEventFlagMaskCommand и подобные: общие не различают левую и правую
    /// клавишу, и удержание левого ⌘ выглядело бы как удержание правого.
    pub fn flag_mask(self) -> u64 {
        match self {
            HotKey::RightCommand => 0x0000_0010, // NX_DEVICERCMDKEYMASK
            HotKey::RightOption => 0x0000_0040,  // NX_DEVICERALTKEYMASK
            HotKey::RightControl => 0x0000_2000, // NX_DEVICERCTLKEYMASK
            HotKey::RightShift => 0x0000_0004,   // NX_DEVICERSHIFTKEYMASK
            HotKey::Fn => 0x0080_0000,           // kCGEventFlagMaskSecondaryFn
            _ => 0,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            HotKey::RightCommand => "Правый ⌘",
            HotKey::RightOption => "Правый ⌥",
            HotKey::RightControl => "Правый ⌃",
            HotKey::RightShift => "Правый ⇧",
            HotKey::Fn => "Fn",
            HotKey::F13 => "F13",
            HotKey::F14 => "F14",
            HotKey::F15 => "F15",
            HotKey::F16 => "F16",
            HotKey::F17 => "F17",
            HotKey::F18 => "F18",
            HotKey::F19 => "F19",
        }
    }

    pub const ALL: [HotKey; 12] = [
        HotKey::RightCommand,
        HotKey::RightOption,
        HotKey::RightControl,
        HotKey::RightShift,
        HotKey::Fn,
        HotKey::F13,
        HotKey::F14,
        HotKey::F15,
        HotKey::F16,
        HotKey::F17,
        HotKey::F18,
        HotKey::F19,
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HotKeyMode {
    /// Держим клавишу — пишем, отпустили — распознаём.
    Hold,
    /// Нажали — пишем, нажали ещё раз — распознаём.
    Toggle,
}

/// Что делать с распознанным текстом перед вставкой.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PostMode {
    /// Вставить как есть.
    Raw,
    /// Починить пунктуацию и опечатки, не меняя смысл.
    Correct,
    /// Перевести на целевой язык.
    Translate,
    /// Свой промпт из настроек.
    Custom,
}

impl PostMode {
    pub fn label(self) -> &'static str {
        match self {
            PostMode::Raw => "Без обработки",
            PostMode::Correct => "Корректура",
            PostMode::Translate => "Перевод",
            PostMode::Custom => "Свой промпт",
        }
    }

    pub const ALL: [PostMode; 4] = [
        PostMode::Raw,
        PostMode::Correct,
        PostMode::Translate,
        PostMode::Custom,
    ];
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SttConfig {
    /// OpenAI-совместимый эндпоинт. Для локальной модели поменять на http://localhost:...
    pub base_url: String,
    pub model: String,
    /// Пустая строка — автоопределение.
    pub language: String,
    /// Подсказка для whisper: имена, термины, стиль пунктуации.
    pub prompt: String,
}

impl Default for SttConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.groq.com/openai/v1".into(),
            model: "whisper-large-v3-turbo".into(),
            language: String::new(),
            prompt: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LlmConfig {
    pub base_url: String,
    pub model: String,
    pub mode: PostMode,
    /// Целевой язык для режима «Перевод».
    pub target_language: String,
    pub custom_prompt: String,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.groq.com/openai/v1".into(),
            model: "openai/gpt-oss-120b".into(),
            mode: PostMode::Raw,
            target_language: "English".into(),
            custom_prompt:
                "Перепиши текст в деловом стиле, сохранив смысл. Выведи только результат.".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneralConfig {
    pub hotkey: HotKey,
    pub hotkey_mode: HotKeyMode,
    /// Показывать иконку в доке (иначе только в верхней панели).
    pub show_in_dock: bool,
    /// Звуковой сигнал в начале и конце записи.
    pub play_sounds: bool,
    /// Возвращать в буфер обмена то, что там было до вставки.
    pub restore_clipboard: bool,
    /// Сколько записей диктовки хранить.
    pub history_limit: usize,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            hotkey: HotKey::RightCommand,
            hotkey_mode: HotKeyMode::Hold,
            show_in_dock: false,
            play_sounds: true,
            restore_clipboard: true,
            history_limit: 200,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub general: GeneralConfig,
    pub stt: SttConfig,
    pub llm: LlmConfig,
}

pub fn config_dir() -> PathBuf {
    directories::ProjectDirs::from("com", "davnozdu", "LLM-dict")
        .map(|d| d.config_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".").join(".llm-dict"))
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

impl Config {
    pub fn load() -> Self {
        let path = config_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str(&text) {
                Ok(cfg) => cfg,
                Err(e) => {
                    log::warn!(
                        "не разобрать {}: {e}, беру значения по умолчанию",
                        path.display()
                    );
                    Config::default()
                }
            },
            Err(_) => Config::default(),
        }
    }

    pub fn save(&self) -> Result<()> {
        let dir = config_dir();
        std::fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
        let text = toml::to_string_pretty(self)?;
        std::fs::write(config_path(), text)?;
        Ok(())
    }
}

/// API-ключ в Keychain. В конфиг и в репозиторий он не попадает.
pub mod secrets {
    use super::KEYCHAIN_SERVICE;
    use anyhow::Result;

    fn entry(account: &str) -> Result<keyring::Entry> {
        Ok(keyring::Entry::new(KEYCHAIN_SERVICE, account)?)
    }

    pub fn get(account: &str) -> Option<String> {
        entry(account).ok()?.get_password().ok()
    }

    pub fn set(account: &str, value: &str) -> Result<()> {
        let e = entry(account)?;
        if value.is_empty() {
            let _ = e.delete_credential();
        } else {
            e.set_password(value)?;
        }
        Ok(())
    }
}
