//! OpenAI-совместимый эндпоинт поверх локальной модели.
//!
//! Модель и так живёт в памяти приложения — отдать её другим программам стоит
//! одного потока. Формат ответов повторяет OpenAI, поэтому клиенты работают
//! без правок: достаточно указать адрес и ключ.
//!
//! Слушаем только `127.0.0.1`. Ключ обязателен даже там: к локальному адресу
//! дотягивается любая программа на этой машине, включая вкладку браузера.

use crate::engine::{Shared, Stage};
use crate::local_llm::Turn;
use serde::Deserialize;
use std::io::Cursor;
use std::sync::Arc;
use tiny_http::{Header, Request, Response, Server};

#[derive(Deserialize)]
struct ChatRequest {
    #[serde(default)]
    messages: Vec<Message>,
    #[serde(default)]
    stream: bool,
    #[serde(default)]
    max_tokens: Option<usize>,
}

#[derive(Deserialize)]
struct Message {
    role: String,
    /// Содержимое бывает и строкой, и списком кусков — так его шлют клиенты,
    /// умеющие картинки. Картинки нам не нужны, но запрос от такого клиента
    /// не должен разваливаться на разборе.
    content: Content,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Content {
    Text(String),
    Parts(Vec<Part>),
}

#[derive(Deserialize)]
struct Part {
    #[serde(default)]
    text: String,
}

impl Content {
    fn into_text(self) -> String {
        match self {
            Content::Text(t) => t,
            Content::Parts(parts) => parts
                .into_iter()
                .map(|p| p.text)
                .collect::<Vec<_>>()
                .join(""),
        }
    }
}

/// Поднимает сервер, если он включён в настройках.
///
/// Порт читается один раз: менять его на лету значило бы держать возможность
/// уронить чужие соединения из окна настроек. После правки — перезапуск.
pub fn spawn(shared: Arc<Shared>) {
    if !shared.config_snapshot().server.enabled {
        return;
    }
    start(shared);
}

/// Поднимает сервер независимо от настройки `enabled`.
///
/// Нужен режиму `--serve`: проверить эндпоинт, не включая его насовсем и не
/// трогая настройки работающего приложения.
pub fn start(shared: Arc<Shared>) {
    let cfg = shared.config_snapshot();
    let addr = format!("127.0.0.1:{}", cfg.server.port);
    let server = match Server::http(&addr) {
        Ok(s) => s,
        Err(e) => {
            log::error!("не поднять локальный эндпоинт на {addr}: {e}");
            shared.notify(format!("Не занять порт {}: {e}", cfg.server.port));
            return;
        }
    };
    log::info!("локальный эндпоинт слушает http://{addr}/v1");

    std::thread::spawn(move || {
        for request in server.incoming_requests() {
            let shared = shared.clone();
            // Каждый запрос в своём потоке, но модель под общим мьютексом:
            // очередь всё равно одна, зато медленный клиент не мешает
            // остальным получить отказ.
            std::thread::spawn(move || {
                if let Err(e) = handle(&shared, request) {
                    log::warn!("локальный эндпоинт: {e}");
                }
            });
        }
    });
}

/// Новый ключ доступа. Не криптостойкий генератор, но и задача не та:
/// ключ защищает от случайного обращения соседней программы, а не от
/// подбора — снаружи машины к порту не достучаться.
pub fn new_key() -> String {
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut x = seed as u64 ^ (std::process::id() as u64) << 32 | 0x9E37_79B9_7F4A_7C15;
    let mut out = String::from("ld-");
    for _ in 0..32 {
        // xorshift: короткий, предсказуемый по коду и без новых зависимостей.
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let n = (x % 36) as u8;
        out.push(if n < 10 {
            (b'0' + n) as char
        } else {
            (b'a' + n - 10) as char
        });
    }
    out
}

fn json_header() -> Header {
    Header::from_bytes(
        &b"Content-Type"[..],
        &b"application/json; charset=utf-8"[..],
    )
    .expect("заголовок постоянный")
}

fn error_response(code: u16, message: &str) -> Response<Cursor<Vec<u8>>> {
    let body = serde_json::json!({
        "error": { "message": message, "type": "invalid_request_error" }
    })
    .to_string();
    Response::from_string(body)
        .with_status_code(code)
        .with_header(json_header())
}

fn handle(shared: &Arc<Shared>, request: Request) -> anyhow::Result<()> {
    let cfg = shared.config_snapshot();
    let url = request.url().to_string();
    let path = url.split('?').next().unwrap_or("").to_string();

    // Ключ проверяем до всего остального, чтобы по ответам нельзя было
    // выяснить, какая модель установлена.
    let expected = cfg.server.api_key.trim().to_string();
    let given = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Authorization"))
        .map(|h| h.value.as_str().trim().to_string())
        .unwrap_or_default();
    let given = given.strip_prefix("Bearer ").unwrap_or(&given).trim();
    if expected.is_empty() || given != expected {
        return Ok(request.respond(error_response(401, "неверный ключ"))?);
    }

    let model_id = cfg.local_llm.model.trim().to_string();
    if model_id.is_empty() {
        return Ok(request.respond(error_response(
            503,
            "локальная модель не выбрана в настройках приложения",
        ))?);
    }

    match (request.method().as_str(), path.as_str()) {
        ("GET", "/v1/models") => {
            let title = crate::models::find(&model_id)
                .map(|m| m.title)
                .unwrap_or("неизвестная модель");
            let body = serde_json::json!({
                "object": "list",
                "data": [{
                    "id": model_id,
                    "object": "model",
                    "owned_by": "llm-dict",
                    "created": now(),
                    "name": title,
                }]
            })
            .to_string();
            Ok(request.respond(Response::from_string(body).with_header(json_header()))?)
        }
        ("POST", "/v1/chat/completions") => chat(shared, request, &model_id),
        ("OPTIONS", _) => Ok(request.respond(Response::empty(204))?),
        _ => Ok(request.respond(error_response(404, "нет такого пути"))?),
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn chat(shared: &Arc<Shared>, mut request: Request, model_id: &str) -> anyhow::Result<()> {
    let mut body = String::new();
    request.as_reader().read_to_string(&mut body)?;
    let parsed: ChatRequest = match serde_json::from_str(&body) {
        Ok(p) => p,
        Err(e) => return Ok(request.respond(error_response(400, &format!("разбор запроса: {e}")))?),
    };
    if parsed.messages.is_empty() {
        return Ok(request.respond(error_response(400, "пустой список сообщений"))?);
    }
    let turns: Vec<Turn> = parsed
        .messages
        .into_iter()
        .map(|m| Turn {
            role: m.role,
            content: m.content.into_text(),
        })
        .collect();

    // Диктовка важнее: программа в первую очередь ваша, а не сервер. Если
    // сейчас идёт запись или обработка, внешний клиент получает отказ и
    // повторит, а не задержит вставку текста под курсором на секунды.
    if !matches!(shared.stage(), Stage::Idle) {
        return Ok(request.respond(error_response(
            503,
            "приложение занято диктовкой, повторите запрос",
        ))?);
    }

    let mut llm = shared.llm.lock().unwrap();
    if let Err(e) = llm.ensure(model_id) {
        return Ok(request.respond(error_response(503, &e.to_string()))?);
    }

    if parsed.stream {
        stream_response(&mut llm, request, model_id, &turns, parsed.max_tokens)
    } else {
        let out = llm.chat_raw(model_id, &turns, parsed.max_tokens, |_| {});
        match out {
            Ok(text) => {
                let body = completion_json(model_id, &text);
                Ok(request.respond(Response::from_string(body).with_header(json_header()))?)
            }
            Err(e) => Ok(request.respond(error_response(500, &e.to_string()))?),
        }
    }
}

fn completion_json(model_id: &str, text: &str) -> String {
    serde_json::json!({
        "id": format!("chatcmpl-{}", now()),
        "object": "chat.completion",
        "created": now(),
        "model": model_id,
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": text },
            "finish_reason": "stop",
        }],
    })
    .to_string()
}

/// Потоковая выдача в формате Server-Sent Events, как у OpenAI.
///
/// Пишем прямо в сокет: держать весь ответ в памяти и отдать разом означало бы
/// потерять смысл потока.
fn stream_response(
    llm: &mut crate::local_llm::LocalLlm,
    request: Request,
    model_id: &str,
    turns: &[Turn],
    max_tokens: Option<usize>,
) -> anyhow::Result<()> {
    use std::io::Write;

    let mut writer = request.into_writer();
    let head = "HTTP/1.1 200 OK\r\n\
                Content-Type: text/event-stream; charset=utf-8\r\n\
                Cache-Control: no-cache\r\n\
                Connection: close\r\n\r\n";
    writer.write_all(head.as_bytes())?;
    writer.flush()?;

    let id = format!("chatcmpl-{}", now());
    let created = now();
    let mut failed: Option<std::io::Error> = None;

    // Первый кусок несёт роль — этого ждут клиенты OpenAI.
    let opening = sse_chunk(
        &id,
        created,
        model_id,
        serde_json::json!({"role": "assistant"}),
        None,
    );
    if let Err(e) = writer.write_all(opening.as_bytes()) {
        failed = Some(e);
    }

    let result = llm.chat_raw(model_id, turns, max_tokens, |piece| {
        if failed.is_some() {
            return;
        }
        let chunk = sse_chunk(
            &id,
            created,
            model_id,
            serde_json::json!({ "content": piece }),
            None,
        );
        if let Err(e) = writer
            .write_all(chunk.as_bytes())
            .and_then(|()| writer.flush())
        {
            // Клиент отвалился — дописывать некуда, но генерацию оборвать
            // отсюда нельзя: она докрутит до конца и просто никому не уйдёт.
            failed = Some(e);
        }
    });

    if let Some(e) = failed {
        log::info!("клиент закрыл поток: {e}");
        return Ok(());
    }
    if let Err(e) = result {
        // Ошибку в уже начатом потоке передать нечем, кроме как событием.
        let msg = serde_json::json!({ "error": { "message": e.to_string() } }).to_string();
        let _ = writer.write_all(format!("data: {msg}\n\n").as_bytes());
        let _ = writer.write_all(b"data: [DONE]\n\n");
        let _ = writer.flush();
        return Ok(());
    }

    let closing = sse_chunk(&id, created, model_id, serde_json::json!({}), Some("stop"));
    writer.write_all(closing.as_bytes())?;
    writer.write_all(b"data: [DONE]\n\n")?;
    writer.flush()?;
    Ok(())
}

fn sse_chunk(
    id: &str,
    created: u64,
    model_id: &str,
    delta: serde_json::Value,
    finish: Option<&str>,
) -> String {
    let payload = serde_json::json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model_id,
        "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
    });
    format!("data: {payload}\n\n")
}
