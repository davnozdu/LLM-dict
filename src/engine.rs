//! Общее состояние и рабочий поток: горячая клавиша → запись → распознавание →
//! пост-обработка → вставка → запись в историю.

use crate::audio;
use crate::config::{Config, HotKeyMode, PostMode};
use crate::history;
use crate::hotkey::{self, HotKeyEvent, HotKeyState};
use crate::insert;
use crate::permissions;
use crate::providers;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Idle,
    Recording,
    Transcribing,
    PostProcessing,
    Inserting,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Stage::Idle => "Готов",
            Stage::Recording => "Идёт запись",
            Stage::Transcribing => "Распознавание",
            Stage::PostProcessing => "Обработка моделью",
            Stage::Inserting => "Вставка",
        }
    }

    pub fn is_busy(self) -> bool {
        !matches!(self, Stage::Idle)
    }
}

pub struct Shared {
    pub config: RwLock<Config>,
    pub api_key: RwLock<String>,
    pub stage: Mutex<Stage>,
    pub level: Arc<audio::Level>,
    pub last_error: Mutex<Option<String>>,
    pub last_text: Mutex<Option<String>>,
    pub history: Mutex<Vec<history::Entry>>,
    pub hotkey_state: Arc<HotKeyState>,
    pub tap_running: AtomicBool,
    /// Растёт при каждом изменении, чтобы трей перерисовал иконку.
    pub dirty: AtomicBool,
}

impl Shared {
    pub fn new(config: Config) -> Arc<Self> {
        let hotkey_state = Arc::new(HotKeyState::new(
            config.general.hotkey,
            config.general.hotkey_mode,
        ));
        let limit = config.general.history_limit;
        Arc::new(Self {
            config: RwLock::new(config),
            api_key: RwLock::new(crate::config::secrets::get("groq_api_key").unwrap_or_default()),
            stage: Mutex::new(Stage::Idle),
            level: Arc::new(audio::Level::default()),
            last_error: Mutex::new(None),
            last_text: Mutex::new(None),
            history: Mutex::new(history::load(limit)),
            hotkey_state,
            tap_running: AtomicBool::new(false),
            dirty: AtomicBool::new(true),
        })
    }

    pub fn stage(&self) -> Stage {
        *self.stage.lock().unwrap()
    }

    fn set_stage(&self, s: Stage) {
        *self.stage.lock().unwrap() = s;
        self.dirty.store(true, Ordering::Relaxed);
    }

    fn set_error(&self, e: Option<String>) {
        if let Some(msg) = &e {
            log::error!("{msg}");
        }
        *self.last_error.lock().unwrap() = e;
        self.dirty.store(true, Ordering::Relaxed);
    }

    pub fn config_snapshot(&self) -> Config {
        self.config.read().unwrap().clone()
    }

    pub fn api_key_snapshot(&self) -> String {
        self.api_key.read().unwrap().clone()
    }

    /// Применяет изменения горячей клавиши к живому тапу без перезапуска.
    pub fn sync_hotkey(&self) {
        let cfg = self.config.read().unwrap();
        self.hotkey_state.set_key(cfg.general.hotkey);
        self.hotkey_state.set_mode(cfg.general.hotkey_mode);
    }
}

/// Слишком короткое нажатие — это промах по клавише, а не диктовка.
const MIN_DURATION_SECS: f32 = 0.35;

pub fn spawn(shared: Arc<Shared>) -> Sender<HotKeyEvent> {
    let (tx, rx) = mpsc::channel::<HotKeyEvent>();

    // Поток тапа: только читает события и шлёт их сюда.
    let tap_handle = hotkey::spawn(shared.hotkey_state.clone(), tx.clone());
    {
        let shared = shared.clone();
        std::thread::spawn(move || {
            // Если тап не поднялся, поток завершится сразу и вернёт false.
            std::thread::sleep(std::time::Duration::from_millis(400));
            if tap_handle.is_finished() {
                shared.tap_running.store(false, Ordering::Relaxed);
                shared.set_error(Some(
                    "Горячая клавиша не работает: нет разрешения «Универсальный доступ»".into(),
                ));
            } else {
                shared.tap_running.store(true, Ordering::Relaxed);
            }
        });
    }

    let worker_shared = shared.clone();
    std::thread::spawn(move || worker(worker_shared, rx));
    tx
}

