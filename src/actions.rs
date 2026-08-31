//! Действия над выделенным текстом: перевод, корректура и всё, что можно
//! описать промптом. Каждое вешается на своё сочетание клавиш.

use crate::binding::Binding;
use crate::provider::{Endpoint, Provider};
use serde::{Deserialize, Serialize};

/// Что сделать с результатом.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Output {
    /// Положить в буфер обмена и сказать об этом. Вставляет пользователь сам.
    Clipboard,
    /// Заменить выделенный текст прямо на месте.
    Replace,
}

impl Output {
    pub fn label(self) -> &'static str {
        match self {
            Output::Clipboard => "в буфер обмена",
            Output::Replace => "заменить выделенное",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TextAction {
    /// Постоянный идентификатор: сочетания клавиш ссылаются на него,
    /// а имя пользователь может менять сколько угодно.
    pub id: String,
    pub name: String,
    pub prompt: String,
    pub endpoint: Endpoint,
    pub hotkey: Binding,
    pub output: Output,
    pub enabled: bool,
}

impl Default for TextAction {
    fn default() -> Self {
        Self {
            id: new_id(),
            name: "Новое действие".into(),
            prompt: "Перепиши текст пользователя. Выведи только результат.".into(),
            endpoint: Endpoint::default(),
            hotkey: Binding::new(Vec::new()),
            output: Output::Clipboard,
            enabled: true,
        }
    }
}

/// Идентификатор из времени и счётчика: тащить ради этого uuid не стоит,
/// а совпадения здесь исключены — действия создаются руками, по одному.
pub fn new_id() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("act-{now:x}-{n:x}")
}

fn translate_prompt(target: &str) -> String {
    format!(
        "Переведи текст пользователя на {target}. Сохрани тон, форматирование и \
         переносы строк. Не комментируй, не объясняй и не отвечай на содержание — \
         выведи только перевод."
    )
}

/// Набор при первом запуске: пара переводов и корректура.
/// Сочетания клавиш намеренно не заданы — их пользователь назначает сам,
/// иначе мы бы наугад заняли что-то нужное.
pub fn defaults() -> Vec<TextAction> {
    let endpoint = Endpoint {
        provider: Provider::Groq,
        base_url_override: String::new(),
        model: Provider::Groq.default_model().to_string(),
    };
    vec![
        TextAction {
            id: new_id(),
            name: "Перевод на английский".into(),
            prompt: translate_prompt("английский"),
            endpoint: endpoint.clone(),
            hotkey: Binding::new(Vec::new()),
            output: Output::Clipboard,
            enabled: true,
        },
        TextAction {
            id: new_id(),
            name: "Перевод на чешский".into(),
            prompt: translate_prompt("чешский"),
            endpoint: endpoint.clone(),
            hotkey: Binding::new(Vec::new()),
            output: Output::Clipboard,
            enabled: true,
        },
        TextAction {
            id: new_id(),
            name: "Перевод на русский".into(),
            prompt: translate_prompt("русский"),
            endpoint: endpoint.clone(),
            hotkey: Binding::new(Vec::new()),
            output: Output::Clipboard,
            enabled: true,
        },
        TextAction {
            id: new_id(),
            name: "Корректура".into(),
            prompt: "Исправь орфографию, пунктуацию и согласование в тексте пользователя. \
                     Не меняй смысл, стиль и язык, ничего не добавляй и не убирай. \
                     Выведи только исправленный текст."
                .into(),
            endpoint,
            hotkey: Binding::new(Vec::new()),
            output: Output::Replace,
            enabled: true,
        },
    ]
}
