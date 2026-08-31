//! Клиенты к OpenAI-совместимым эндпоинтам.
//!
//! И Groq, и Ollama, и LM Studio, и локальный whisper.cpp-сервер говорят на одном
//! протоколе, поэтому провайдер задаётся одним полем `base_url` в настройках —
//! переход на локальные модели не требует изменений в коде.

use crate::config::{LlmConfig, PostMode, SttConfig};
use anyhow::{anyhow, Result};
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

fn explain(status: reqwest::StatusCode, body: &str) -> anyhow::Error {
    if let Ok(e) = serde_json::from_str::<ApiError>(body) {
        return anyhow!("{}: {}", status.as_u16(), e.error.message);
    }
    let short: String = body.chars().take(300).collect();
    anyhow!("{}: {}", status.as_u16(), short)
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

    if !cfg.language.trim().is_empty() {
        form = form.text("language", cfg.language.trim().to_string());
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
        return Err(explain(status, &body));
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
        return Err(explain(status, &body));
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
        return Err(explain(status, &body));
    }
    let parsed: ModelList =
        serde_json::from_str(&body).map_err(|e| anyhow!("неожиданный ответ /models: {e}"))?;
    let mut ids: Vec<String> = parsed.data.into_iter().map(|m| m.id).collect();
    ids.sort();
    Ok(ids)
}