fn worker(shared: Arc<Shared>, rx: Receiver<HotKeyEvent>) {
    let mut recording: Option<audio::Recording> = None;
    let mut started_at: Option<Instant> = None;

    while let Ok(event) = rx.recv() {
        match event {
            HotKeyEvent::StartRecording => {
                if recording.is_some() {
                    continue;
                }
                let cfg = shared.config_snapshot();
                match audio::start(shared.level.clone()) {
                    Ok(rec) => {
                        recording = Some(rec);
                        started_at = Some(Instant::now());
                        shared.hotkey_state.set_recording(true);
                        shared.set_stage(Stage::Recording);
                        shared.set_error(None);
                        if cfg.general.play_sounds {
                            insert::play_sound("Tink");
                        }
                    }
                    Err(e) => {
                        shared.hotkey_state.set_recording(false);
                        shared.set_error(Some(format!("Микрофон недоступен: {e}")));
                    }
                }
            }

            HotKeyEvent::StopRecording => {
                let Some(rec) = recording.take() else {
                    shared.hotkey_state.set_recording(false);
                    continue;
                };
                shared.hotkey_state.set_recording(false);
                let samples = rec.finish();
                let elapsed = started_at.take().map(|t| t.elapsed());
                let cfg = shared.config_snapshot();

                if audio::duration_secs(&samples) < MIN_DURATION_SECS {
                    shared.set_stage(Stage::Idle);
                    continue;
                }
                if cfg.general.play_sounds {
                    insert::play_sound("Pop");
                }
                let started = Instant::now();
                let result = process(&shared, &cfg, samples);
                record_result(&shared, &cfg, result, started, elapsed);
                shared.set_stage(Stage::Idle);
            }
        }
    }
}

struct Outcome {
    raw_text: String,
    final_text: String,
    duration_secs: f32,
    clipboard_before: Option<String>,
}

fn process(shared: &Arc<Shared>, cfg: &Config, samples: Vec<f32>) -> anyhow::Result<Outcome> {
    let duration_secs = audio::duration_secs(&samples);
    let wav = audio::to_wav(&samples)?;
    let key = shared.api_key_snapshot();

    shared.set_stage(Stage::Transcribing);
    let raw_text = providers::transcribe(&cfg.stt, &key, wav)?;
    if raw_text.trim().is_empty() {
        anyhow::bail!("распознавание вернуло пустой текст — тишина или слишком тихий микрофон");
    }

    let final_text = if matches!(cfg.llm.mode, PostMode::Raw) {
        raw_text.clone()
    } else {
        shared.set_stage(Stage::PostProcessing);
        providers::post_process(&cfg.llm, &key, &raw_text)?
    };

    shared.set_stage(Stage::Inserting);
    let clipboard_before = insert::insert(&final_text, cfg.general.restore_clipboard)?;

    Ok(Outcome {
        raw_text,
        final_text,
        duration_secs,
        clipboard_before,
    })
}

fn record_result(
    shared: &Arc<Shared>,
    cfg: &Config,
    result: anyhow::Result<Outcome>,
    started: Instant,
    _elapsed: Option<std::time::Duration>,
) {
    let latency_ms = started.elapsed().as_millis() as u64;
    let entry = match result {
        Ok(out) => {
            *shared.last_text.lock().unwrap() = Some(out.final_text.clone());
            shared.set_error(None);
            history::Entry {
                at: chrono::Local::now(),
                duration_secs: out.duration_secs,
                raw_text: out.raw_text,
                final_text: out.final_text,
                mode: cfg.llm.mode.label().to_string(),
                stt_model: cfg.stt.model.clone(),
                llm_model: (!matches!(cfg.llm.mode, PostMode::Raw)).then(|| cfg.llm.model.clone()),
                latency_ms,
                clipboard_before: out.clipboard_before,
                error: None,
            }
        }
        Err(e) => {
            let msg = e.to_string();
            shared.set_error(Some(msg.clone()));
            if cfg.general.play_sounds {
                insert::play_sound("Basso");
            }
            history::Entry {
                at: chrono::Local::now(),
                duration_secs: 0.0,
                raw_text: String::new(),
                final_text: String::new(),
                mode: cfg.llm.mode.label().to_string(),
                stt_model: cfg.stt.model.clone(),
                llm_model: None,
                latency_ms,
                clipboard_before: None,
                error: Some(msg),
            }
        }
    };

    let _ = history::append(&entry);
    let mut hist = shared.history.lock().unwrap();
    hist.insert(0, entry);
    hist.truncate(cfg.general.history_limit);
    drop(hist);
    let _ = history::trim(cfg.general.history_limit);
    shared.dirty.store(true, Ordering::Relaxed);
}

/// Разрешения, которых не хватает для работы. Для баннера в UI.
pub fn missing_permissions() -> Vec<&'static str> {
    let mut out = Vec::new();
    if !permissions::accessibility().is_ok() {
        out.push("Универсальный доступ");
    }
    if !permissions::microphone().is_ok() {
        out.push("Микрофон");
    }
    out
}

pub fn hotkey_mode_label(mode: HotKeyMode) -> &'static str {
    match mode {
        HotKeyMode::Hold => "Удержание",
        HotKeyMode::Toggle => "Нажатие / повторное нажатие",
    }
}
