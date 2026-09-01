//! Конфигурация приложения. Хранится в ~/Library/Application Support/LLM-dict/config.toml
//! API-ключ в конфиг НЕ пишется — он живёт в Keychain (см. `secrets`).

use crate::binding::Binding;
use crate::models::Engine;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const KEYCHAIN_SERVICE: &str = "com.davnozdu.llm-dict";

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
    /// Чем распознавать по умолчанию.
    pub engine: Engine,
    /// На что переключиться, если основной движок отказал.
    /// `None` — не переключаться, показать ошибку.
    pub fallback: Option<Engine>,
    /// OpenAI-совместимый эндпоинт. Для локального сервера поменять на http://localhost:...
    pub base_url: String,
    pub model: String,
    /// Код языка или `auto`. Parakeet его игнорирует — он всегда определяет сам.
    pub language: String,
    /// Подсказка для whisper: имена, термины, стиль пунктуации.
    pub prompt: String,
    /// Выбранная модель для каждого локального движка.
    pub whisper_model: String,
    pub parakeet_model: String,
    /// Загружать локальную модель при запуске, не дожидаясь первой диктовки.
    pub preload_local: bool,
}

impl Default for SttConfig {
    fn default() -> Self {
        Self {
            engine: Engine::Cloud,
            fallback: None,
            base_url: "https://api.groq.com/openai/v1".into(),
            model: "whisper-large-v3-turbo".into(),
            language: "ru".into(),
            prompt: String::new(),
            whisper_model: "whisper-large-v3-turbo-q5".into(),
            parakeet_model: "parakeet-tdt-0.6b-v3-int8".into(),
            preload_local: true,
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
    pub hotkey: Binding,
    pub hotkey_mode: HotKeyMode,
    /// Показывать иконку в доке (иначе только в верхней панели).
    pub show_in_dock: bool,
    /// Звуковой сигнал в начале и конце записи.
    pub play_sounds: bool,
    /// Возвращать в буфер обмена то, что там было до вставки.
    pub restore_clipboard: bool,
    /// Сколько записей диктовки хранить.
    pub history_limit: usize,
    /// Предел длины одной диктовки в секундах. Запись оборвётся сама, даже
    /// если клавишу зажали и забыли.
    pub max_recording_secs: u32,
    /// Обрывать запись после стольких секунд тишины. Ноль — не обрывать.
    pub silence_stop_secs: u32,
    /// Показывать маленький индикатор у курсора во время диктовки.
    pub show_overlay: bool,
    /// Проверять обновления при запуске.
    pub check_updates: bool,
    /// Не пропускать обычную клавишу сочетания дальше в систему.
    ///
    /// Без этого программа получает сочетание, которого не знает, и отвечает
    /// системным писком. Модификаторы пропускаются всегда: диктовка обычно
    /// висит на модификаторе, и его проглатывание сломало бы обычные
    /// сочетания вроде ⌘C.
    pub swallow_hotkey: bool,
    /// Набор действий уже создавался: пустой список после этого — выбор
    /// пользователя, а не первый запуск.
    pub actions_initialised: bool,
    /// Хранить ключ прямо в файле настроек, минуя Keychain.
    ///
    /// ACL записи в связке ключей привязан к подписи приложения, поэтому при
    /// нестабильной подписи macOS считает каждую сборку новой программой и
    /// переспрашивает пароль. Это запасной путь для таких случаев: файл лежит
    /// в домашнем каталоге и правами защищён слабее связки ключей.
    pub key_in_config: bool,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            hotkey: Binding::default(),
            hotkey_mode: HotKeyMode::Hold,
            show_in_dock: false,
            play_sounds: true,
            restore_clipboard: true,
            history_limit: 200,
            max_recording_secs: 60,
            silence_stop_secs: 4,
            show_overlay: true,
            check_updates: true,
            swallow_hotkey: true,
            actions_initialised: false,
            key_in_config: false,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub general: GeneralConfig,
    pub stt: SttConfig,
    pub llm: LlmConfig,
    /// Действия над выделенным текстом со своими сочетаниями клавиш.
    pub actions: Vec<crate::actions::TextAction>,
    /// Заполняется только при включённом `general.key_in_config`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub api_key: String,
    /// Ключи поставщиков, когда они хранятся в файле, а не в связке ключей.
    /// Ключ словаря — имя записи поставщика.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub provider_keys: std::collections::BTreeMap<String, String>,
}

/// Языки: 25 европейских из Parakeet TDT v3 плюс те, что часто нужны и
/// поддерживаются Whisper с облаком. Первым идёт автоопределение.
pub const LANGUAGES: &[(&str, &str)] = &[
    ("auto", "Автоопределение"),
    ("ru", "Русский"),
    ("en", "English"),
    ("uk", "Українська"),
    ("de", "Deutsch"),
    ("fr", "Français"),
    ("es", "Español"),
    ("it", "Italiano"),
    ("pt", "Português"),
    ("nl", "Nederlands"),
    ("pl", "Polski"),
    ("cs", "Čeština"),
    ("sk", "Slovenčina"),
    ("sl", "Slovenščina"),
    ("hr", "Hrvatski"),
    ("bg", "Български"),
    ("ro", "Română"),
    ("hu", "Magyar"),
    ("el", "Ελληνικά"),
    ("sv", "Svenska"),
    ("da", "Dansk"),
    ("fi", "Suomi"),
    ("et", "Eesti"),
    ("lv", "Latviešu"),
    ("lt", "Lietuvių"),
    ("mt", "Malti"),
    ("tr", "Türkçe"),
    ("he", "עברית"),
    ("ar", "العربية"),
    ("zh", "中文"),
    ("ja", "日本語"),
    ("ko", "한국어"),
];

pub fn language_name(code: &str) -> String {
    LANGUAGES
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, name)| name.to_string())
        .unwrap_or_else(|| format!("код «{code}»"))
}

