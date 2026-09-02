//! Клиенты к OpenAI-совместимым эндпоинтам.
//!
//! И Groq, и Ollama, и LM Studio, и локальный whisper.cpp-сервер говорят на одном
//! протоколе, поэтому провайдер задаётся одним полем `base_url` в настройках —
//! переход на локальные модели не требует изменений в коде.

use crate::config::SttConfig;
use anyhow::{anyhow, bail, Result};
use serde::Deserialize;
use std::time::Duration;

/// Для загрузки записи: минута речи — это мегабайты, отправка небыстрая.
fn client() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(120))
        .connect_timeout(Duration::from_secs(8))
        .build()?)
}

/// Для запросов к модели по тексту.
///
/// Таймаут короткий намеренно: обработка идёт между распознаванием и
/// вставкой, и если облако не отвечает, лучше быстро вставить текст как есть,
/// чем держать пользователя две минуты.
fn text_client() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(25))
        .connect_timeout(Duration::from_secs(5))
        .build()?)
}

/// Отчего запрос к поставщику не удался.
///
/// Разделение нужно ради отката на локальную модель: подменять облако она
/// должна там, где сервис недоступен, и не должна там, где запрос отвергнут.
/// Иначе протухший ключ выглядел бы как исправно работающая программа, и
/// пользователь никогда не узнал бы, что платит за облако впустую.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// Сервис не отвечает, перегружен или сломался — попробовать локально
    /// осмысленно.
    Unavailable,
    /// Запрос отвергнут: нет ключа, ключ не тот, нет такой модели, кривой
    /// запрос. Локальная модель это не чинит.
    Rejected,
}

/// Отказ запроса вместе с его видом.
#[derive(Debug)]
pub struct PromptError {
    pub kind: Failure,
    pub error: anyhow::Error,
}

impl PromptError {
    fn unavailable(error: anyhow::Error) -> Self {
        Self {
            kind: Failure::Unavailable,
            error,
        }
    }
    fn rejected(error: anyhow::Error) -> Self {
        Self {
            kind: Failure::Rejected,
            error,
        }
    }
    /// Есть ли смысл пробовать локальную модель.
    pub fn worth_local_retry(&self) -> bool {
        self.kind == Failure::Unavailable
    }
}

impl std::fmt::Display for PromptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.error)
    }
}

impl From<PromptError> for anyhow::Error {
    fn from(e: PromptError) -> Self {
        e.error
    }
}

#[derive(Deserialize)]
struct TranscriptionResponse {
    text: String,
}

#[derive(Deserialize)]
struct ApiError {
    error: ApiErrorBody,
}

#[derive(Deserialize)]
struct ApiErrorBody {
    message: String,
}

fn explain(status: reqwest::StatusCode, body: &str, base_url: &str) -> anyhow::Error {
    let detail = serde_json::from_str::<ApiError>(body)
        .map(|e| e.error.message)
        .unwrap_or_else(|_| body.chars().take(300).collect());

    // Голый код ответа ничего не подсказывает. Чаще всего 401 означает не
    // «ключ сломан», а «ключ от другого сервиса»: адрес и ключ настраиваются
    // порознь, и их легко развести.
    let hint = match status.as_u16() {
        401 | 403 => format!(
            "\nКлюч не принят или не сохранён. Проверьте, что он выдан тем же \
             сервисом, что стоит в адресе API ({base_url}), и что вы нажали \
             «Сохранить ключ». Ключи Groq начинаются с gsk_."
        ),
        404 => "\nАдрес API или название модели не найдены — проверьте оба поля.".to_string(),
        429 => "\nПревышен лимит запросов, попробуйте позже.".to_string(),
        _ => String::new(),
    };
    anyhow!("{}: {}{}", status.as_u16(), detail, hint)
}

/// Без ключа запрос уходил без заголовка авторизации, и сервис отвечал
/// «неверный ключ» — сообщение, по которому не догадаешься, что ключа просто нет.
fn require_key(api_key: &str, base_url: &str) -> Result<()> {
    if api_key.trim().is_empty() && !is_local(base_url) {
        bail!("не задан API-ключ: откройте «Настройки» и вставьте его");
    }
    Ok(())
}

/// Локальным серверам (whisper.cpp, Ollama, LM Studio) ключ не нужен.
fn is_local(base_url: &str) -> bool {
    crate::net::is_local_url(base_url)
}

