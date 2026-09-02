//! Стенд для сравнения локальных моделей на реальных диктовках.
//!
//! Движок тот же, что пойдёт в релиз: llama.cpp с Metal, статически внутри
//! процесса. Блок `Corrector` ниже — это и есть будущий локальный поставщик,
//! он переносится в приложение почти как есть.
//!
//! Запуск: llmtest <файл.gguf> <testset.json> <результат.json>

use anyhow::{anyhow, bail, Result};
use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaChatMessage, LlamaChatTemplate, LlamaModel};
use llama_cpp_2::sampling::LlamaSampler;
use std::num::NonZeroU32;
use std::path::Path;
use std::time::Instant;

// ---------------------------------------------------------------------------
// Часть, которая пойдёт в релиз
// ---------------------------------------------------------------------------

/// Контекст намеренно маленький: диктовка — это десятки слов, а каждый лишний
/// токен стоит памяти под KV-кеш.
const N_CTX: u32 = 4096;

/// Инструкция та же, что у облачного действия «Корректура», иначе локальную
/// модель не с чем сравнивать.
const PROMPT_BASE: &str = "Исправь орфографию, пунктуацию и согласование в тексте \
пользователя. Не меняй смысл, стиль и язык, ничего не добавляй и не убирай. \
Выведи только исправленный текст.";

/// Ужесточённый вариант: задача у модели узкая, и мелкие модели про это
/// забывают — начинают отвечать на текст или пересказывать его.
const PROMPT_STRICT: &str = "Ты корректор. Верни тот же самый текст, изменив в нём \
только орфографические ошибки, знаки препинания и заглавные буквы.\n\
ЗАПРЕЩЕНО: заменять слова синонимами, менять порядок слов, переводить, сокращать, \
дополнять, отвечать на содержание, объяснять правки, добавлять заголовки и \
предисловия.\n\
Если исправлять нечего — верни текст дословно без изменений.\n\
В ответе не должно быть ничего, кроме самого текста.";

pub struct Corrector {
    model: LlamaModel,
    /// Шаблон берётся из самого файла модели: подставить чужой — верный
    /// способ получить мусор на выходе.
    template: LlamaChatTemplate,
    /// У Gemma в шаблоне нет роли system, инструкция уходит в сообщение
    /// пользователя.
    system_role: bool,
    /// Qwen3 умеет рассуждать вслух; для корректуры это только мешает.
    no_think: bool,
    /// Какую инструкцию давать модели.
    strict: bool,
    /// У Gemma 4 шаблон чата — Jinja с макросами, встроенный в llama.cpp
    /// упрощённый движок его не разбирает. Для таких моделей формат
    /// собирается вручную.
    manual: Option<Family>,
    pub load_ms: u128,
}

impl Corrector {
    pub fn load(backend: &LlamaBackend, path: &Path, strict: bool) -> Result<Self> {
        let started = Instant::now();
        // Все слои на GPU: память на Apple Silicon общая, держать часть слоёв
        // на CPU смысла нет.
        let params = LlamaModelParams::default().with_n_gpu_layers(u32::MAX);
        let model = LlamaModel::load_from_file(backend, path, &params)
            .map_err(|e| anyhow!("не загрузить модель: {e}"))?;
        let name_probe = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let template = match model.chat_template(None) {
            Ok(t) => t,
            Err(e) if name_probe.contains("gemma4") => {
                // Собираем подсказку сами, шаблон не понадобится.
                LlamaChatTemplate::new("chatml").map_err(|_| anyhow!("{e}"))?
            }
            Err(e) => return Err(anyhow!("в модели нет шаблона чата: {e}")),
        };
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        Ok(Self {
            system_role: !name.contains("gemma"),
            no_think: name.contains("qwen"),
            strict,
            manual: if name.contains("gemma4") {
                Some(Family::Gemma4)
            } else {
                None
            },
            model,
            template,
            load_ms: started.elapsed().as_millis(),
        })
    }

