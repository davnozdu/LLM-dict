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
            Output::Clipboard => "положить в буфер",
            Output::Replace => "заменить выделенное сразу",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Output::Clipboard => {
                "Результат ложится в буфер обмена, у курсора появляется плашка. \
                 Вставляете сами, ⌘V — удобно, когда ответ идёт в другое окно."
            }
            Output::Replace => {
                "Выделенный текст заменяется результатом на месте, вставлять ничего \
                 не нужно. Удобно для перевода и правки прямо в письме."
            }
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
    /// Прогонять через это действие текст сразу после диктовки, до вставки.
    /// Так надиктованное можно автоматически причёсывать.
    pub after_dictation: bool,
    /// Файл с данными, на которые опирается ответ: расписание, услуги, что
    /// угодно. Читается при каждом запуске, поэтому правки подхватываются
    /// без перезапуска приложения.
    pub context_file: String,
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
            after_dictation: false,
            context_file: String::new(),
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

/// Готовое действие для правки надиктованного.
pub fn correction_action(endpoint: Endpoint) -> TextAction {
    TextAction {
        id: new_id(),
        name: "Правка после диктовки".into(),
        prompt: "Расставь знаки препинания и заглавные буквы, исправь опечатки и \
                 явные ошибки распознавания речи. Не меняй смысл, стиль и язык, \
                 ничего не добавляй и не убирай. Выведи только исправленный текст."
            .into(),
        endpoint,
        hotkey: Binding::new(Vec::new()),
        output: Output::Clipboard,
        enabled: true,
        after_dictation: true,
        context_file: String::new(),
    }
}

/// Ответ на сообщение по своим данным: расписание, услуги, условия.
///
/// Язык ответа задаётся не настройкой, а самим сообщением: спросили
/// по-чешски — ответ по-чешски. Иначе пришлось бы держать отдельное
/// действие на каждый язык.
pub fn answer_action(endpoint: Endpoint) -> TextAction {
    TextAction {
        id: new_id(),
        name: "Ответ по моим данным".into(),
        prompt: "Ниже даны сведения обо мне и о моей работе. Ответь на сообщение \
                 пользователя от моего лица, опираясь только на эти сведения.\n\n\
                 Отвечай на том языке, на котором написано сообщение. Если сведений \
                 для ответа не хватает, скажи об этом прямо и ничего не выдумывай. \
                 Пиши коротко и по делу, выведи только сам ответ без пояснений."
            .into(),
        endpoint,
        hotkey: Binding::new(Vec::new()),
        output: Output::Clipboard,
        enabled: true,
        after_dictation: false,
        context_file: String::new(),
    }
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
            after_dictation: false,
            context_file: String::new(),
        },
        TextAction {
            id: new_id(),
            name: "Перевод на чешский".into(),
            prompt: translate_prompt("чешский"),
            endpoint: endpoint.clone(),
            hotkey: Binding::new(Vec::new()),
            output: Output::Clipboard,
            enabled: true,
            after_dictation: false,
            context_file: String::new(),
        },
        TextAction {
            id: new_id(),
            name: "Перевод на русский".into(),
            prompt: translate_prompt("русский"),
            endpoint: endpoint.clone(),
            hotkey: Binding::new(Vec::new()),
            output: Output::Clipboard,
            enabled: true,
            after_dictation: false,
            context_file: String::new(),
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
            after_dictation: false,
            context_file: String::new(),
        },
    ]
}

/// Больше этого файл с данными не читаем: он уходит в каждый запрос, а
/// оплачивается и обрабатывается по объёму.
const MAX_CONTEXT_BYTES: u64 = 256 * 1024;

impl TextAction {
    /// Читает файл с данными. Перечитывается при каждом запуске, поэтому
    /// правки в файле подхватываются без перезапуска приложения.
    pub fn load_context(&self) -> anyhow::Result<Option<String>> {
        let path = self.context_file.trim();
        if path.is_empty() {
            return Ok(None);
        }
        let meta = std::fs::metadata(path)
            .map_err(|e| anyhow::anyhow!("не открыть файл данных {path}: {e}"))?;
        if meta.len() > MAX_CONTEXT_BYTES {
            anyhow::bail!(
                "файл данных больше {} КБ — он уходит в каждый запрос, сократите его",
                MAX_CONTEXT_BYTES / 1024
            );
        }
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("не прочитать файл данных {path}: {e}"))?;
        Ok(Some(text))
    }
}