/// Отказ до запроса, когда сети заведомо нет.
///
/// Без этого каждый запрос упирался бы в таймаут: секунды ожидания там, где
/// ответ известен заранее.
fn require_network(base_url: &str) -> Result<()> {
    if !is_local(base_url) && !crate::net::is_online() {
        bail!("нет сети");
    }
    Ok(())
}

/// Распознавание речи: POST /audio/transcriptions (multipart).
pub fn transcribe(cfg: &SttConfig, api_key: &str, wav: Vec<u8>) -> Result<String> {
    require_network(&cfg.base_url)?;
    let url = format!(
        "{}/audio/transcriptions",
        cfg.base_url.trim_end_matches('/')
    );

    let part = reqwest::blocking::multipart::Part::bytes(wav)
        .file_name("audio.wav")
        .mime_str("audio/wav")?;

    let mut form = reqwest::blocking::multipart::Form::new()
        .part("file", part)
        .text("model", cfg.model.clone())
        .text("response_format", "json");

    // «auto» — это отсутствие параметра: сервис определит язык сам.
    let lang = crate::config::normalize_language(&cfg.language);
    if lang != "auto" {
        form = form.text("language", lang.to_string());
    }
    if !cfg.prompt.trim().is_empty() {
        form = form.text("prompt", cfg.prompt.trim().to_string());
    }

    let mut req = client()?.post(&url).multipart(form);
    if !api_key.is_empty() {
        req = req.bearer_auth(api_key);
    }

    let resp = req.send()?;
    let status = resp.status();
    let body = resp.text()?;
    if !status.is_success() {
        return Err(explain(status, &body, &cfg.base_url));
    }
    let parsed: TranscriptionResponse =
        serde_json::from_str(&body).map_err(|e| anyhow!("неожиданный ответ распознавания: {e}"))?;
    Ok(parsed.text.trim().to_string())
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatMessage,
}

#[derive(Deserialize)]
struct ChatMessage {
    content: String,
}

/// Один запрос к модели: системный промпт плюс текст пользователя.
/// Используется действиями над выделенным текстом.
pub fn run_prompt(
    endpoint: &crate::provider::Endpoint,
    api_key: &str,
    system_prompt: &str,
    context: Option<&str>,
    text: &str,
) -> std::result::Result<String, PromptError> {
    if text.trim().is_empty() {
        return Err(PromptError::rejected(anyhow!(
            "нечего обрабатывать: текст пустой"
        )));
    }
    let base_url = endpoint.base_url();
    if base_url.trim().is_empty() {
        return Err(PromptError::rejected(anyhow!(
            "не задан адрес API для поставщика {}",
            endpoint.provider.label()
        )));
    }
    // Сети нет — это ровно тот случай, ради которого локальная модель и есть.
    require_network(&base_url).map_err(PromptError::unavailable)?;
    require_key(api_key, &base_url).map_err(PromptError::rejected)?;

    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));

    // Данные идут отдельным системным сообщением, а не внутри промпта: так
    // видно, где инструкция, а где сведения, и промпт остаётся читаемым.
    let mut messages = vec![serde_json::json!({
        "role": "system", "content": system_prompt
    })];
    if let Some(context) = context.filter(|c| !c.trim().is_empty()) {
        messages.push(serde_json::json!({
            "role": "system",
            "content": format!("Сведения, на которые нужно опираться:\n\n{context}"),
        }));
    }
    messages.push(serde_json::json!({ "role": "user", "content": text }));

    let payload = serde_json::json!({
        "model": endpoint.model,
        "temperature": 0.0,
        "messages": messages,
    });

    let client = text_client().map_err(PromptError::unavailable)?;
    let mut req = client.post(&url).json(&payload);
    if !api_key.is_empty() {
        req = req.bearer_auth(api_key);
    }
    // Отдельное сообщение вместо английского текста от библиотеки: эту
    // ошибку пользователь видит на плашке во время диктовки.
    let resp = req.send().map_err(|e| {
        if e.is_timeout() || e.is_connect() {
            // Помечаем сеть недоступной: следующий запрос откажет сразу,
            // а не через таймаут.
            if !is_local(&base_url) {
                crate::net::mark_offline();
            }
            PromptError::unavailable(anyhow!("{} не отвечает", endpoint.provider.label()))
        } else {
            PromptError::unavailable(anyhow!("{e}"))
        }
    })?;
    let status = resp.status();
    let body = resp
        .text()
        .map_err(|e| PromptError::unavailable(anyhow!("{e}")))?;
    if !status.is_success() {
        let error = explain(status, &body, &base_url);
        // 5xx и 429 — сервису плохо, это пройдёт. Остальное (401, 403, 404,
        // 400) означает, что запрос неверен, и повтор локально его не спасёт,
        // а только скроет поломку от пользователя.
        return Err(if status.is_server_error() || status.as_u16() == 429 {
            PromptError::unavailable(error)
        } else {
            PromptError::rejected(error)
        });
    }
    let parsed: ChatResponse = serde_json::from_str(&body)
        .map_err(|e| PromptError::unavailable(anyhow!("неожиданный ответ модели: {e}")))?;
    parsed
        .choices
        .into_iter()
        .next()
        .map(|c| c.message.content.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| PromptError::unavailable(anyhow!("модель вернула пустой ответ")))
}

