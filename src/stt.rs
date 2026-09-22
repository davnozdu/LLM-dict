//! Распознавание речи: облако и локальные движки за общим интерфейсом.
//!
//! Локальная модель весит больше полугигабайта и держится в памяти между
//! диктовками: загрузка занимает секунды, и делать её на каждую фразу
//! бессмысленно. По простою она выгружается — см. `unload`.

use crate::config::{Config, SttConfig};
use crate::models::{self, Engine};
use anyhow::{bail, Context, Result};

/// Загруженная модель. Какая именно — помнит `LocalEngines::loaded`.
enum Loaded {
    Parakeet(Box<parakeet_rs::ParakeetTDT>),
    GigaAm(Box<crate::gigaam::GigaAm>),
}

/// Надо ли грузить модель заново.
///
/// Вынесено отдельной функцией, потому что проверить это на самих моделях
/// нельзя: каждая весит сотни мегабайт и требует скачивания.
pub fn needs_reload(current: Option<(Engine, &str)>, engine: Engine, model_id: &str) -> bool {
    !matches!(current, Some((e, id)) if e == engine && id == model_id)
}

#[derive(Default)]
pub struct LocalEngines {
    /// Движок, идентификатор модели и она сама. Идентификатор хранится
    /// рядом, чтобы понимать, не сменился ли выбор в настройках.
    loaded: Option<(Engine, String, Loaded)>,
    /// Когда моделью пользовались в последний раз — по этому отсчитывается
    /// простой перед выгрузкой.
    last_used: Option<std::time::Instant>,
}

