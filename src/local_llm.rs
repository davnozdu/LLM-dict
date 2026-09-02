//! Локальная языковая модель: llama.cpp с Metal, внутри процесса.
//!
//! Отдельного сервера нет, модель живёт в том же потоке, что и распознавание,
//! и подчиняется тем же правилам: грузится при первой надобности, выгружается
//! по простою.
//!
//! Задача у модели узкая — исправить надиктованное, ничего не сочиняя.
//! Маленькие модели про это забывают, поэтому каждый ответ проходит проверки
//! (`sanitize`): не подошёл — вставляется исходный текст. Правило то же, что
//! и для облака: текст важнее обработки.

use anyhow::{anyhow, bail, Result};
use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaChatMessage, LlamaChatTemplate, LlamaModel};
use llama_cpp_2::sampling::LlamaSampler;
use std::num::NonZeroU32;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Instant;

/// Контекст намеренно маленький: диктовка — это десятки слов, а каждый лишний
/// токен стоит памяти под KV-кеш.
const N_CTX: u32 = 4096;

/// Инструкция намеренно запретительная. На стенде мягкая формулировка давала
/// втрое больше испорченных реплик: модели дописывали своё, меняли порядок
/// слов и отвечали на текст вместо правки.
pub const CORRECT_PROMPT: &str = "Ты корректор. Верни тот же самый текст, изменив в нём \
только орфографические ошибки, знаки препинания и заглавные буквы.\n\
ЗАПРЕЩЕНО: заменять слова синонимами, менять порядок слов, переводить, сокращать, \
дополнять, отвечать на содержание, объяснять правки, добавлять заголовки и предисловия.\n\
Если исправлять нечего — верни текст дословно без изменений.\n\
В ответе не должно быть ничего, кроме самого текста.";

/// Бэкенд инициализируется один раз на процесс: повторный вызов llama.cpp не
/// прощает.
fn backend() -> Result<&'static LlamaBackend> {
    static BACKEND: OnceLock<Option<LlamaBackend>> = OnceLock::new();
    BACKEND
        .get_or_init(|| LlamaBackend::init().ok())
        .as_ref()
        .ok_or_else(|| anyhow!("не поднять движок локальной модели"))
}

/// Семейства, которым нужен свой сборщик подсказки.
#[derive(Debug, Clone, Copy)]
enum Family {
    /// У Gemma 4 шаблон чата — Jinja с макросами, и встроенный в llama.cpp
    /// упрощённый движок шаблонов его не разбирает. Формат собираем руками.
    Gemma4,
}

pub struct Corrector {
    model: LlamaModel,
    template: LlamaChatTemplate,
    /// У Gemma 3 в шаблоне нет роли system: инструкция уходит в сообщение
    /// пользователя.
    system_role: bool,
    /// Qwen3 умеет рассуждать вслух, для корректуры это только мешает.
    no_think: bool,
    manual: Option<Family>,
    /// Какая модель загружена — чтобы не перезагружать ту же самую.
    pub model_id: String,
}

impl Corrector {
    pub fn load(model_id: &str, path: &Path) -> Result<Self> {
        let started = Instant::now();
        // Все слои на GPU: память на Apple Silicon общая, держать часть слоёв
        // на процессоре смысла нет.
        let params = LlamaModelParams::default().with_n_gpu_layers(u32::MAX);
        let model = LlamaModel::load_from_file(backend()?, path, &params)
            .map_err(|e| anyhow!("не загрузить модель: {e}"))?;

        let manual = if model_id.contains("gemma-4") {
            Some(Family::Gemma4)
        } else {
            None
        };
        let template = match model.chat_template(None) {
            Ok(t) => t,
            // Для ручного формата шаблон не нужен, но поле должно быть занято.
            Err(e) => match manual {
                Some(_) => LlamaChatTemplate::new("chatml").map_err(|_| anyhow!("{e}"))?,
                None => bail!("в модели нет шаблона чата: {e}"),
            },
        };
        log::info!(
            "локальная модель {model_id} загружена за {} мс",
            started.elapsed().as_millis()
        );
        Ok(Self {
            system_role: !model_id.contains("gemma-3"),
            no_think: model_id.contains("qwen"),
            manual,
            model,
            template,
            model_id: model_id.to_string(),
        })
    }

