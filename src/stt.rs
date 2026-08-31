//! Распознавание речи: облако и два локальных движка за общим интерфейсом.
//!
//! Локальные модели грузятся по 0.5–1 ГБ и держатся в памяти между диктовками:
//! загрузка занимает секунды, и делать её на каждую фразу бессмысленно.

use crate::config::{Config, SttConfig};
use crate::models::{self, Engine};
use anyhow::{bail, Context, Result};

/// Загруженная локальная модель. Хранится между вызовами, поэтому помнит,
/// что именно загружено — при смене модели в настройках надо перезагрузиться.
enum Loaded {
    Whisper {
        id: String,
        ctx: Box<whisper_rs::WhisperContext>,
    },
    Parakeet {
        id: String,
        model: Box<parakeet_rs::ParakeetTDT>,
    },
}

impl Loaded {
    fn matches(&self, engine: Engine, model_id: &str) -> bool {
        match self {
            Loaded::Whisper { id, .. } => engine == Engine::Whisper && id == model_id,
            Loaded::Parakeet { id, .. } => engine == Engine::Parakeet && id == model_id,
        }
    }
}

#[derive(Default)]
pub struct LocalEngines {
    loaded: Option<Loaded>,
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
            Engine::Whisper => {
                let path = spec.dir().join(spec.files[0].name);
                let mut params = whisper_rs::WhisperContextParameters::default();
                params.use_gpu(true);
                let ctx = whisper_rs::WhisperContext::new_with_params(&path, params)
                    .map_err(|e| anyhow::anyhow!("не загрузить модель Whisper: {e}"))?;
                Loaded::Whisper {
                    id: model_id.to_string(),
                    ctx: Box::new(ctx),
                }
            }
            Engine::Parakeet => {
                let model = parakeet_rs::ParakeetTDT::from_pretrained(spec.dir(), None)
                    .map_err(|e| anyhow::anyhow!("не загрузить модель Parakeet: {e}"))?;
                Loaded::Parakeet {
                    id: model_id.to_string(),
                    model: Box::new(model),
                }
            }
            Engine::Cloud => bail!("облако не требует загрузки модели"),
        });

        log::info!(
            "модель {model_id} загружена за {:.1} с",
            started.elapsed().as_secs_f32()
        );
        Ok(())
    }

    fn run(&mut self, samples: &[f32], language: &str) -> Result<String> {
        match self.loaded.as_mut() {
            Some(Loaded::Whisper { ctx, .. }) => {
                let mut state = ctx
                    .create_state()
                    .map_err(|e| anyhow::anyhow!("Whisper: не создать состояние: {e}"))?;

                let mut params =
                    whisper_rs::FullParams::new(whisper_rs::SamplingStrategy::Greedy {
                        best_of: 1,
                    });
                // Логи whisper.cpp в консоль нам не нужны — у нас свой журнал.
                params.set_print_special(false);
                params.set_print_progress(false);
                params.set_print_realtime(false);
                params.set_print_timestamps(false);
                params.set_translate(false);
                params.set_suppress_blank(true);
                // Оставляем ядро свободным под интерфейс и запись.
                let threads = (num_cpus().saturating_sub(1)).clamp(1, 8);
                params.set_n_threads(threads as i32);
                // Без явного языка whisper-rs декодирует как английский, а не
                // определяет сам: русская речь превращалась в смесь латиницы
                // с кириллицей, а короткие фразы — в пустую строку.
                let lang = language.trim();
                params.set_language(Some(if lang.is_empty() { "auto" } else { lang }));

                state
                    .full(params, samples)
                    .map_err(|e| anyhow::anyhow!("Whisper: распознавание не удалось: {e}"))?;

                let mut text = String::new();
                for segment in state.as_iter() {
                    if let Ok(s) = segment.to_str_lossy() {
                        text.push_str(&s);
                    }
                }
                Ok(text.trim().to_string())
            }
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

    /// Прогревает модель заранее, чтобы первая диктовка не ждала загрузку.
    pub fn preload(&mut self, engine: Engine, model_id: &str) {
        if engine.is_local() {
            if let Err(e) = self.ensure(engine, model_id) {
                log::warn!("предзагрузка {model_id}: {e}");
            }
        }
    }
}

fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

/// Какая модель выбрана для движка в настройках.
fn model_id_for(cfg: &SttConfig, engine: Engine) -> &str {
    match engine {
        Engine::Whisper => &cfg.whisper_model,
        Engine::Parakeet => &cfg.parakeet_model,
        Engine::Cloud => "",
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
        Engine::Whisper | Engine::Parakeet => {
            let model_id = model_id_for(&cfg.stt, engine).to_string();
            local.ensure(engine, &model_id)?;
            local.run(samples, &cfg.stt.language)
        }
    }
}
