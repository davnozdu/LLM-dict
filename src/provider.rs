//! Поставщики языковых моделей.
//!
//! Все четыре говорят на OpenAI-совместимом протоколе, поэтому различаются
//! только адресом и тем, нужен ли ключ. У Gemini для этого есть отдельный
//! совместимый эндпоинт — родной формат Google нам не нужен.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Provider {
    #[default]
    Groq,
    Ollama,
    /// Ollama на их серверах: тот же протокол, но с ключом и большими моделями.
    OllamaCloud,
    Gemini,
    DeepSeek,
    /// Свой адрес: локальный сервер или что-то ещё OpenAI-совместимое.
    Custom,
    /// Модель внутри самого приложения: llama.cpp, без сервера и без сети.
    /// Какая именно — задаётся один раз в настройках, а не у каждого действия.
    Local,
}

impl Provider {
    pub fn label(self) -> &'static str {
        match self {
            Provider::Groq => "Groq",
            Provider::Ollama => "Ollama (локально)",
            Provider::OllamaCloud => "Ollama Cloud",
            Provider::Gemini => "Gemini",
            Provider::DeepSeek => "DeepSeek",
            Provider::Custom => "Свой адрес",
            Provider::Local => "Локальная модель",
        }
    }

    pub fn default_base_url(self) -> &'static str {
        match self {
            Provider::Groq => "https://api.groq.com/openai/v1",
            Provider::Ollama => "http://127.0.0.1:11434/v1",
            Provider::OllamaCloud => "https://ollama.com/v1",
            Provider::Gemini => "https://generativelanguage.googleapis.com/v1beta/openai",
            Provider::DeepSeek => "https://api.deepseek.com/v1",
            Provider::Custom => "",
            // Адреса нет: запрос никуда не уходит.
            Provider::Local => "",
        }
    }

    /// Ollama крутится на своей машине и ключа не спрашивает.
    pub fn needs_key(self) -> bool {
        !matches!(self, Provider::Ollama | Provider::Local)
    }

    /// Имя записи в связке ключей. У каждого поставщика свой ключ.
    pub fn key_account(self) -> &'static str {
        match self {
            Provider::Groq => "groq_api_key",
            Provider::Ollama => "ollama_api_key",
            Provider::OllamaCloud => "ollama_cloud_api_key",
            Provider::Gemini => "gemini_api_key",
            Provider::DeepSeek => "deepseek_api_key",
            Provider::Custom => "custom_api_key",
            Provider::Local => "",
        }
    }

    /// Где взять ключ — чтобы не гадать, куда идти за ним.
    pub fn key_url(self) -> Option<&'static str> {
        match self {
            Provider::Groq => Some("https://console.groq.com/keys"),
            Provider::OllamaCloud => Some("https://ollama.com/settings/keys"),
            Provider::Gemini => Some("https://aistudio.google.com/apikey"),
            Provider::DeepSeek => Some("https://platform.deepseek.com/api_keys"),
            Provider::Ollama | Provider::Custom | Provider::Local => None,
        }
    }

    pub fn default_model(self) -> &'static str {
        match self {
            Provider::Groq => "openai/gpt-oss-120b",
            Provider::Ollama => "llama3.2",
            Provider::OllamaCloud => "gpt-oss:120b",
            Provider::Gemini => "gemini-2.5-flash",
            Provider::DeepSeek => "deepseek-chat",
            Provider::Custom => "",
            // Модель берётся из общей настройки, у действия её нет.
            Provider::Local => "",
        }
    }

    /// Не требует ни сети, ни ключа — работает всегда.
    pub fn is_local(self) -> bool {
        matches!(self, Provider::Local)
    }

    pub const ALL: [Provider; 7] = [
        Provider::Groq,
        Provider::Ollama,
        Provider::OllamaCloud,
        Provider::Gemini,
        Provider::DeepSeek,
        Provider::Custom,
        Provider::Local,
    ];
}

/// Куда обращаться и с каким ключом.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Endpoint {
    pub provider: Provider,
    /// Пусто — берётся адрес по умолчанию для поставщика.
    pub base_url_override: String,
    pub model: String,
}

impl Default for Endpoint {
    fn default() -> Self {
        Self {
            provider: Provider::Groq,
            base_url_override: String::new(),
            model: Provider::Groq.default_model().to_string(),
        }
    }
}

impl Endpoint {
    pub fn base_url(&self) -> String {
        if self.base_url_override.trim().is_empty() {
            self.provider.default_base_url().to_string()
        } else {
            self.base_url_override.trim().to_string()
        }
    }

    /// Ключ берётся через настройки: они знают, лежит он в связке ключей
    /// или в файле.
    pub fn api_key(&self, cfg: &crate::config::Config) -> String {
        if !self.provider.needs_key() {
            return String::new();
        }
        cfg.key_for(self.provider.key_account())
    }

    /// Сменить поставщика: адрес-переопределение и модель от прежнего
    /// к новому не относятся, поэтому сбрасываются.
    pub fn set_provider(&mut self, provider: Provider) {
        if self.provider == provider {
            return;
        }
        self.provider = provider;
        self.base_url_override.clear();
        self.model = provider.default_model().to_string();
    }
}