impl LocalEngines {
    fn ensure(&mut self, engine: Engine, model_id: &str) -> Result<()> {
        if !needs_reload(self.current(), engine, model_id) {
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

        let model = match engine {
            Engine::Parakeet => {
                let model = parakeet_rs::ParakeetTDT::from_pretrained(spec.dir(), None)
                    .map_err(|e| anyhow::anyhow!("не загрузить модель Parakeet: {e}"))?;
                Loaded::Parakeet(Box::new(model))
            }
            Engine::GigaAm => Loaded::GigaAm(Box::new(
                crate::gigaam::GigaAm::load(&spec.dir())
                    .with_context(|| format!("не загрузить модель {model_id}"))?,
            )),
            Engine::Cloud => bail!("облако не требует загрузки модели"),
            Engine::Llm => bail!("языковая модель загружается не здесь"),
        };
        self.loaded = Some((engine, model_id.to_string(), model));

        log::info!(
            "модель {model_id} загружена за {:.1} с",
            started.elapsed().as_secs_f32()
        );
        Ok(())
    }

    /// Язык не принимается: локальные движки его не выбирают, а облако
    /// получает настройку отдельно, в `providers::transcribe`.
    fn run(&mut self, samples: &[f32]) -> Result<String> {
        match self.loaded.as_mut() {
            Some((_, _, Loaded::Parakeet(model))) => {
                use parakeet_rs::Transcriber;
                let result = model
                    .transcribe_samples(samples.to_vec(), crate::audio::TARGET_RATE, 1, None)
                    .map_err(|e| anyhow::anyhow!("Parakeet: распознавание не удалось: {e}"))?;
                Ok(result.text.trim().to_string())
            }
            Some((_, _, Loaded::GigaAm(model))) => model.transcribe(samples),
            None => bail!("локальная модель не загружена"),
        }
    }

    /// Убирает из памяти модель, которую настройки больше не выбирают.
    ///
    /// Иначе прежняя модель жила бы до таймаута простоя или до следующей
    /// диктовки: сменив движок, пользователь продолжал бы платить за старый
    /// сотнями мегабайт. Порог простоя тут ни при чём — эта модель не нужна
    /// уже сейчас, сколько бы ей ни оставалось.
    ///
    /// Возвращает, пришлось ли что-то выгрузить.
    pub fn drop_if_stale(&mut self, cfg: &SttConfig) -> bool {
        if !self.current().is_some_and(|loaded| !is_wanted(cfg, loaded)) {
            return false;
        }
        self.unload();
        true
    }

    /// Выгружает модель из памяти. Файл на диске остаётся: «установлена» и
    /// «загружена» — разные вещи.
    pub fn unload(&mut self) {
        if self.loaded.take().is_some() {
            log::info!("модель распознавания выгружена из памяти");
        }
        self.last_used = None;
    }

    /// Что сейчас в памяти: движок и модель. Пусто — не загружено ничего.
    pub fn current(&self) -> Option<(Engine, &str)> {
        self.loaded.as_ref().map(|(e, id, _)| (*e, id.as_str()))
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

/// Что выбрано в настройках: движок и его модель.
///
/// У облака модели нет, и это не пробел в данных: по пустому имени видно,
/// что держать в памяти нечего.
pub fn selected(cfg: &SttConfig) -> (Engine, &str) {
    (cfg.engine, model_id_for(cfg, cfg.engine))
}

/// Нужна ли загруженная модель при нынешних настройках.
///
/// Основным движком дело не ограничивается: модель запасного держат в
/// памяти именно на случай отказа основного — выбранной в настройках она
/// не значится никогда, и по одному `selected` выглядела бы лишней.
pub fn is_wanted(cfg: &SttConfig, loaded: (Engine, &str)) -> bool {
    let fallback = cfg.fallback.map(|e| (e, model_id_for(cfg, e)));
    loaded == selected(cfg) || Some(loaded) == fallback
}

/// Какая модель выбрана для движка в настройках.
pub fn model_id_for(cfg: &SttConfig, engine: Engine) -> &str {
    match engine {
        Engine::Parakeet => &cfg.parakeet_model,
        Engine::GigaAm => &cfg.gigaam_model,
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
        Engine::Parakeet | Engine::GigaAm => {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Модель весит сотни мегабайт, и оставить в памяти прежнюю при смене
    /// выбора значило бы держать две сразу — или, что хуже, распознавать
    /// старой, пока пользователь думает, что переключился.
    #[test]
    fn другая_модель_того_же_движка_требует_перезагрузки() {
        assert!(needs_reload(
            Some((Engine::GigaAm, "gigaam-v3-e2e-ctc-int8")),
            Engine::GigaAm,
            "gigaam-v4-когда-нибудь",
        ));
    }

    #[test]
    fn смена_движка_требует_перезагрузки() {
        assert!(needs_reload(
            Some((Engine::Parakeet, "parakeet-tdt-0.6b-v3-int8")),
            Engine::GigaAm,
            "gigaam-v3-e2e-ctc-int8",
        ));
    }

    #[test]
    fn та_же_модель_повторно_не_грузится() {
        assert!(!needs_reload(
            Some((Engine::GigaAm, "gigaam-v3-e2e-ctc-int8")),
            Engine::GigaAm,
            "gigaam-v3-e2e-ctc-int8",
        ));
    }

    #[test]
    fn настройки_называют_модель_выбранного_движка() {
        let mut cfg = SttConfig {
            gigaam_model: "gigaam-v3-e2e-ctc-int8".into(),
            parakeet_model: "parakeet-tdt-0.6b-v3-int8".into(),
            engine: Engine::GigaAm,
            ..SttConfig::default()
        };
        assert_eq!(selected(&cfg), (Engine::GigaAm, "gigaam-v3-e2e-ctc-int8"));

        cfg.engine = Engine::Parakeet;
        assert_eq!(
            selected(&cfg),
            (Engine::Parakeet, "parakeet-tdt-0.6b-v3-int8")
        );
    }

    /// У облака модели в памяти нет, и «выбрана» для него — пустая строка:
    /// после перехода на облако локальная модель оказывается лишней.
    #[test]
    fn у_облака_локальной_модели_нет() {
        let cfg = SttConfig {
            engine: Engine::Cloud,
            ..SttConfig::default()
        };
        assert_eq!(selected(&cfg), (Engine::Cloud, ""));
        assert!(needs_reload(
            Some((Engine::GigaAm, "gigaam-v3-e2e-ctc-int8")),
            Engine::Cloud,
            "",
        ));
    }

    /// Сколько памяти занимает процесс, в килобайтах.
    fn память_процесса() -> u64 {
        let out = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &std::process::id().to_string()])
            .output()
            .expect("ps не запустился");
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .expect("ps вернул не число")
    }

    /// Смена модели в настройках должна освобождать память, а не копить
    /// модели: их тут сотни мегабайт каждая.
    ///
    /// Запуск: cargo test смена_модели_освобождает -- --ignored --nocapture
    #[test]
    #[ignore = "нужны скачанные модели обоих движков"]
    fn смена_модели_освобождает_память() {
        let mut local = LocalEngines::default();

        local.preload(Engine::GigaAm, "gigaam-v3-e2e-ctc-int8");
        assert_eq!(
            local.current(),
            Some((Engine::GigaAm, "gigaam-v3-e2e-ctc-int8")),
            "GigaAM не загрузился"
        );
        let с_гигаамом = память_процесса();

        local.unload();
        assert_eq!(local.current(), None, "выгрузка не очистила память движка");
        let после_выгрузки = память_процесса();

        let cfg = SttConfig {
            engine: Engine::Parakeet,
            ..SttConfig::default()
        };
        assert!(
            !local.drop_if_stale(&cfg),
            "выгружать нечего: память уже пуста"
        );
        local.preload(Engine::Parakeet, "parakeet-tdt-0.6b-v3-int8");
        assert_eq!(
            local.current(),
            Some((Engine::Parakeet, "parakeet-tdt-0.6b-v3-int8")),
            "Parakeet не загрузился"
        );
        let с_паракитом = память_процесса();

        // Настройки просят Parakeet, в памяти он и есть — трогать нечего.
        assert!(!local.drop_if_stale(&cfg), "выгрузил нужную модель");
        assert_eq!(local.current().map(|(e, _)| e), Some(Engine::Parakeet));

        // А теперь в настройках снова GigaAM: Parakeet стал лишним.
        let cfg = SttConfig {
            engine: Engine::GigaAm,
            ..SttConfig::default()
        };
        assert!(local.drop_if_stale(&cfg), "устаревшую модель не выгрузил");
        assert_eq!(local.current(), None, "модель осталась в памяти");

        println!(
            "память: GigaAM {} МБ → выгрузка {} МБ → Parakeet {} МБ",
            с_гигаамом / 1024,
            после_выгрузки / 1024,
            с_паракитом / 1024
        );
        assert!(
            после_выгрузки + 100_000 < с_гигаамом,
            "после выгрузки память не освободилась: было {с_гигаамом} КБ, стало {после_выгрузки} КБ"
        );
    }

    /// Модель запасного движка выгружать нельзя: её держат ровно на случай
    /// отказа основного, и у неё нет шанса «быть выбранной» в настройках.
    #[test]
    fn модель_запасного_движка_остаётся_нужной() {
        let cfg = SttConfig {
            engine: Engine::Cloud,
            fallback: Some(Engine::GigaAm),
            ..SttConfig::default()
        };
        assert!(is_wanted(&cfg, (Engine::GigaAm, "gigaam-v3-e2e-ctc-int8")));
        assert!(!is_wanted(
            &cfg,
            (Engine::Parakeet, "parakeet-tdt-0.6b-v3-int8")
        ));
    }

    #[test]
    fn без_запасного_движка_чужая_модель_лишняя() {
        let cfg = SttConfig {
            engine: Engine::Cloud,
            fallback: None,
            ..SttConfig::default()
        };
        assert!(!is_wanted(&cfg, (Engine::GigaAm, "gigaam-v3-e2e-ctc-int8")));
    }

    #[test]
    fn выбранная_модель_нужна() {
        let cfg = SttConfig {
            engine: Engine::GigaAm,
            ..SttConfig::default()
        };
        assert!(is_wanted(&cfg, (Engine::GigaAm, "gigaam-v3-e2e-ctc-int8")));
        assert!(!is_wanted(&cfg, (Engine::GigaAm, "gigaam-v4-когда-нибудь")));
    }

    #[test]
    fn пустая_память_требует_загрузки() {
        assert!(needs_reload(None, Engine::GigaAm, "gigaam-v3-e2e-ctc-int8"));
    }
}
