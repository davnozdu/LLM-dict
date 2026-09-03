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

/// Контекст подбирается под запрос: каждый лишний токен стоит памяти под
/// KV-кеш, а диктовка — это десятки слов. Но действие может нести файл
/// сведений на тысячи токенов, и тогда маленького контекста не хватит.
///
/// Нижняя граница — обычная диктовка, верхняя выбрана по памяти: сами модели
/// держат куда больше (у Gemma 4 E2B это 131072), но такой KV-кеш съел бы
/// больше самой модели.
const N_CTX_MIN: u32 = 4096;
const N_CTX_MAX: u32 = 32768;

/// Реплика беседы в том же виде, в каком её присылает клиент OpenAI.
#[derive(Debug, Clone)]
pub struct Turn {
    pub role: String,
    pub content: String,
}

/// Насколько строго проверять ответ модели.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Guard {
    /// Правка надиктованного: на выходе ожидается тот же текст с точностью
    /// до знаков препинания, поэтому проверяются длина и письменность.
    Correction,
    /// Перевод, ответ по данным, пересказ — выход по замыслу не похож на
    /// вход, и мерить его длиной входа бессмысленно.
    FreeForm,
}

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

    /// Одна реплика беседы. Роли те же, что у OpenAI: system, user, assistant.
    fn render_chat(&self, turns: &[Turn]) -> Result<String> {
        if let Some(Family::Gemma4) = self.manual {
            // Формат Gemma 4: <bos><|turn>роль\n…<turn|>\n, в конце пустая
            // реплика модели как приглашение к ответу. Встроенный в llama.cpp
            // движок шаблонов её Jinja с макросами не разбирает.
            let mut out = String::from("<bos>");
            for t in turns {
                let role = if t.role == "assistant" {
                    "model"
                } else {
                    &t.role
                };
                out.push_str(&format!("<|turn>{role}\n{}<turn|>\n", t.content));
            }
            out.push_str("<|turn>model\n");
            return Ok(out);
        }

        // У части моделей (Gemma 3) в шаблоне нет роли system: её содержимое
        // приклеивается к первой реплике пользователя.
        let mut chat: Vec<LlamaChatMessage> = Vec::with_capacity(turns.len());
        let mut pending_system = String::new();
        for t in turns {
            if t.role == "system" && !self.system_role {
                if !pending_system.is_empty() {
                    pending_system.push_str("\n\n");
                }
                pending_system.push_str(&t.content);
                continue;
            }
            let content = if t.role == "user" && !pending_system.is_empty() {
                let merged = format!("{pending_system}\n\n{}", t.content);
                pending_system.clear();
                merged
            } else {
                t.content.clone()
            };
            chat.push(LlamaChatMessage::new(t.role.clone(), content)?);
        }
        // Системная часть без единой реплики пользователя — отправляем как
        // реплику пользователя, иначе она потерялась бы.
        if !pending_system.is_empty() {
            chat.push(LlamaChatMessage::new("user".into(), pending_system)?);
        }
        self.model
            .apply_chat_template(&self.template, &chat, true)
            .map_err(|e| anyhow!("шаблон чата не применился: {e}"))
    }

    fn build_prompt(&self, system: &str, context: Option<&str>, text: &str) -> Result<String> {
        let mut instruction = system.to_string();
        // Сведения идут в ту же системную часть, что и инструкция. Облако
        // кладёт их отдельным сообщением, но здесь это ничего не меняет:
        // шаблоны части моделей второе системное сообщение не принимают.
        if let Some(c) = context.filter(|c| !c.trim().is_empty()) {
            instruction.push_str("\n\nСведения, на которые нужно опираться:\n\n");
            instruction.push_str(c.trim());
        }
        if self.no_think {
            instruction.push_str(" /no_think");
        }
        self.render_chat(&[
            Turn {
                role: "system".into(),
                content: instruction,
            },
            Turn {
                role: "user".into(),
                content: text.to_string(),
            },
        ])
    }

    /// Сколько токенов занимает текст у этой модели.
    pub fn count_tokens(&self, text: &str) -> Result<usize> {
        self.model
            .str_to_token(text, AddBos::Never)
            .map(|t| t.len())
            .map_err(|e| anyhow!("токенизация не удалась: {e}"))
    }

    /// Прогоняет текст через модель и возвращает уже проверенный результат.
    /// Ошибка означает «оставить исходный текст».
    pub fn correct(
        &self,
        system: &str,
        context: Option<&str>,
        text: &str,
        guard: Guard,
    ) -> Result<String> {
        let g = self.generate(system, context, text)?;
        sanitize(text, &g, guard).map_err(|r| anyhow!("{}", r.explain()))
    }

    fn generate(&self, system: &str, context: Option<&str>, text: &str) -> Result<Generated> {
        let prompt = self.build_prompt(system, context, text)?;
        // Потолок генерации: правка не бывает многократно длиннее входа.
        let budget = |n: usize| n * 2 + 256;
        self.run_prompt(prompt, budget, |_| {})
    }

    /// Беседа произвольной формы: то, что приходит снаружи через
    /// OpenAI-совместимый эндпоинт.
    ///
    /// `on_token` вызывается на каждый готовый кусок текста — им и кормится
    /// потоковая выдача. Проверки `sanitize` здесь не применяются: они
    /// написаны для правки надиктованного, а внешний клиент ждёт обычный
    /// ответ модели.
    pub fn chat(
        &self,
        turns: &[Turn],
        max_tokens: Option<usize>,
        on_token: impl FnMut(&str),
    ) -> Result<Generated> {
        let prompt = self.render_chat(turns)?;
        let budget = move |n: usize| max_tokens.unwrap_or(n * 2 + 512);
        self.run_prompt(prompt, budget, on_token)
    }

    /// Общий цикл генерации.
    fn run_prompt(
        &self,
        prompt: String,
        budget_for: impl Fn(usize) -> usize,
        mut on_token: impl FnMut(&str),
    ) -> Result<Generated> {
        // BOS не добавляем: если модель его требует, он уже есть в шаблоне.
        let tokens = self
            .model
            .str_to_token(&prompt, AddBos::Never)
            .map_err(|e| anyhow!("токенизация не удалась: {e}"))?;
        if tokens.is_empty() {
            bail!("пустой запрос");
        }
        // Без потолка зациклившаяся модель молотит до конца контекста.
        let budget = budget_for(tokens.len()).max(16);

        // Контекст под конкретный запрос, а не всегда самый большой: KV-кеш
        // занимает память пропорционально размеру, и держать 32k ради фразы
        // в двадцать слов незачем.
        let needed = (tokens.len() + budget + 64) as u32;
        if needed > N_CTX_MAX {
            bail!(
                "запрос не влезает в контекст локальной модели: {} токенов при пределе {}",
                tokens.len(),
                N_CTX_MAX
            );
        }
        let n_ctx = needed.max(N_CTX_MIN).next_power_of_two().min(N_CTX_MAX);
        let budget = budget.min((n_ctx as usize).saturating_sub(tokens.len() + 4));

        let ctx_params = LlamaContextParams::default()
            .with_n_ctx(NonZeroU32::new(n_ctx))
            .with_n_batch(n_ctx);
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

        // Жадный выбор: правка должна быть воспроизводимой.
        let mut sampler = LlamaSampler::chain_simple([LlamaSampler::greedy()]);
        // Копим байты, а не строки: один токен может разрезать многобайтовый
        // символ пополам, и посимвольное декодирование ломает кириллицу.
        let mut out: Vec<u8> = Vec::new();
        // Сколько байтов уже отдано наружу: остаток — незаконченный символ,
        // который ждёт следующего токена.
        let mut sent = 0usize;
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

            // Отдаём только то, что уже складывается в целые символы.
            if let Some(chunk) = complete_utf8(&out[sent..]) {
                if !chunk.is_empty() {
                    on_token(chunk);
                    sent += chunk.len();
                }
            }

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
pub fn sanitize(input: &str, g: &Generated, guard: Guard) -> Result<String, Reject> {
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
    // Дальше — проверки, осмысленные только для правки: там выход обязан
    // походить на вход. Для перевода или ответа по данным он по замыслу
    // другой, и мерить его длиной входа значило бы браковать исправное.
    if guard == Guard::FreeForm {
        return Ok(s);
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

/// Самый длинный кусок байтов, который целиком складывается в UTF-8.
///
/// Токен может кончиться на середине многобайтового символа: отдать такой
/// хвост наружу — получить «ромбик с вопросом» в чужом клиенте.
fn complete_utf8(bytes: &[u8]) -> Option<&str> {
    match std::str::from_utf8(bytes) {
        Ok(s) => Some(s),
        Err(e) => std::str::from_utf8(&bytes[..e.valid_up_to()]).ok(),
    }
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
    pub fn run(
        &mut self,
        model_id: &str,
        system: &str,
        context: Option<&str>,
        text: &str,
        guard: Guard,
    ) -> Result<String> {
        self.ensure(model_id)?;
        let out = self
            .loaded
            .as_ref()
            .ok_or_else(|| anyhow!("локальная модель не загружена"))?
            .correct(system, context, text, guard);
        // Отметка ставится и при отказе: модель всё равно в памяти.
        self.last_used = Some(Instant::now());
        out
    }

    /// Беседа произвольной формы для внешнего эндпоинта. Проверки корректуры
    /// не применяются: клиент ждёт обычный ответ модели, а не правку текста.
    pub fn chat_raw(
        &mut self,
        model_id: &str,
        turns: &[Turn],
        max_tokens: Option<usize>,
        on_token: impl FnMut(&str),
    ) -> Result<String> {
        self.ensure(model_id)?;
        let out = self
            .loaded
            .as_ref()
            .ok_or_else(|| anyhow!("локальная модель не загружена"))?
            .chat(turns, max_tokens, on_token)
            .map(|g| g.raw);
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