    fn build_prompt(&self, system: &str, text: &str) -> Result<String> {
        let mut instruction = system.to_string();
        if self.no_think {
            instruction.push_str(" /no_think");
        }
        if let Some(Family::Gemma4) = self.manual {
            return Ok(format!(
                "<bos><|turn>system\n{instruction}<turn|>\n\
                 <|turn>user\n{text}<turn|>\n<|turn>model\n"
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

    /// Прогоняет текст через модель и возвращает уже проверенный результат.
    /// Ошибка означает «вставить исходный текст».
    pub fn correct(&self, system: &str, text: &str) -> Result<String> {
        let g = self.generate(system, text)?;
        sanitize(text, &g).map_err(|r| anyhow!("{}", r.explain()))
    }

    fn generate(&self, system: &str, text: &str) -> Result<Generated> {
        let prompt = self.build_prompt(system, text)?;
        // BOS не добавляем: если модель его требует, он уже есть в шаблоне.
        let tokens = self
            .model
            .str_to_token(&prompt, AddBos::Never)
            .map_err(|e| anyhow!("токенизация не удалась: {e}"))?;
        if tokens.len() + 64 >= N_CTX as usize {
            bail!("текст не влезает в контекст локальной модели");
        }
        // Потолок генерации: исправление не бывает заметно длиннее входа.
        // Без него зациклившаяся модель молотит до конца контекста.
        let budget = (tokens.len() * 2 + 32).min(N_CTX as usize - tokens.len() - 4);

        let ctx_params = LlamaContextParams::default()
            .with_n_ctx(NonZeroU32::new(N_CTX))
            .with_n_batch(N_CTX);
        let mut ctx = self
            .model
            .new_context(backend()?, ctx_params)
            .map_err(|e| anyhow!("не создать контекст: {e}"))?;

        let mut batch = LlamaBatch::new(tokens.len().max(512), 1);
        let last = tokens.len() - 1;
        for (i, t) in tokens.iter().enumerate() {
            batch.add(*t, i as i32, &[0], i == last)?;
        }
        ctx.decode(&mut batch)?;

        // Жадный выбор: корректура должна быть воспроизводимой.
        let mut sampler = LlamaSampler::chain_simple([LlamaSampler::greedy()]);
        // Копим байты, а не строки: один токен может разрезать многобайтовый
        // символ пополам, и посимвольное декодирование ломает кириллицу.
        let mut out: Vec<u8> = Vec::new();
        let mut n_cur = tokens.len() as i32;
        let mut produced = 0usize;

        while produced < budget {
            let token = sampler.sample(&ctx, -1);
            sampler.accept(token);
            if self.model.is_eog_token(token) {
                break;
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
            hit_budget: produced >= budget,
        })
    }
}

pub struct Generated {
    pub raw: String,
    /// Модель упёрлась в потолок, то есть не остановилась сама.
    pub hit_budget: bool,
}

/// Почему ответ модели забракован.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Reject {
    Empty,
    Runaway,
    Length,
    Script,
}

impl Reject {
    pub fn explain(self) -> &'static str {
        match self {
            Reject::Empty => "локальная модель вернула пустой ответ",
            Reject::Runaway => "локальная модель не остановилась",
            Reject::Length => "локальная модель переписала текст, а не исправила",
            Reject::Script => "локальная модель сменила язык текста",
        }
    }
}

/// Чистка ответа и проверки, после которых текст можно вставлять.
///
/// Проверки дешёвые, но снимают основную опасность: на стенде модель, которая
/// вместо правки отвечала на текст, была забракована в 96% случаев — то есть
/// пользователь получил бы свой текст нетронутым вместо чужого сочинения.
pub fn sanitize(input: &str, g: &Generated) -> Result<String, Reject> {
    let mut s = g.raw.as_str();
    // Рассуждения вслух: Qwen3 их выдаёт даже с /no_think, если сочтёт задачу
    // сложной.
    if let Some(end) = s.find("</think>") {
        s = &s[end + "</think>".len()..];
    }
    let mut s = s.trim().to_string();

    // Ответ, завёрнутый в блок кода или кавычки.
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
    // Преамбула вида «Исправленный текст:» — её выдавала Gemma 3 на стенде.
    s = strip_preamble(&s);

    if s.is_empty() {
        return Err(Reject::Empty);
    }
    if g.hit_budget {
        return Err(Reject::Runaway);
    }
    // Длина: правка пунктуации не меняет объём текста на треть.
    let (a, b) = (input.chars().count() as f32, s.chars().count() as f32);
    if b < a * 0.7 || b > a * 1.35 {
        return Err(Reject::Length);
    }
    // Письменность: была кириллица — должна остаться.
    let cyr = |t: &str| t.chars().filter(|c| is_cyrillic(*c)).count();
    if cyr(input) > 5 && cyr(&s) * 2 < cyr(input) {
        return Err(Reject::Script);
    }
    Ok(s)
}

fn strip_preamble(s: &str) -> String {
    const HEADS: [&str; 6] = [
        "исправленный текст:",
        "исправленный вариант:",
        "вот исправленный текст:",
        "результат:",
        "ответ:",
        "corrected text:",
    ];
    let lower = s.to_lowercase();
    for h in HEADS {
        if lower.starts_with(h) {
            return s[h.len()..].trim_start().to_string();
        }
    }
    s.to_string()
}

fn is_cyrillic(c: char) -> bool {
    ('\u{0400}'..='\u{04FF}').contains(&c)
}

/// Загруженная языковая модель и отсчёт её простоя.
///
/// Живёт в том же потоке, что и `LocalEngines`, и подчиняется общему правилу
/// выгрузки: две модели по несколько гигабайт в фоновой программе — это много.
#[derive(Default)]
pub struct LocalLlm {
    loaded: Option<Corrector>,
    last_used: Option<Instant>,
}

impl LocalLlm {
    /// Загружает нужную модель, если загружена не та или ничего не загружено.
    pub fn ensure(&mut self, model_id: &str) -> Result<()> {
        if self.loaded.as_ref().is_some_and(|c| c.model_id == model_id) {
            return Ok(());
        }
        let spec = crate::models::find(model_id)
            .ok_or_else(|| anyhow!("неизвестная модель: {model_id}"))?;
        if !spec.is_installed() {
            bail!(
                "модель «{}» не скачана — откройте настройки и нажмите «Скачать»",
                spec.title
            );
        }
        // Старую выгружаем до загрузки новой, иначе на пике в памяти окажутся обе.
        self.loaded = None;
        let path = spec.dir().join(spec.files[0].name);
        self.loaded = Some(Corrector::load(model_id, &path)?);
        self.last_used = Some(Instant::now());
        Ok(())
    }

    /// Обрабатывает текст. Ошибка означает «оставить текст как есть».
    pub fn run(&mut self, model_id: &str, system: &str, text: &str) -> Result<String> {
        self.ensure(model_id)?;
        let out = self
            .loaded
            .as_ref()
            .ok_or_else(|| anyhow!("локальная модель не загружена"))?
            .correct(system, text);
        // Отметка ставится и при отказе: модель всё равно в памяти.
        self.last_used = Some(Instant::now());
        out
    }

    pub fn unload(&mut self) {
        if self.loaded.take().is_some() {
            log::info!("локальная языковая модель выгружена из памяти");
        }
        self.last_used = None;
    }

    pub fn is_loaded(&self) -> bool {
        self.loaded.is_some()
    }

    pub fn idle_secs(&self) -> Option<u64> {
        self.last_used.map(|t| t.elapsed().as_secs())
    }

    /// Прогревает модель заранее, чтобы первое обращение не ждало загрузку.
    pub fn preload(&mut self, model_id: &str) {
        if let Err(e) = self.ensure(model_id) {
            log::warn!("предзагрузка языковой модели {model_id}: {e}");
        }
    }
}