/// Язык не задан — значит автоопределение. Пустую строку из старых конфигов
/// приводим к явному `auto`, чтобы дальше был один вариант вместо двух.
pub fn normalize_language(code: &str) -> &str {
    if code.trim().is_empty() {
        "auto"
    } else {
        code.trim()
    }
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

    /// Приводит поля к каноническому виду после чтения с диска.
    pub fn normalized(mut self) -> Self {
        self.stt.language = normalize_language(&self.stt.language).to_string();
        // Пустой список при первом запуске заполняем набором по умолчанию,
        // а вот осознанно вычищенный трогать нельзя — отсюда флаг.
        if self.actions.is_empty() && !self.general.actions_initialised {
            self.actions = crate::actions::defaults();
            self.general.actions_initialised = true;
        }
        self
    }

    pub fn save(&self) -> Result<()> {
        let dir = config_dir();
        std::fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
        let text = toml::to_string_pretty(self)?;
        std::fs::write(config_path(), text)?;
        Ok(())
    }
}

impl Config {
    /// Читает ключ оттуда, куда его положили настройки.
    pub fn load_api_key(&self) -> String {
        self.key_for("groq_api_key")
    }

    /// Ключ поставщика: из файла настроек или из связки ключей — смотря что
    /// выбрано. Раньше действия ходили только в связку и не видели ключ,
    /// положенный в файл.
    pub fn key_for(&self, account: &str) -> String {
        if self.general.key_in_config {
            if let Some(key) = self.provider_keys.get(account) {
                return key.clone();
            }
            // Ключ Groq лежал в отдельном поле до появления словаря.
            if account == "groq_api_key" {
                return self.api_key.clone();
            }
            return String::new();
        }
        secrets::get(account).unwrap_or_default()
    }

    pub fn set_key_for(&mut self, account: &str, value: &str) -> anyhow::Result<()> {
        if self.general.key_in_config {
            let _ = secrets::set(account, "");
            if value.is_empty() {
                self.provider_keys.remove(account);
            } else {
                self.provider_keys
                    .insert(account.to_string(), value.to_string());
            }
            if account == "groq_api_key" {
                self.api_key = value.to_string();
            }
            Ok(())
        } else {
            self.provider_keys.remove(account);
            if account == "groq_api_key" {
                self.api_key.clear();
            }
            secrets::set(account, value)
        }
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