    fn build_prompt(&self, text: &str) -> Result<String> {
        let mut instruction = if self.strict {
            PROMPT_STRICT
        } else {
            PROMPT_BASE
        }
        .to_string();
        if self.no_think {
            instruction.push_str(" /no_think");
        }
        if let Some(Family::Gemma4) = self.manual {
            // <bos><|turn>system\n…<turn|>\n<|turn>user\n…<turn|>\n<|turn>model\n
            return Ok(format!(
                "<bos><|turn>system\n{instruction}<turn|>\n<|turn>user\n{text}<turn|>\n<|turn>model\n"
            ));
        }
        let chat = if self.system_role {
            vec![
                LlamaChatMessage::new("system".into(), instruction)?,
                LlamaChatMessage::new("user".into(), text.to_string())?,
            ]
        } else {
            vec![LlamaChatMessage::new(
                "user".into(),
                format!("{instruction}\n\n{text}"),
            )?]
        };
        self.model
            .apply_chat_template(&self.template, &chat, true)
            .map_err(|e| anyhow!("шаблон чата не применился: {e}"))
    }

    /// Один прогон. Возвращает сырой ответ модели, число выданных токенов и
    /// время до первого токена в миллисекундах.
    pub fn run(&self, backend: &LlamaBackend, text: &str) -> Result<Generated> {
        let prompt = self.build_prompt(text)?;
        // BOS не добавляем: если модель его требует, он уже есть в шаблоне.
        let tokens = self
            .model
            .str_to_token(&prompt, AddBos::Never)
            .map_err(|e| anyhow!("токенизация не удалась: {e}"))?;

        // Потолок генерации: исправление не может быть заметно длиннее входа.
        // Без него зациклившаяся модель молотит до конца контекста.
        let budget = (tokens.len() * 2 + 32).min(N_CTX as usize - tokens.len() - 4);
        if tokens.len() + 64 >= N_CTX as usize {
            bail!("текст не влезает в контекст: {} токенов", tokens.len());
        }

        let ctx_params = LlamaContextParams::default()
            .with_n_ctx(NonZeroU32::new(N_CTX))
            .with_n_batch(N_CTX);
        let mut ctx = self
            .model
            .new_context(backend, ctx_params)
            .map_err(|e| anyhow!("не создать контекст: {e}"))?;

        let mut batch = LlamaBatch::new(tokens.len().max(512), 1);
        let last = tokens.len() - 1;
        for (i, t) in tokens.iter().enumerate() {
            batch.add(*t, i as i32, &[0], i == last)?;
        }
        let started = Instant::now();
        ctx.decode(&mut batch)?;

        // Жадный выбор: корректура должна быть воспроизводимой, разброс здесь
        // не нужен.
        let mut sampler = LlamaSampler::chain_simple([LlamaSampler::greedy()]);
        // Копим байты, а не строки: один токен может разрезать многобайтовый
        // символ пополам, и посимвольное декодирование ломает кириллицу.
        let mut out: Vec<u8> = Vec::new();
        let mut n_cur = tokens.len() as i32;
        let mut produced = 0usize;
        let mut ttft = 0u128;

        while produced < budget {
            let token = sampler.sample(&ctx, -1);
            sampler.accept(token);
            if self.model.is_eog_token(token) {
                break;
            }
            if produced == 0 {
                ttft = started.elapsed().as_millis();
            }
            out.extend_from_slice(&self.model.token_to_piece_bytes(token, 64, false, None)?);
            produced += 1;

            batch.clear();
            batch.add(token, n_cur, &[0], true)?;
            n_cur += 1;
            ctx.decode(&mut batch)?;
        }
        Ok(Generated {
            raw: String::from_utf8_lossy(&out).into_owned(),
            tokens_in: tokens.len(),
            tokens_out: produced,
            ttft_ms: ttft,
            hit_budget: produced >= budget,
        })
    }
}

/// Семейства, которым нужен свой сборщик подсказки.
#[derive(Debug, Clone, Copy)]
pub enum Family {
    Gemma4,
}

pub struct Generated {
    pub raw: String,
    pub tokens_in: usize,
    pub tokens_out: usize,
    pub ttft_ms: u128,
    pub hit_budget: bool,
}

/// Почему результат модели забракован.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Reject {
    Empty,
    /// Ответ заметно длиннее или короче входа — модель не исправляла, а
    /// сочиняла или проглотила часть текста.
    Length,
    /// Сменилась письменность: кириллица на входе, латиница на выходе —
    /// модель перевела.
    Script,
    /// Модель упёрлась в потолок генерации, то есть не остановилась сама.
    Runaway,
}

