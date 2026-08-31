//! Клиенты к OpenAI-совместимым эндпоинтам.
//!
//! И Groq, и Ollama, и LM Studio, и локальный whisper.cpp-сервер говорят на одном
//! протоколе, поэтому провайдер задаётся одним полем `base_url` в настройках —
//! переход на локальные модели не требует изменений в коде.

use crate::config::{LlmConfig, PostMode, SttConfig};
use anyhow::{anyhow, bail, Result};
use serde::Deserialize;
use std::time::Duration;

fn client() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()?)
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
            "\nКлюч не принят. Проверьте, что он выдан тем же сервисом, \
             что стоит в адресе API ({base_url}). Ключи Groq начинаются с gsk_."
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
    base_url.contains("localhost") || base_url.contains("127.0.0.1") || base_url.contains("0.0.0.0")
}

/// Распознавание речи: POST /audio/transcriptions (multipart).
pub fn transcribe(cfg: &SttConfig, api_key: &str, wav: Vec<u8>) -> Result<String> {
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

fn system_prompt(cfg: &LlmConfig) -> String {
    match cfg.mode {
        PostMode::Raw => String::new(),
        PostMode::Correct => "Ты редактор расшифровок речи. Исправь пунктуацию, регистр и явные \
             ошибки распознавания. Не меняй смысл, не добавляй и не убирай информацию, не отвечай \
             на содержание. Сохрани язык оригинала. Выведи только исправленный текст."
            .to_string(),
        PostMode::Translate => format!(
            "Переведи текст пользователя на язык: {}. Сохрани тон и форматирование. \
             Не комментируй и не отвечай на содержание. Выведи только перевод.",
            cfg.target_language
        ),
        PostMode::Custom => cfg.custom_prompt.clone(),
    }
}

/// Пост-обработка: POST /chat/completions.
pub fn post_process(cfg: &LlmConfig, api_key: &str, text: &str) -> Result<String> {
    if matches!(cfg.mode, PostMode::Raw) || text.trim().is_empty() {
        return Ok(text.to_string());
    }
    require_key(api_key, &cfg.base_url)?;
    let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
    let payload = serde_json::json!({
        "model": cfg.model,
        "temperature": 0.0,
        "messages": [
            { "role": "system", "content": system_prompt(cfg) },
            { "role": "user", "content": text }
        ]
    });

    let mut req = client()?.post(&url).json(&payload);
    if !api_key.is_empty() {
        req = req.bearer_auth(api_key);
    }

    let resp = req.send()?;
    let status = resp.status();
    let body = resp.text()?;
    if !status.is_success() {
        return Err(explain(status, &body, &cfg.base_url));
    }
    let parsed: ChatResponse =
        serde_json::from_str(&body).map_err(|e| anyhow!("неожиданный ответ модели: {e}"))?;
    parsed
        .choices
        .into_iter()
        .next()
        .map(|c| c.message.content.trim().to_string())
        .ok_or_else(|| anyhow!("модель вернула пустой ответ"))
}

/// Один запрос к модели: системный промпт плюс текст пользователя.
/// Используется действиями над выделенным текстом.
pub fn run_prompt(
    endpoint: &crate::provider::Endpoint,
    api_key: &str,
    system_prompt: &str,
    text: &str,
) -> Result<String> {
    if text.trim().is_empty() {
        bail!("нечего обрабатывать: текст пустой");
    }
    let base_url = endpoint.base_url();
    if base_url.trim().is_empty() {
        bail!(
            "не задан адрес API для поставщика {}",
            endpoint.provider.label()
        );
    }
    require_key(api_key, &base_url)?;

    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let payload = serde_json::json!({
        "model": endpoint.model,
        "temperature": 0.0,
        "messages": [
            { "role": "system", "content": system_prompt },
            { "role": "user", "content": text }
        ]
    });

    let mut req = client()?.post(&url).json(&payload);
    if !api_key.is_empty() {
        req = req.bearer_auth(api_key);
    }
    let resp = req.send()?;
    let status = resp.status();
    let body = resp.text()?;
    if !status.is_success() {
        return Err(explain(status, &body, &base_url));
    }
    let parsed: ChatResponse =
        serde_json::from_str(&body).map_err(|e| anyhow!("неожиданный ответ модели: {e}"))?;
    parsed
        .choices
        .into_iter()
        .next()
        .map(|c| c.message.content.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("модель вернула пустой ответ"))
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
