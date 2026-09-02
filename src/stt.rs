//! Распознавание речи: облако и локальный Parakeet за общим интерфейсом.
//!
//! Локальная модель весит больше полугигабайта и держится в памяти между
//! диктовками: загрузка занимает секунды, и делать её на каждую фразу
//! бессмысленно. По простою она выгружается — см. `unload`.

use crate::config::{Config, SttConfig};
use crate::models::{self, Engine};
use anyhow::{bail, Context, Result};

/// Загруженная локальная модель. Хранится между вызовами, поэтому помнит,
/// что именно загружено — при смене модели в настройках надо перезагрузиться.
enum Loaded {
    Parakeet {
        id: String,
        model: Box<parakeet_rs::ParakeetTDT>,
    },
}

impl Loaded {
    fn matches(&self, engine: Engine, model_id: &str) -> bool {
        match self {
            Loaded::Parakeet { id, .. } => engine == Engine::Parakeet && id == model_id,
        }
    }
}

#[derive(Default)]
pub struct LocalEngines {
    loaded: Option<Loaded>,
    /// Когда моделью пользовались в последний раз — по этому отсчитывается
    /// простой перед выгрузкой.
    last_used: Option<std::time::Instant>,
}

impl LocalEngines {
    fn ensure(&mut self, engine: Engine, model_id: &str) -> Result<()> {
        if self
            .loaded
            .as_ref()
            .is_some_and(|l| l.matches(engine, model_id))
        {
            return Ok(());
        }
        let spec =
            models::find(model_id).with_context(|| format!("неизвестная модель: {model_id}"))?;
        if !spec.is_installed() {
            bail!(
                "модель «{}» не скачана — откройте настройки и нажмите «Скачать»",
                spec.title
            );
        }

        // Старую выгружаем до загрузки новой, иначе на пике в памяти окажутся обе.
        self.loaded = None;
        let started = std::time::Instant::now();

        self.loaded = Some(match engine {
            Engine::Parakeet => {
                let model = parakeet_rs::ParakeetTDT::from_pretrained(spec.dir(), None)
                    .map_err(|e| anyhow::anyhow!("не загрузить модель Parakeet: {e}"))?;
                Loaded::Parakeet {
                    id: model_id.to_string(),
                    model: Box::new(model),
                }
            }
            Engine::Cloud => bail!("облако не требует загрузки модели"),
            Engine::Llm => bail!("языковая модель загружается не здесь"),
        });

        log::info!(
            "модель {model_id} загружена за {:.1} с",
            started.elapsed().as_secs_f32()
        );
        Ok(())
    }

    /// Язык не принимается: Parakeet определяет его сам, а облако получает
    /// настройку отдельно, в `providers::transcribe`.
    fn run(&mut self, samples: &[f32]) -> Result<String> {
        match self.loaded.as_mut() {
            Some(Loaded::Parakeet { model, .. }) => {
                use parakeet_rs::Transcriber;
                let result = model
                    .transcribe_samples(samples.to_vec(), crate::audio::TARGET_RATE, 1, None)
                    .map_err(|e| anyhow::anyhow!("Parakeet: распознавание не удалось: {e}"))?;
                Ok(result.text.trim().to_string())
            }
            None => bail!("локальная модель не загружена"),
        }
    }

    /// Выгружает модель из памяти. Файл на диске остаётся: «установлена» и
    /// «загружена» — разные вещи.
    pub fn unload(&mut self) {
        if self.loaded.take().is_some() {
            log::info!("модель распознавания выгружена из памяти");
        }
        self.last_used = None;
    }

    /// Сколько секунд моделью не пользовались.
    pub fn idle_secs(&self) -> Option<u64> {
        self.last_used.map(|t| t.elapsed().as_secs())
    }

    /// Прогревает модель заранее, чтобы первая диктовка не ждала загрузку.
    pub fn preload(&mut self, engine: Engine, model_id: &str) {
        if engine.is_local() {
            match self.ensure(engine, model_id) {
                // Иначе прогретая при запуске модель попала бы под выгрузку
                // сразу же: обращений к ней ещё не было.
                Ok(()) => self.last_used = Some(std::time::Instant::now()),
                Err(e) => log::warn!("предзагрузка {model_id}: {e}"),
            }
        }
    }
}

/// Какая модель выбрана для движка в настройках.
fn model_id_for(cfg: &SttConfig, engine: Engine) -> &str {
    match engine {
        Engine::Parakeet => &cfg.parakeet_model,
        Engine::Cloud | Engine::Llm => "",
    }
}

/// Распознаёт запись выбранным движком.
pub fn transcribe(
    local: &mut LocalEngines,
    cfg: &Config,
    api_key: &str,
    engine: Engine,
    samples: &[f32],
) -> Result<String> {
    match engine {
        Engine::Cloud => {
            let wav = crate::audio::to_wav(samples)?;
            crate::providers::transcribe(&cfg.stt, api_key, wav)
        }
        Engine::Parakeet => {
            let model_id = model_id_for(&cfg.stt, engine).to_string();
            local.ensure(engine, &model_id)?;
            let out = local.run(samples);
            // Отметка ставится и при отказе: модель всё равно в памяти, и
            // отсчёт простоя должен идти от последнего обращения.
            local.last_used = Some(std::time::Instant::now());
            out
        }
        Engine::Llm => bail!("языковая модель не распознаёт речь"),
    }
}
