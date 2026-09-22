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

    // Ограниченный пул: запросы не создают неограниченное число потоков.
    let (tx, rx) = std::sync::mpsc::sync_channel::<Request>(4);
    let rx = Arc::new(std::sync::Mutex::new(rx));
    for _ in 0..4 {
        let shared = shared.clone();
        let rx = rx.clone();
        std::thread::spawn(move || loop {
            let request = { rx.lock().unwrap().recv() };
            let Ok(request) = request else {
                break;
            };
            if let Err(e) = handle(&shared, request) {
                log::warn!("локальный эндпоинт: {e}");
            }
        });
    }
    std::thread::spawn(move || {
        for request in server.incoming_requests() {
            if let Err(e) = tx.try_send(request) {
                let request = match e {
                    std::sync::mpsc::TrySendError::Full(r)
                    | std::sync::mpsc::TrySendError::Disconnected(r) => r,
                };
                let _ = request.respond(error_response(503, "очередь сервера заполнена"));
            }
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
    use std::io::Read;
    const MAX_BODY: usize = 1024 * 1024;
    if request.body_length().is_some_and(|n| n > MAX_BODY) {
        return Ok(request.respond(error_response(413, "слишком большой запрос"))?);
    }
    let mut body = String::new();
    request
        .as_reader()
        .take((MAX_BODY + 1) as u64)
        .read_to_string(&mut body)?;
    if body.len() > MAX_BODY {
        return Ok(request.respond(error_response(413, "слишком большой запрос"))?);
    }
    let parsed: ChatRequest = match serde_json::from_str(&body) {
        Ok(p) => p,
        Err(e) => return Ok(request.respond(error_response(400, &format!("разбор запроса: {e}")))?),
    };
    if parsed.max_tokens.is_some_and(|n| n == 0 || n > 32768) {
        return Ok(request.respond(error_response(400, "max_tokens должен быть от 1 до 32768"))?);
    }
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

    if parsed.stream {
        stream_response(shared, request, model_id, &turns, parsed.max_tokens)
    } else {
        let Ok(mut llm) = shared.llm.try_lock() else {
            return Ok(request.respond(error_response(503, "модель занята, повторите запрос"))?);
        };
        if shared.stage() != Stage::Idle {
            drop(llm);
            return Ok(request.respond(error_response(503, "приложение занято"))?);
        }
        let out = llm.chat_raw(model_id, &turns, parsed.max_tokens, |_| {
            shared.stage() == Stage::Idle
        });
        drop(llm); // Сеть никогда не удерживает модель.
        match out {
            Ok(out) => {
                let body = completion_json(model_id, &out);
                Ok(request.respond(Response::from_string(body).with_header(json_header()))?)
            }
            Err(e) => Ok(request.respond(error_response(503, &e.to_string()))?),
        }
    }
}

fn completion_json(model_id: &str, out: &crate::local_llm::Generated) -> String {
    serde_json::json!({
        "id": format!("chatcmpl-{}", now()),
        "object": "chat.completion",
        "created": now(),
        "model": model_id,
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": out.raw },
            "finish_reason": out.finish_reason(),
        }],
    })
    .to_string()
}

/// Потоковая выдача в формате Server-Sent Events, как у OpenAI.
///
/// Пишем прямо в сокет: держать весь ответ в памяти и отдать разом означало бы
/// потерять смысл потока.
fn stream_response(
    shared: &Arc<Shared>,
    request: Request,
    model_id: &str,
    turns: &[Turn],
    max_tokens: Option<usize>,
) -> anyhow::Result<()> {
    use std::io::Write;
    let Ok(mut llm) = shared.llm.try_lock() else {
        return Ok(request.respond(error_response(503, "модель занята, повторите запрос"))?);
    };
    if shared.stage() != Stage::Idle {
        drop(llm);
        return Ok(request.respond(error_response(503, "приложение занято"))?);
    }
    let (tx, rx) = std::sync::mpsc::sync_channel::<String>(64);
    let mut writer = request.into_writer();
    let sender = std::thread::spawn(move || -> std::io::Result<()> {
        writer.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n")?;
        writer.flush()?;
        for chunk in rx {
            writer.write_all(chunk.as_bytes())?;
            writer.flush()?;
        }
        Ok(())
    });
    let id = format!("chatcmpl-{}", now());
    let created = now();
    let opening = sse_chunk(
        &id,
        created,
        model_id,
        serde_json::json!({"role": "assistant"}),
        None,
    );
    let _ = tx.try_send(opening);
    let result = llm.chat_raw(model_id, turns, max_tokens, |piece| {
        if shared.stage() != Stage::Idle || sender.is_finished() {
            return false;
        }
        piece.is_empty()
            || tx
                .try_send(sse_chunk(
                    &id,
                    created,
                    model_id,
                    serde_json::json!({"content": piece}),
                    None,
                ))
                .is_ok()
    });
    drop(llm);
    let closing = match result {
        Ok(out) => sse_chunk(
            &id,
            created,
            model_id,
            serde_json::json!({}),
            Some(out.finish_reason()),
        ),
        Err(e) => format!(
            "data: {}\n\n",
            serde_json::json!({"error": {"message": e.to_string()}})
        ),
    };
    // Очередь уже не держит модель. Закрытый/медленный клиент не мешает диктовке.
    let _ = tx.try_send(format!("{closing}data: [DONE]\n\n"));
    drop(tx);
    match sender.join() {
        Ok(result) => Ok(result?),
        Err(_) => anyhow::bail!("поток отправки ответа завершился аварийно"),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_truncation_in_json_and_stream() {
        for (hit_budget, reason) in [(false, "stop"), (true, "length")] {
            let out = crate::local_llm::Generated {
                raw: "текст".into(),
                hit_budget,
            };
            let json: serde_json::Value =
                serde_json::from_str(&completion_json("test", &out)).unwrap();
            assert_eq!(json["choices"][0]["finish_reason"], reason);
            let chunk = sse_chunk(
                "test",
                1,
                "test",
                serde_json::json!({}),
                Some(out.finish_reason()),
            );
            let json: serde_json::Value =
                serde_json::from_str(chunk.trim().strip_prefix("data: ").unwrap()).unwrap();
            assert_eq!(json["choices"][0]["finish_reason"], reason);
        }
    }
}