/// Настоящая проверка ключа: короткий запрос к модели.
///
/// Список моделей для этого не годится — у некоторых поставщиков, в том числе
/// у Ollama Cloud, он отдаётся вообще без ключа. Кнопка «Считать модели»
/// поэтому срабатывала, а перевод падал с 401.
pub fn verify_key(endpoint: &crate::provider::Endpoint, api_key: &str) -> Result<()> {
    let base_url = endpoint.base_url();
    require_key(api_key, &base_url)?;

    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let payload = serde_json::json!({
        "model": endpoint.model,
        "max_tokens": 1,
        "messages": [{ "role": "user", "content": "ok" }]
    });

    let client = text_client().map_err(PromptError::unavailable)?;
    let mut req = client.post(&url).json(&payload);
    if !api_key.is_empty() {
        req = req.bearer_auth(api_key);
    }
    let resp = req.send()?;
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    let body = resp.text()?;
    Err(explain(status, &body, &base_url))
}

#[derive(Deserialize)]
struct ModelList {
    data: Vec<ModelEntry>,
}

#[derive(Deserialize)]
struct ModelEntry {
    id: String,
}

/// Проверка ключа и заодно список доступных моделей для выпадающих списков.
pub fn list_models(base_url: &str, api_key: &str) -> Result<Vec<String>> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let mut req = client()?.get(&url);
    if !api_key.is_empty() {
        req = req.bearer_auth(api_key);
    }
    let resp = req.send()?;
    let status = resp.status();
    let body = resp.text()?;
    if !status.is_success() {
        return Err(explain(status, &body, base_url));
    }
    let parsed: ModelList =
        serde_json::from_str(&body).map_err(|e| anyhow!("неожиданный ответ /models: {e}"))?;
    let mut ids: Vec<String> = parsed.data.into_iter().map(|m| m.id).collect();
    ids.sort();
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Отвергнутый запрос не должен подменяться локальной моделью.
    ///
    /// Иначе протухший ключ выглядел бы как исправно работающая программа:
    /// текст обрабатывается, ошибки нет, и пользователь не узнаёт, что
    /// облако у него не работает вовсе.
    #[test]
    fn отвергнутый_запрос_не_уходит_на_локальную_модель() {
        for e in [
            PromptError::rejected(anyhow!("401")),
            PromptError::rejected(anyhow!("нет ключа")),
        ] {
            assert!(!e.worth_local_retry(), "{e} не должно уходить локально");
        }
    }

    /// А недоступный сервис — должен: ровно ради этого локальная модель и есть.
    #[test]
    fn недоступный_сервис_уходит_на_локальную_модель() {
        let e = PromptError::unavailable(anyhow!("Groq не отвечает"));
        assert!(e.worth_local_retry());
    }

    /// Классификация по коду ответа: временное — локально, постоянное — нет.
    #[test]
    fn коды_ответа_разделены_верно() {
        let cases = [
            (500u16, true),
            (502, true),
            (503, true),
            (429, true),
            (400, false),
            (401, false),
            (403, false),
            (404, false),
        ];
        for (code, expect_local) in cases {
            let status = reqwest::StatusCode::from_u16(code).unwrap();
            let temporary = status.is_server_error() || status.as_u16() == 429;
            assert_eq!(temporary, expect_local, "код {code} отнесён не туда");
        }
    }
}