/// Приведение ответа в порядок и проверки, после которых текст можно
/// вставлять пользователю. Ошибка означает «вставить исходный текст».
pub fn sanitize(input: &str, g: &Generated) -> Result<String, Reject> {
    let mut s = g.raw.as_str();

    // Рассуждения вслух: Qwen3 их выдаёт даже с /no_think, если решит, что
    // задача сложная.
    if let Some(end) = s.find("</think>") {
        s = &s[end + "</think>".len()..];
    }
    let mut s = s.trim().to_string();

    // Модель любит завернуть ответ в блок кода или кавычки.
    for fence in ["```text", "```markdown", "```"] {
        if s.starts_with(fence) {
            s = s[fence.len()..].trim_start().to_string();
            if let Some(p) = s.rfind("```") {
                s.truncate(p);
            }
            s = s.trim().to_string();
        }
    }
    if s.len() > 2 && s.starts_with('"') && s.ends_with('"') {
        s = s[1..s.len() - 1].to_string();
    }

    if s.is_empty() {
        return Err(Reject::Empty);
    }
    if g.hit_budget {
        return Err(Reject::Runaway);
    }

    // Длина: исправление пунктуации не меняет объём текста на треть.
    let (a, b) = (input.chars().count() as f32, s.chars().count() as f32);
    if b < a * 0.7 || b > a * 1.35 {
        return Err(Reject::Length);
    }

    // Письменность: если во входе была кириллица, она должна остаться.
    let in_cyr = input.chars().filter(|c| is_cyrillic(*c)).count();
    let out_cyr = s.chars().filter(|c| is_cyrillic(*c)).count();
    if in_cyr > 5 && out_cyr * 2 < in_cyr {
        return Err(Reject::Script);
    }
    Ok(s)
}

fn is_cyrillic(c: char) -> bool {
    ('\u{0400}'..='\u{04FF}').contains(&c)
}

// ---------------------------------------------------------------------------
// Драйвер стенда
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct Case {
    raw: String,
    #[serde(rename = "ref")]
    reference: String,
    changed: bool,
}

#[derive(serde::Serialize)]
struct Out {
    raw: String,
    reference: String,
    got: String,
    accepted: bool,
    reject: Option<String>,
    changed_ref: bool,
    tokens_in: usize,
    tokens_out: usize,
    ttft_ms: u128,
    total_ms: u128,
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        bail!("использование: llmtest <модель.gguf> <testset.json> <результат.json>");
    }
    let model_path = Path::new(&args[1]);
    let cases: Vec<Case> = serde_json::from_reader(std::fs::File::open(&args[2])?)?;

    let backend = LlamaBackend::init().map_err(|e| anyhow!("llama backend: {e}"))?;
    let strict = args.iter().any(|a| a == "--strict");
    let corrector = Corrector::load(&backend, model_path, strict)?;
    eprintln!(
        "инструкция: {}",
        if strict {
            "жёсткая"
        } else {
            "как в облаке"
        }
    );
    eprintln!(
        "модель загружена за {} мс: {}",
        corrector.load_ms,
        model_path.file_name().unwrap_or_default().to_string_lossy()
    );

    let mut outs = Vec::new();
    for (i, c) in cases.iter().enumerate() {
        let t0 = Instant::now();
        let res = corrector.run(&backend, &c.raw);
        let total = t0.elapsed().as_millis();
        let (got, accepted, reject, ti, to, ttft) = match res {
            Ok(g) => match sanitize(&c.raw, &g) {
                Ok(text) => (text, true, None, g.tokens_in, g.tokens_out, g.ttft_ms),
                Err(r) => (
                    g.raw.clone(),
                    false,
                    Some(format!("{r:?}")),
                    g.tokens_in,
                    g.tokens_out,
                    g.ttft_ms,
                ),
            },
            Err(e) => (String::new(), false, Some(format!("Error: {e}")), 0, 0, 0),
        };
        eprint!("\r  {}/{}", i + 1, cases.len());
        outs.push(Out {
            raw: c.raw.clone(),
            reference: c.reference.clone(),
            got,
            accepted,
            reject,
            changed_ref: c.changed,
            tokens_in: ti,
            tokens_out: to,
            ttft_ms: ttft,
            total_ms: total,
        });
    }
    eprintln!();
    serde_json::to_writer_pretty(std::fs::File::create(&args[3])?, &outs)?;
    println!("load_ms={}", corrector.load_ms);
    Ok(())
}
