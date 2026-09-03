//! Окно настроек, статуса и истории.

use crate::actions::{Output, TextAction};
use crate::binding::Binding;
use crate::config::{secrets, ClipboardMode, Config, HotKeyMode, PostMode};
use crate::engine::{self, Shared, Stage};
use crate::history;
use crate::insert;
use crate::models::{self, Engine};
use crate::permissions;
use crate::provider::{Endpoint, Provider};
use crate::{autostart, conflicts, macos, providers, updater};
use std::collections::HashMap;

use eframe::egui;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};

/// Момент в прошлом, пригодный как «давно не проверяли».
///
/// Прямое вычитание из `Instant::now()` паникует в первые секунды после
/// загрузки системы: отсчёт идёт от неё. Для приложения в автозапуске это
/// падение при входе в систему.
fn long_ago(secs: u64) -> Instant {
    Instant::now()
        .checked_sub(Duration::from_secs(secs))
        .unwrap_or_else(Instant::now)
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Tab {
    Status,
    Actions,
    Settings,
    History,
    Clipboard,
    Permissions,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum HistoryFilter {
    All,
    Transformed,
    Errors,
}

const MENU_OPEN: &str = "open";
const MENU_QUIT: &str = "quit";

pub struct App {
    shared: Arc<Shared>,
    cfg: Config,
    saved_cfg: Config,
    api_key_input: String,
    tab: Tab,
    models: Vec<String>,
    model_check: Option<Receiver<Result<Vec<String>, String>>>,
    check_message: Option<(String, bool)>,
    tray: Option<TrayIcon>,
    tray_stage: Option<Stage>,
    autostart_on: bool,
    filter: HistoryFilter,
    toast: Option<(String, Instant)>,
    perms: (permissions::Status, permissions::Status),
    perms_checked: Instant,
    capturing: bool,
    capture_preview: Vec<u16>,
    verdict: conflicts::Verdict,
    verdict_for: Binding,
    update: UpdateState,
    update_checked: bool,
    /// Открыто окно выбора из буфера обмена.
    picker: bool,
    picker_query: String,
    picker_index: usize,
    /// Куда вернуть фокус после выбора.
    picker_return_pid: Option<i32>,
    /// Когда список открылся. Нужно, чтобы не закрыть его от потери фокуса
    /// в тот же миг: фокус приходит с задержкой в несколько кадров.
    picker_opened: Instant,
    /// Размер обычного окна, чтобы вернуть его после списка: список
    /// перекраивает то же самое окно, а не заводит своё.
    main_size: egui::Vec2,
    /// Набирается сочетание именно для окна буфера, а не для действия.
    capturing_clipboard: bool,
    duplicates: Vec<String>,
    duplicates_checked: Instant,
    key_synced: bool,
    overlay: crate::overlay::Overlay,
    /// Идущая загрузка модели: её id, прогресс и канал с результатом.
    download: Option<Download>,
    /// Показывать ключ эндпоинта открытым текстом.
    show_server_key: bool,
    /// Какое действие сейчас раскрыто в редакторе.
    editing_action: Option<String>,
    /// Для какого действия набирается сочетание.
    capturing_action: Option<String>,
    /// Списки моделей, считанные с поставщиков.
    provider_models: HashMap<Provider, Vec<String>>,
    /// Идущее чтение списка моделей.
    model_fetch: Option<ModelFetch>,
    /// Ключи поставщиков в полях ввода.
    provider_keys: HashMap<Provider, String>,
}

/// Что именно спрашиваем у поставщика: список моделей или годность ключа.
#[derive(PartialEq, Eq, Clone, Copy)]
enum FetchKind {
    Models,
    KeyCheck,
}

/// Идущий запрос к поставщику.
type ModelFetch = (Provider, FetchKind, Receiver<Result<Vec<String>, String>>);

struct Download {
    model_id: String,
    progress: Arc<models::Progress>,
    result: Receiver<Result<(), String>>,
}

enum UpdateState {
    Idle,
    Checking(Receiver<Result<Option<updater::Release>, String>>),
    UpToDate,
    Available(updater::Release),
    Installing(Receiver<Result<std::path::PathBuf, String>>),
    Installed(std::path::PathBuf),
    Failed(String),
}

impl App {
    pub fn new(shared: Arc<Shared>) -> Self {
        let cfg = shared.config_snapshot();
        let api_key_input = shared.api_key_snapshot();
        Self {
            saved_cfg: cfg.clone(),
            cfg,
            api_key_input,
            shared,
            tab: Tab::Status,
            models: Vec::new(),
            model_check: None,
            check_message: None,
            tray: None,
            tray_stage: None,
            autostart_on: autostart::is_enabled(),
            filter: HistoryFilter::All,
            toast: None,
            perms: (permissions::Status::NotAsked, permissions::Status::NotAsked),
            // checked_sub, а не вычитание: Instant отсчитывается от загрузки
            // системы, и в первую минуту после неё вычитание паникует.
            // Приложение стоит в автозапуске, то есть стартует именно тогда.
            perms_checked: long_ago(10),
            capturing: false,
            capture_preview: Vec::new(),
            verdict: conflicts::Verdict::Free,
            verdict_for: Binding::new(Vec::new()),
            update: UpdateState::Idle,
            update_checked: false,
            picker: false,
            picker_query: String::new(),
            picker_index: 0,
            picker_return_pid: None,
            picker_opened: long_ago(10),
            main_size: egui::vec2(600.0, 680.0),
            capturing_clipboard: false,
            duplicates: Vec::new(),
            duplicates_checked: long_ago(60),
            key_synced: false,
            overlay: crate::overlay::Overlay::new(),
            download: None,
            show_server_key: false,
            editing_action: None,
            capturing_action: None,
            provider_models: HashMap::new(),
            model_fetch: None,
            provider_keys: HashMap::new(),
        }
    }

    fn toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    fn apply_config(&mut self) {
        if self.cfg == self.saved_cfg {
            return;
        }
        *self.shared.config.write().unwrap() = self.cfg.clone();
        self.shared.sync_hotkey();
        if self.cfg.general.show_in_dock != self.saved_cfg.general.show_in_dock {
            macos::set_dock_visible(self.cfg.general.show_in_dock);
        }
        if let Err(e) = self.cfg.save() {
            self.toast(format!("Не сохранить настройки: {e}"));
        }
        self.saved_cfg = self.cfg.clone();
    }

    /// Иконка в панели: цвет кружка = стадия работы.
    fn tray_icon_for(stage: Stage) -> tray_icon::Icon {
        const S: usize = 22;
        let (r, g, b) = match stage {
            Stage::Idle => (140u8, 140u8, 140u8),
            Stage::LoadingModel => (150, 120, 200),
            Stage::ActionRunning => (90, 140, 240),
            Stage::Recording => (230, 60, 60),
            Stage::Transcribing => (230, 160, 40),
            Stage::PostProcessing => (90, 140, 240),
            Stage::Inserting => (60, 190, 110),
        };
        let mut rgba = vec![0u8; S * S * 4];
        let c = (S as f32 - 1.0) / 2.0;
        let radius = S as f32 * 0.36;
        for y in 0..S {
            for x in 0..S {
                let d = ((x as f32 - c).powi(2) + (y as f32 - c).powi(2)).sqrt();
                // Мягкий край, иначе кружок выглядит рваным.
                let a = ((radius + 0.5 - d).clamp(0.0, 1.0) * 255.0) as u8;
                let i = (y * S + x) * 4;
                rgba[i] = r;
                rgba[i + 1] = g;
                rgba[i + 2] = b;
                rgba[i + 3] = a;
            }
        }
        tray_icon::Icon::from_rgba(rgba, S as u32, S as u32)
            .expect("иконка трея фиксированного размера")
    }

    fn ensure_tray(&mut self) {
        if self.tray.is_some() {
            return;
        }
        let menu = Menu::new();
        let open = MenuItem::with_id(MENU_OPEN, "Открыть LLM-dict", true, None);
        let quit = MenuItem::with_id(MENU_QUIT, "Выход", true, None);
        let _ = menu.append(&open);
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&quit);

        match TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("LLM-dict")
            .with_icon(Self::tray_icon_for(Stage::Idle))
            .build()
        {
            Ok(t) => {
                log::info!("иконка в панели создана");
                self.tray = Some(t);
                self.tray_stage = Some(Stage::Idle);
            }
            Err(e) => log::error!("не создать иконку в панели: {e}"),
        }
    }

    fn sync_tray(&mut self) {
        let stage = self.shared.stage();
        if self.tray_stage == Some(stage) {
            return;
        }
        if let Some(tray) = &self.tray {
            let _ = tray.set_icon(Some(Self::tray_icon_for(stage)));
            let _ = tray.set_tooltip(Some(format!("LLM-dict — {}", stage.label())));
        }
        self.tray_stage = Some(stage);
    }

    fn poll_model_check(&mut self) {
        let Some(rx) = &self.model_check else { return };
        match rx.try_recv() {
            Ok(Ok(models)) => {
                let n = models.len();
                self.models = models;
                self.check_message = Some((format!("Ключ принят, моделей доступно: {n}"), true));
                self.model_check = None;
            }
            Ok(Err(e)) => {
                self.check_message = Some((format!("Ошибка: {e}"), false));
                self.model_check = None;
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => self.model_check = None,
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
    }

    /// Пересчитывает вердикт о занятости сочетания, но только когда оно
    /// изменилось: чтение системных настроек — это запуск plutil.
    fn refresh_verdict(&mut self) {
        if self.verdict_for != self.cfg.general.hotkey {
            self.verdict = conflicts::check(&self.cfg.general.hotkey);
            self.verdict_for = self.cfg.general.hotkey.clone();
        }
    }

    fn set_capturing(&mut self, on: bool) {
        self.capturing = on;
        self.capture_preview.clear();
        let _ = self.shared.take_captured();
        self.shared.hotkey_state.set_capturing(on);
    }

    /// Ключ читается фоном, а поле ввода заполняется при создании окна —
    /// то есть заведомо раньше. Без досыла поле осталось бы пустым, и первое
    /// же «Сохранить» стёрло бы настоящий ключ.
    fn poll_api_key(&mut self) {
        if self.key_synced || !self.shared.key_loaded() {
            return;
        }
        self.key_synced = true;
        if self.api_key_input.is_empty() {
            self.api_key_input = self.shared.api_key_snapshot();
        }
    }

    fn poll_download(&mut self) {
        let Some(dl) = &self.download else { return };
        match dl.result.try_recv() {
            Ok(Ok(())) => {
                let id = dl.model_id.clone();
                self.toast(format!("Модель {id} скачана"));
                self.download = None;
            }
            Ok(Err(e)) => {
                self.toast(format!("Загрузка не удалась: {e}"));
                self.download = None;
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => self.download = None,
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
    }

    fn start_download(&mut self, spec: &'static models::ModelSpec) {
        let progress = Arc::new(models::Progress::default());
        let (tx, rx) = std::sync::mpsc::channel();
        let p = progress.clone();
        std::thread::spawn(move || {
            let _ = tx.send(models::download(spec, p).map_err(|e| e.to_string()));
        });
        self.download = Some(Download {
            model_id: spec.id.to_string(),
            progress,
            result: rx,
        });
    }

    fn poll_capture(&mut self) {
        if !self.capturing {
            return;
        }
        if let Some(keys) = self.shared.take_captured() {
            self.capture_preview = keys;
        }
    }

    fn start_update_check(&mut self) {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(updater::check().map_err(|e| e.to_string()));
        });
        self.update = UpdateState::Checking(rx);
    }

    fn start_update_install(&mut self, release: updater::Release) {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(updater::install(&release).map_err(|e| e.to_string()));
        });
        self.update = UpdateState::Installing(rx);
    }

    fn poll_update(&mut self) {
        let next = match &self.update {
            UpdateState::Checking(rx) => match rx.try_recv() {
                Ok(Ok(Some(r))) => Some(UpdateState::Available(r)),
                Ok(Ok(None)) => Some(UpdateState::UpToDate),
                Ok(Err(e)) => Some(UpdateState::Failed(e)),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(UpdateState::Idle),
                Err(_) => None,
            },
            UpdateState::Installing(rx) => match rx.try_recv() {
                Ok(Ok(path)) => Some(UpdateState::Installed(path)),
                Ok(Err(e)) => Some(UpdateState::Failed(e)),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(UpdateState::Idle),
                Err(_) => None,
            },
            _ => None,
        };
        if let Some(next) = next {
            self.update = next;
        }
    }

    /// Маленькая плашка у курсора: видно, что диктовка идёт, даже когда окно
    /// приложения закрыто. Окно не активируется и не перехватывает мышь,
    /// иначе оно уводило бы фокус из программы, куда мы собираемся вставлять.
    /// Плашка у курсора. Живёт в `logic`, а не в `ui`: пока главное окно
    /// скрыто, eframe не выполняет кадры интерфейса, и плашка на окне egui
    /// оставалась бы на экране навсегда.
    fn update_overlay(&mut self, ctx: &egui::Context) {
        if !self.cfg.general.show_overlay {
            self.overlay.hide();
            return;
        }

        let stage = self.shared.stage();
        let notice = self.shared.notice_with_fade();

        let (text, fade) = match (notice, stage) {
            (Some((msg, fade)), _) => (msg, fade),
            (None, Stage::Recording) => ("Слушаю…".to_string(), 1.0),
            (None, Stage::LoadingModel) => ("Загружаю модель…".to_string(), 1.0),
            (None, Stage::Transcribing) => ("Распознаю…".to_string(), 1.0),
            (None, Stage::PostProcessing) | (None, Stage::ActionRunning) => {
                ("Обрабатываю…".to_string(), 1.0)
            }
            (None, Stage::Inserting) => ("Вставляю…".to_string(), 1.0),
            (None, Stage::Idle) => {
                self.overlay.hide();
                return;
            }
        };

        let Some(pos) = macos::cursor_position() else {
            return;
        };
        // Полупрозрачная и гаснущая — просили ненавязчивую.
        self.overlay.show(&text, pos, 0.88 * fade);
        // Часто перерисовывать нужно только пока сообщение гаснет. В
        // остальное время плашка неподвижна, и лишние кадры только грузят
        // систему — на них жаловались как на подтормаживание курсора.
        let interval = if fade < 1.0 { 40 } else { 300 };
        ctx.request_repaint_after(Duration::from_millis(interval));
    }

    /// Ключ уходит туда, куда указано настройками, а из другого места
    /// подчищается: иначе он остался бы лежать в двух местах сразу.
    fn save_api_key(&mut self) {
        let key = self.api_key_input.trim().to_string();
        if self.cfg.general.key_in_config {
            self.cfg.api_key = key.clone();
            let _ = secrets::set("groq_api_key", "");
            self.toast("Ключ сохранён в файле настроек");
        } else {
            self.cfg.api_key.clear();
            match secrets::set("groq_api_key", &key) {
                Ok(()) => self.toast("Ключ сохранён в Keychain"),
                Err(e) => {
                    self.toast(format!("Keychain: {e}"));
                    return;
                }
            }
        }
        *self.shared.api_key.write().unwrap() = key;
    }

    fn start_model_check(&mut self) {
        let (tx, rx) = std::sync::mpsc::channel();
        let base = self.cfg.stt.base_url.clone();
        let key = self.api_key_input.clone();
        std::thread::spawn(move || {
            let res = providers::list_models(&base, &key).map_err(|e| e.to_string());
            let _ = tx.send(res);
        });
        self.model_check = Some(rx);
        self.check_message = Some(("Проверяю…".into(), true));
    }
}

impl eframe::App for App {
    /// Выполняется и когда окно скрыто — значит сюда идёт всё, что должно
    /// работать в фоне: значок в панели и плашка у курсора.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.ensure_tray();
        self.sync_tray();
        self.update_overlay(ctx);

        while let Ok(event) = MenuEvent::receiver().try_recv() {
            match event.id.as_ref() {
                MENU_OPEN => {
                    // Список и настройки живут в одном окне: не закрыв список,
                    // мы показали бы настройки плашкой без рамки поверх всего.
                    if self.picker {
                        self.close_picker(ctx);
                    }
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    macos::activate();
                }
                MENU_QUIT => std::process::exit(0),
                _ => {}
            }
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        self.poll_model_check();
        // Запрос на окно буфера приходит из перехватчика клавиш.
        if self
            .shared
            .clipboard_requested
            .swap(false, std::sync::atomic::Ordering::Relaxed)
        {
            self.open_picker(ctx);
        }

        // Список из буфера занимает всё окно: ни вкладок, ни строки состояния
        // у него нет — только записи на полупрозрачной плашке.
        if self.picker {
            self.ui_picker_window(ui);
            self.apply_config();
            ctx.request_repaint_after(Duration::from_millis(50));
            return;
        }

        // Размер обычного окна запоминаем, пока оно обычное: списку оно
        // достаётся перекроенным, и вернуть надо именно то, что было.
        if let Some(rect) = ctx.input(|i| i.viewport().inner_rect) {
            if rect.width() > 100.0 && rect.height() > 100.0 {
                self.main_size = rect.size();
            }
        }

        self.poll_api_key();
        self.poll_download();
        self.poll_capture();
        self.poll_update();
        self.refresh_verdict();

        // Проверка обновлений один раз за запуск, чтобы не дёргать GitHub.
        if self.cfg.general.check_updates && !self.update_checked {
            self.update_checked = true;
            self.start_update_check();
        }

        // Разрешения опрашиваем раз в секунду: вызовы дешёвые, но не бесплатные.
        if self.perms_checked.elapsed() > Duration::from_secs(1) {
            self.perms = (permissions::accessibility(), permissions::microphone());
            self.perms_checked = Instant::now();
        }

        // Закрытие окна прячет его, а не завершает приложение.
        if ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }

        egui::Panel::top("tabs").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Status, "Статус");
                ui.selectable_value(&mut self.tab, Tab::Actions, "Действия");
                ui.selectable_value(&mut self.tab, Tab::Settings, "Настройки");
                ui.selectable_value(&mut self.tab, Tab::History, "История");
                ui.selectable_value(&mut self.tab, Tab::Clipboard, "Буфер");
                let perms_ok = self.perms.0.is_ok() && self.perms.1.is_ok();
                let label = if perms_ok {
                    "Права"
                } else {
                    "Права — нужны"
                };
                ui.selectable_value(&mut self.tab, Tab::Permissions, label);
            });
            ui.add_space(4.0);
        });

        egui::Panel::bottom("status_bar").show(ui, |ui| {
            ui.add_space(3.0);
            ui.horizontal(|ui| {
                let stage = self.shared.stage();
                // Слежение за клавишей — это и есть «работает»: без него
                // приложение висит в памяти и ничего не делает.
                let listening = self.shared.tap_running.load(Ordering::Relaxed)
                    && !self.shared.hotkey_state.is_disabled_by_system();
                let dot = match stage {
                    Stage::Idle if listening => egui::Color32::from_rgb(60, 190, 110),
                    Stage::Idle => egui::Color32::from_rgb(220, 130, 40),
                    Stage::LoadingModel => egui::Color32::from_rgb(150, 120, 200),
                    Stage::ActionRunning => egui::Color32::from_rgb(90, 140, 240),
                    Stage::Recording => egui::Color32::from_rgb(230, 60, 60),
                    Stage::Transcribing => egui::Color32::from_rgb(230, 160, 40),
                    Stage::PostProcessing => egui::Color32::from_rgb(90, 140, 240),
                    Stage::Inserting => egui::Color32::from_rgb(60, 190, 110),
                };
                // Кружок рисуем, а не пишем символом: подходящего глифа может
                // не оказаться в шрифте, и вместо него вылезет пустой квадрат.
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                ui.painter().circle_filled(rect.center(), 4.0, dot);
                ui.label(match stage {
                    Stage::Idle if listening => "Работает",
                    Stage::Idle => "Не работает",
                    other => other.label(),
                });
                ui.separator();
                ui.label(format!(
                    "{} · {}",
                    self.cfg.general.hotkey.label(),
                    engine::hotkey_mode_label(self.cfg.general.hotkey_mode)
                ));
                ui.separator();
                ui.label(self.cfg.stt.engine.label());
                if !crate::net::is_online() {
                    ui.separator();
                    ui.colored_label(egui::Color32::from_rgb(220, 130, 40), "нет сети");
                }
                if let Some((msg, at)) = &self.toast {
                    if at.elapsed() < Duration::from_secs(4) {
                        ui.separator();
                        ui.label(msg);
                    }
                }
            });
            ui.add_space(3.0);
        });

        egui::CentralPanel::default().show(ui, |ui| match self.tab {
            Tab::Status => self.ui_status(ui),
            Tab::Actions => self.ui_actions(ui),
            Tab::Settings => self.ui_settings(ui),
            Tab::History => self.ui_history(ui),
            Tab::Clipboard => self.ui_clipboard(ui),
            Tab::Permissions => self.ui_permissions(ui),
        });

        self.apply_config();

        // Во время набора сочетания опрос должен успевать за пальцами: при
        // 500 мс между кадрами третья клавиша не попадала в превью, и казалось,
        // что приложение читает только две.
        if self.capturing {
            ctx.request_repaint_after(Duration::from_millis(16));
        } else if self.shared.stage().is_busy() || self.model_check.is_some() {
            ctx.request_repaint_after(Duration::from_millis(60));
        } else {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }

    /// Окно создано прозрачным — иначе список из буфера не смог бы стать
    /// полупрозрачным на ходу: режим смешивания выбирается один раз, при
    /// создании поверхности. В обычном виде заливаем непрозрачным, и окно
    /// выглядит как всегда.
    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        if self.picker {
            egui::Color32::TRANSPARENT.to_normalized_gamma_f32()
        } else {
            visuals.panel_fill.to_opaque().to_normalized_gamma_f32()
        }
    }
}

impl App {
    fn ui_status(&mut self, ui: &mut egui::Ui) {
        let stage = self.shared.stage();

        ui.add_space(6.0);
        ui.heading(stage.label());
        ui.add_space(4.0);

        if matches!(stage, Stage::Idle) {
            ui.label(format!(
                "{} — {}. Текст встанет туда, где стоит курсор.",
                self.cfg.general.hotkey.label(),
                engine::hotkey_mode_hint(self.cfg.general.hotkey_mode)
            ));
        }

        ui.add_space(8.0);
        let level = self.shared.level.get();
        ui.add(
            egui::ProgressBar::new(level.min(1.0))
                .desired_width(ui.available_width())
                .text(if matches!(stage, Stage::Recording) {
                    "уровень сигнала"
                } else {
                    ""
                }),
        );

        ui.add_space(10.0);
        ui.separator();
        ui.add_space(6.0);

        // Быстрое переключение режима — то, что меняют чаще всего.
        ui.horizontal(|ui| {
            ui.label("Режим обработки:");
            for m in PostMode::ALL {
                ui.selectable_value(&mut self.cfg.llm.mode, m, m.label());
            }
        });
        if matches!(self.cfg.llm.mode, PostMode::Translate) {
            ui.horizontal(|ui| {
                ui.label("Переводить на:");
                ui.text_edit_singleline(&mut self.cfg.llm.target_language);
            });
        }

        ui.add_space(8.0);

        // Без «Мониторинга ввода» работают только сочетания из модификаторов.
        // Диктовка на правом ⌘ при этом идёт как ни в чём не бывало, поэтому
        // без явного предупреждения причина неочевидна.
        if !permissions::input_monitoring().is_ok() {
            ui.colored_label(
                egui::Color32::from_rgb(220, 80, 80),
                "Нет разрешения «Мониторинг ввода»",
            );
            ui.label(
                "Сочетания с буквами и цифрами работать не будут: macOS отдаёт \
                 приложению только модификаторы. Сочетания из одних модификаторов \
                 при этом работают, поэтому со стороны похоже на выборочный сбой.",
            );
            ui.horizontal(|ui| {
                if ui.button("Выдать разрешение").clicked() {
                    permissions::prompt_input_monitoring();
                }
                if ui.button("Открыть настройки").clicked() {
                    permissions::open_input_monitoring_settings();
                }
            });
            ui.weak("После выдачи приложение нужно перезапустить.");
            ui.add_space(10.0);
        }

        // Без сети облачные действия отказывают сразу, а не по таймауту —
        // но об этом надо сказать, иначе выглядит как поломка.
        if !crate::net::is_online() {
            ui.colored_label(
                egui::Color32::from_rgb(220, 130, 40),
                "Сети нет — облачные модели недоступны",
            );
            let local_ok = self.cfg.stt.fallback.map(|f| f.is_local()).unwrap_or(false)
                || self.cfg.stt.engine.is_local();
            ui.label(if local_ok {
                "Диктовка работает через локальную модель. Перевод и другие действия \
                 через облако будут отказывать сразу, без ожидания."
            } else {
                "Запасной локальный движок не выбран — диктовка тоже не сработает. \
                 Настройте его в разделе «Распознавание речи»."
            });
            ui.add_space(8.0);
        }

        let missing = engine::missing_permissions();
        if !missing.is_empty() {
            ui.colored_label(
                egui::Color32::from_rgb(220, 130, 40),
                format!("Не выданы разрешения: {}", missing.join(", ")),
            );
            if ui.button("Открыть вкладку «Права»").clicked() {
                self.tab = Tab::Permissions;
            }
            ui.add_space(6.0);
        } else if !self.shared.tap_running.load(Ordering::Relaxed) {
            ui.colored_label(
                egui::Color32::from_rgb(220, 130, 40),
                "Слежение за клавишей не запущено — перезапустите приложение.",
            );
            ui.add_space(6.0);
        } else if self.shared.hotkey_state.is_disabled_by_system() {
            ui.colored_label(
                egui::Color32::from_rgb(220, 130, 40),
                "Система отключила слежение за клавишей — перезапустите приложение.",
            );
            ui.add_space(6.0);
        }

        if self.api_key_input.trim().is_empty() && self.shared.key_loaded() {
            ui.colored_label(
                egui::Color32::from_rgb(220, 130, 40),
                "Не задан API-ключ — распознавание работать не будет.",
            );
            if ui.button("Задать ключ").clicked() {
                self.tab = Tab::Settings;
            }
            ui.add_space(6.0);
        }

        if let Some(err) = self.shared.last_error.lock().unwrap().clone() {
            ui.colored_label(egui::Color32::from_rgb(220, 80, 80), err);
            ui.add_space(6.0);
        }

        let last = self.shared.last_text.lock().unwrap().clone();
        if let Some(text) = last {
            ui.label(egui::RichText::new("Последняя вставка").strong());
            ui.add_space(2.0);
            egui::ScrollArea::vertical()
                .max_height(140.0)
                .id_salt("last_text")
                .show(ui, |ui| {
                    ui.label(&text);
                });
            ui.add_space(4.0);
            if ui.button("Скопировать").clicked() {
                let _ = insert::write_clipboard(&text);
                self.toast("Скопировано");
            }
        }

        ui.add_space(10.0);
        if let Some(dev) = crate::audio::input_device_name() {
            ui.weak(format!("Микрофон: {dev}"));
        }
    }

    fn ui_settings(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.add_space(6.0);
            ui.label(egui::RichText::new("Доступ к API").strong());
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label("Ключ:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.api_key_input)
                        .password(true)
                        .desired_width(300.0)
                        .hint_text("gsk_..."),
                );
            });
            ui.horizontal(|ui| {
                if ui.button("Сохранить").clicked() {
                    self.save_api_key();
                }
                if ui.button("Проверить ключ").clicked() {
                    self.start_model_check();
                }
            });
            ui.checkbox(
                &mut self.cfg.general.key_in_config,
                "Хранить ключ в файле настроек, а не в Keychain",
            );
            if let Some((msg, ok)) = &self.check_message {
                let color = if *ok {
                    egui::Color32::from_rgb(60, 160, 90)
                } else {
                    egui::Color32::from_rgb(220, 80, 80)
                };
                ui.colored_label(color, msg);
            }
            if self.cfg.general.key_in_config {
                ui.colored_label(
                    egui::Color32::from_rgb(220, 130, 40),
                    "Ключ лежит в обычном файле — это менее надёжно, чем связка ключей.",
                );
            } else {
                ui.weak(
                    "Ключ хранится в системной связке ключей. Если macOS спрашивает пароль \
                     при каждом запуске — нажмите в её окне «Всегда разрешать».",
                );
            }

            ui.add_space(12.0);
            ui.separator();
            ui.add_space(6.0);
            ui.label(egui::RichText::new("Горячая клавиша").strong());
            ui.add_space(4.0);

            if self.capturing {
                let shown = if self.capture_preview.is_empty() {
                    "нажмите клавиши…".to_string()
                } else {
                    format!(
                        "{}  ({})",
                        Binding::new(self.capture_preview.clone()).label(),
                        self.capture_preview.len()
                    )
                };
                ui.horizontal(|ui| {
                    ui.add_sized(
                        [220.0, 24.0],
                        egui::Label::new(
                            egui::RichText::new(shown)
                                .color(egui::Color32::from_rgb(90, 140, 240))
                                .strong(),
                        ),
                    );
                    let can_save = !self.capture_preview.is_empty();
                    if ui
                        .add_enabled(can_save, egui::Button::new("Сохранить"))
                        .clicked()
                    {
                        self.cfg.general.hotkey = Binding::new(self.capture_preview.clone());
                        self.set_capturing(false);
                    }
                    if ui.button("Отмена").clicked() {
                        self.set_capturing(false);
                    }
                });
                ui.weak(
                    "Зажмите нужное сочетание целиком — до трёх клавиш. \
                     Записывается самый полный набор, который вы удерживали.",
                );
                if !permissions::input_monitoring().is_ok() {
                    ui.colored_label(
                        egui::Color32::from_rgb(220, 80, 80),
                        "Буквы и цифры сейчас не поймаются — нет «Мониторинга ввода»",
                    );
                }
            } else {
                ui.horizontal(|ui| {
                    ui.add_sized(
                        [220.0, 24.0],
                        egui::Label::new(
                            egui::RichText::new(self.cfg.general.hotkey.label()).strong(),
                        ),
                    );
                    if ui.button("Задать сочетание").clicked() {
                        self.set_capturing(true);
                    }
                });

                match &self.verdict {
                    conflicts::Verdict::Free => {
                        ui.colored_label(
                            egui::Color32::from_rgb(60, 160, 90),
                            "Сочетание свободно",
                        );
                    }
                    conflicts::Verdict::Taken(why) => {
                        ui.colored_label(
                            egui::Color32::from_rgb(220, 80, 80),
                            format!("Занято — {why}"),
                        );
                    }
                    conflicts::Verdict::Risky(why) => {
                        ui.colored_label(
                            egui::Color32::from_rgb(220, 130, 40),
                            format!("Осторожно — {why}"),
                        );
                    }
                }
            }

            ui.add_space(8.0);
            ui.label("Как срабатывает:");
            ui.radio_value(
                &mut self.cfg.general.hotkey_mode,
                HotKeyMode::Hold,
                "Push to Talk — держите клавишу, пока говорите",
            );
            ui.radio_value(
                &mut self.cfg.general.hotkey_mode,
                HotKeyMode::Toggle,
                "Переключатель — нажали, говорите, нажали ещё раз",
            );
            ui.add_space(4.0);
            ui.checkbox(
                &mut self.cfg.general.swallow_hotkey,
                "Не пропускать клавишу дальше — убирает системный писк",
            );
            if self.cfg.general.swallow_hotkey {
                ui.weak(
                    "Обычная клавиша сочетания не уходит дальше в систему, поэтому программа \
                     не получает незнакомое сочетание и не пищит. Модификаторы пропускаются \
                     всегда — иначе сломались бы обычные ⌘C и ⌘V.",
                );
            } else {
                ui.weak(
                    "Сочетание уходит дальше в программу. Если она его не знает, macOS \
                     отвечает системным писком.",
                );
            }

            ui.add_space(12.0);
            ui.separator();
            ui.add_space(6.0);
            ui.label(egui::RichText::new("История буфера обмена").strong());
            ui.add_space(4.0);
            ui.checkbox(
                &mut self.cfg.general.clipboard_history,
                "Запоминать всё, что попадает в буфер",
            );
            self.shared
                .clipboard
                .set_enabled(self.cfg.general.clipboard_history);

            let capturing_clip = self.capturing_clipboard;
            ui.horizontal(|ui| {
                ui.add_sized([130.0, 20.0], egui::Label::new("Сочетание"));
                if capturing_clip {
                    let shown = if self.capture_preview.is_empty() {
                        "нажмите клавиши…".to_string()
                    } else {
                        Binding::new(self.capture_preview.clone()).label()
                    };
                    ui.colored_label(egui::Color32::from_rgb(90, 140, 240), shown);
                    if ui
                        .add_enabled(!self.capture_preview.is_empty(), egui::Button::new("ОК"))
                        .clicked()
                    {
                        self.cfg.general.clipboard_hotkey =
                            Binding::new(self.capture_preview.clone());
                        self.set_capturing(false);
                        self.capturing_clipboard = false;
                    }
                    if ui.button("Отмена").clicked() {
                        self.set_capturing(false);
                        self.capturing_clipboard = false;
                    }
                } else {
                    ui.label(self.cfg.general.clipboard_hotkey.label());
                    if ui.button("Задать").clicked() {
                        self.set_capturing(true);
                        self.capturing_clipboard = true;
                    }
                    if !self.cfg.general.clipboard_hotkey.is_empty()
                        && ui.button("Убрать").clicked()
                    {
                        self.cfg.general.clipboard_hotkey = Binding::new(Vec::new());
                    }
                }
            });
            ui.add_space(4.0);
            ui.label("Что делает это сочетание:");
            for m in ClipboardMode::ALL {
                ui.radio_value(&mut self.cfg.general.clipboard_mode, m, m.label());
            }
            ui.weak(self.cfg.general.clipboard_mode.hint());
            if self.cfg.general.clipboard_mode == ClipboardMode::Cycle
                && !self.cfg.general.show_overlay
            {
                ui.colored_label(
                    egui::Color32::from_rgb(220, 130, 40),
                    "Плашка у курсора выключена — листать придётся вслепую. \
                     Включите её ниже, в «Поведении».",
                );
            }
            ui.add_space(4.0);

            labeled(ui, "Показывать последние", |ui| {
                ui.add(egui::DragValue::new(&mut self.cfg.general.clipboard_recent).range(1..=100));
                ui.weak(match self.cfg.general.clipboard_mode {
                    ClipboardMode::List => "записей в списке; поиск идёт по всей истории",
                    ClipboardMode::Cycle => "записей в круге листания",
                });
            });

            labeled(ui, "Хранить дней", |ui| {
                ui.add(egui::DragValue::new(&mut self.cfg.general.clipboard_days).range(0..=365));
                ui.weak(if self.cfg.general.clipboard_days == 0 {
                    "не выбрасывать"
                } else {
                    "старые записи удаляются сами"
                });
            });

            ui.add_space(12.0);
            ui.separator();
            ui.add_space(6.0);
            ui.label(egui::RichText::new("Распознавание речи").strong());
            ui.add_space(4.0);

            ui.label("Движок по умолчанию:");
            for e in Engine::ALL {
                ui.radio_value(&mut self.cfg.stt.engine, e, e.label());
            }

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label("Если откажет:");
                egui::ComboBox::from_id_salt("fallback")
                    .width(200.0)
                    .selected_text(match self.cfg.stt.fallback {
                        None => "ничего, показать ошибку".to_string(),
                        Some(e) => format!("перейти на {}", e.label()),
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.cfg.stt.fallback,
                            None,
                            "ничего, показать ошибку",
                        );
                        for e in Engine::ALL {
                            if e == self.cfg.stt.engine {
                                continue;
                            }
                            ui.selectable_value(
                                &mut self.cfg.stt.fallback,
                                Some(e),
                                format!("перейти на {}", e.label()),
                            );
                        }
                    });
            });
            ui.weak(
                "Откат срабатывает молча: пропала сеть, кончился лимит, протух ключ — \
                 диктовка всё равно доводится до конца.",
            );

            ui.add_space(10.0);
            labeled(ui, "Язык речи", |ui| {
                egui::ComboBox::from_id_salt("stt_lang")
                    .width(220.0)
                    .selected_text(crate::config::language_name(&self.cfg.stt.language))
                    .show_ui(ui, |ui| {
                        for (code, name) in crate::config::LANGUAGES {
                            ui.selectable_value(
                                &mut self.cfg.stt.language,
                                code.to_string(),
                                *name,
                            );
                        }
                    });
            });
            if self.cfg.stt.engine == Engine::Parakeet {
                ui.weak(
                    "Parakeet определяет язык сам и выбор здесь не учитывает — \
                     он влияет только на облачное распознавание.",
                );
            } else if self.cfg.stt.language == "auto" {
                ui.weak(
                    "Автоопределение стоит лишнего прохода по записи и иногда ошибается \
                     на коротких фразах. Если язык всегда один, укажите его явно.",
                );
            }

            // --- облако ---
            ui.add_space(10.0);
            ui.label(egui::RichText::new("Облако").weak());
            labeled(ui, "Адрес API", |ui| {
                ui.add(egui::TextEdit::singleline(&mut self.cfg.stt.base_url).desired_width(320.0));
            });
            labeled(ui, "Модель", |ui| {
                model_picker(
                    ui,
                    "stt_model",
                    &mut self.cfg.stt.model,
                    &self.models,
                    "whisper",
                );
            });
            labeled(ui, "Подсказка", |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.cfg.stt.prompt)
                        .desired_width(320.0)
                        .hint_text("имена и термины, которые часто путает"),
                );
            });

            // --- локальные модели ---
            ui.add_space(12.0);
            ui.label(egui::RichText::new("Локальные модели").weak());
            ui.add_space(4.0);
            self.ui_local_models(ui);
            ui.add_space(4.0);
            ui.checkbox(
                &mut self.cfg.stt.preload_local,
                "Загружать локальную модель при запуске",
            );
            ui.weak(
                "Загрузка занимает несколько секунд. Без неё первая диктовка после \
                 запуска будет заметно дольше остальных.",
            );

            // --- локальная обработка текста ---
            ui.add_space(12.0);
            ui.separator();
            ui.add_space(6.0);
            ui.label(egui::RichText::new("Локальная обработка текста").strong());
            ui.weak(
                "Правит надиктованное без сети. Включается сама, когда облако \
                 не ответило, — и только тогда, чтобы не занимать память зря.",
            );
            ui.add_space(6.0);
            self.ui_llm_models(ui);
            ui.add_space(4.0);
            ui.checkbox(
                &mut self.cfg.local_llm.keep_loaded,
                "Держать модель в памяти постоянно",
            );
            ui.weak(
                "Быстрее отвечает, но несколько гигабайт заняты всё время. \
                 Без этого модель грузится при первой надобности.",
            );
            ui.add_space(6.0);
            labeled(ui, "Выгружать через", |ui| {
                ui.add(
                    egui::DragValue::new(&mut self.cfg.general.idle_unload_min)
                        .range(0..=120)
                        .suffix(" мин"),
                );
                ui.weak("простоя, 0 — не выгружать");
            });
            ui.weak(
                "Касается и распознавания, и языковой модели: обе освобождают \
                 память, пока ими не пользуются.",
            );

            // --- свой эндпоинт ---
            ui.add_space(12.0);
            ui.separator();
            ui.add_space(6.0);
            ui.label(egui::RichText::new("Отдавать модель другим программам").strong());
            ui.weak(
                "Поднимает OpenAI-совместимый адрес поверх той же локальной модели. \
                 Второй копии в памяти не появится.",
            );
            ui.add_space(6.0);
            ui.checkbox(&mut self.cfg.server.enabled, "Включить локальный эндпоинт");
            if self.cfg.server.enabled {
                if self.cfg.server.api_key.trim().is_empty() {
                    self.cfg.server.api_key = crate::server::new_key();
                }
                labeled(ui, "Порт", |ui| {
                    ui.add(egui::DragValue::new(&mut self.cfg.server.port).range(1024..=65535));
                    ui.weak(format!("http://127.0.0.1:{}/v1", self.cfg.server.port));
                });
                labeled(ui, "Ключ", |ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.cfg.server.api_key)
                            .desired_width(300.0)
                            .password(!self.show_server_key),
                    );
                    if ui
                        .small_button(if self.show_server_key {
                            "Скрыть"
                        } else {
                            "Показать"
                        })
                        .clicked()
                    {
                        self.show_server_key = !self.show_server_key;
                    }
                    if ui.small_button("Копировать").clicked() {
                        ui.ctx().copy_text(self.cfg.server.api_key.clone());
                        self.toast("Ключ скопирован");
                    }
                    if ui.small_button("Новый").clicked() {
                        self.cfg.server.api_key = crate::server::new_key();
                        self.toast("Ключ заменён — обновите его в других программах");
                    }
                });
                ui.colored_label(
                    egui::Color32::from_rgb(220, 130, 40),
                    "Адрес и порт применяются после перезапуска приложения",
                );
                ui.weak(
                    "Слушает только 127.0.0.1 — из сети не доступен. Пока идёт диктовка, \
                     внешние запросы получают отказ: программа в первую очередь ваша. \
                     Чтобы не ждать загрузку модели на каждом холодном запросе, включите \
                     «Держать модель в памяти постоянно».",
                );
            }

            ui.add_space(12.0);
            ui.separator();
            ui.add_space(6.0);
            ui.label(egui::RichText::new("Обработка после диктовки").strong());
            ui.add_space(4.0);
            let after: Vec<String> = self
                .cfg
                .actions
                .iter()
                .filter(|a| a.enabled && a.after_dictation)
                .map(|a| a.name.clone())
                .collect();
            if after.is_empty() {
                ui.weak("Надиктованное вставляется как есть.");
            } else {
                ui.label(format!("Применяется по порядку: {}", after.join(" → ")));
            }
            ui.weak(
                "Настраивается на вкладке «Действия»: у каждого промпта есть галочка \
                 «Использовать после диктовки».",
            );
            if ui.button("Открыть «Действия»").clicked() {
                self.tab = Tab::Actions;
            }

            ui.add_space(12.0);
            ui.separator();
            ui.add_space(6.0);
            ui.label(egui::RichText::new("Поведение").strong());
            ui.add_space(4.0);
            ui.checkbox(
                &mut self.cfg.general.show_in_dock,
                "Показывать иконку в доке",
            );
            ui.checkbox(
                &mut self.cfg.general.play_sounds,
                "Звуки начала и конца записи",
            );
            ui.checkbox(
                &mut self.cfg.general.restore_clipboard,
                "Возвращать прежнее содержимое буфера обмена",
            );
            ui.checkbox(
                &mut self.cfg.general.show_overlay,
                "Плашка у курсора: что сейчас происходит",
            );
            if ui
                .checkbox(&mut self.autostart_on, "Запускать при входе в систему")
                .changed()
            {
                match autostart::set(self.autostart_on) {
                    Ok(()) => self.toast(if self.autostart_on {
                        "Автозапуск включён"
                    } else {
                        "Автозапуск выключен"
                    }),
                    Err(e) => {
                        self.autostart_on = autostart::is_enabled();
                        self.toast(format!("Автозапуск: {e}"));
                    }
                }
            }
            labeled(ui, "Хранить записей", |ui| {
                ui.add(egui::DragValue::new(&mut self.cfg.general.history_limit).range(10..=5000));
            });

            ui.add_space(8.0);
            ui.label(egui::RichText::new("Пределы записи").weak());
            labeled(ui, "Максимум диктовки", |ui| {
                ui.add(
                    egui::DragValue::new(&mut self.cfg.general.max_recording_secs)
                        .range(5..=1800)
                        .suffix(" с"),
                );
                ui.weak("запись оборвётся сама, даже если клавишу зажали и забыли");
            });
            labeled(ui, "Обрыв по тишине", |ui| {
                ui.add(
                    egui::DragValue::new(&mut self.cfg.general.silence_stop_secs)
                        .range(0..=60)
                        .suffix(" с"),
                );
                ui.weak(if self.cfg.general.silence_stop_secs == 0 {
                    "выключено"
                } else {
                    "столько тишины подряд — и запись останавливается"
                });
            });

            ui.add_space(12.0);
            ui.separator();
            ui.add_space(6.0);
            ui.label(egui::RichText::new("Обновление").strong());
            ui.add_space(4.0);
            ui.weak(format!("Текущая версия: {}", updater::current_version()));
            ui.checkbox(
                &mut self.cfg.general.check_updates,
                "Проверять обновления при запуске",
            );
            ui.add_space(4.0);
            self.ui_update_block(ui);

            ui.add_space(12.0);
            ui.separator();
            ui.add_space(6.0);
            ui.label(egui::RichText::new("Диагностика").strong());
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.button("Открыть журнал").clicked() {
                    let _ = open::that(crate::logging::log_path());
                }
                if ui.button("Показать папку настроек").clicked() {
                    let _ = open::that(crate::config::config_dir());
                }
                if ui.button("Проверить плашку").clicked() {
                    self.shared.notify("Так выглядит плашка у курсора");
                }
            });
            ui.add_space(8.0);
            ui.weak(format!(
                "Настройки: {}",
                crate::config::config_path().display()
            ));
            ui.weak(format!("Журнал: {}", crate::logging::log_path().display()));
            ui.add_space(12.0);
        });
    }

    /// Список локальных моделей: что скачано, что качается, что можно удалить.
    fn ui_local_models(&mut self, ui: &mut egui::Ui) {
        self.ui_model_list(ui, models::CATALOG);
    }

    /// Языковые модели для локальной обработки текста.
    fn ui_llm_models(&mut self, ui: &mut egui::Ui) {
        self.ui_model_list(ui, models::LLM_CATALOG);
    }

    fn ui_model_list(&mut self, ui: &mut egui::Ui, catalog: &'static [models::ModelSpec]) {
        let mut to_download: Option<&'static models::ModelSpec> = None;
        let mut to_remove: Option<&'static models::ModelSpec> = None;
        let mut cancel = false;

        for spec in catalog.iter() {
            let installed = spec.is_installed();
            let downloading = self
                .download
                .as_ref()
                .is_some_and(|d| d.model_id == spec.id);

            // Радиокнопка выбирает модель внутри своего движка, а не движок.
            let selected = match spec.engine {
                Engine::Parakeet => self.cfg.stt.parakeet_model == spec.id,
                Engine::Llm => self.cfg.local_llm.model == spec.id,
                Engine::Cloud => false,
            };

            ui.push_id(spec.id, |ui| {
                ui.horizontal(|ui| {
                    if ui.radio(selected, spec.title).clicked() {
                        match spec.engine {
                            Engine::Parakeet => self.cfg.stt.parakeet_model = spec.id.to_string(),
                            Engine::Llm => self.cfg.local_llm.model = spec.id.to_string(),
                            Engine::Cloud => {}
                        }
                    }
                    ui.weak(models::human_size(spec.total_size()));
                    if installed {
                        ui.colored_label(egui::Color32::from_rgb(60, 160, 90), "скачана");
                    }
                });
                ui.weak(spec.note);

                if downloading {
                    let p = self.download.as_ref().map(|d| d.progress.clone());
                    if let Some(p) = p {
                        let done = p.downloaded.load(Ordering::Relaxed);
                        let total = p.total.load(Ordering::Relaxed);
                        ui.add(
                            egui::ProgressBar::new(p.fraction())
                                .desired_width(320.0)
                                .text(format!(
                                    "{} из {}",
                                    models::human_size(done),
                                    models::human_size(total)
                                )),
                        );
                        if ui.small_button("Отменить").clicked() {
                            p.cancel();
                            cancel = true;
                        }
                    }
                } else {
                    ui.horizontal(|ui| {
                        if !installed && ui.small_button("Скачать").clicked() {
                            to_download = Some(spec);
                        }
                        if installed && ui.small_button("Удалить").clicked() {
                            to_remove = Some(spec);
                        }
                    });
                }
                ui.add_space(6.0);
            });
        }

        if let Some(spec) = to_download {
            if self.download.is_some() {
                self.toast("Дождитесь окончания текущей загрузки");
            } else {
                self.start_download(spec);
            }
        }
        if let Some(spec) = to_remove {
            match spec.remove() {
                Ok(()) => self.toast(format!("Модель {} удалена", spec.title)),
                Err(e) => self.toast(format!("Не удалить: {e}")),
            }
        }
        if cancel {
            self.toast("Загрузка отменена");
        }

        ui.weak(format!("Папка моделей: {}", models::models_dir().display()));
    }

    fn poll_model_fetch(&mut self) {
        let Some((provider, kind, rx)) = &self.model_fetch else {
            return;
        };
        let provider = *provider;
        let kind = *kind;
        match rx.try_recv() {
            Ok(Ok(models)) => {
                if kind == FetchKind::KeyCheck {
                    self.toast(format!("Ключ {} принят", provider.label()));
                } else {
                    let n = models.len();
                    self.provider_models.insert(provider, models);
                    self.toast(format!("{}: моделей {n}", provider.label()));
                }
                self.model_fetch = None;
            }
            Ok(Err(e)) => {
                self.toast(format!("{}: {e}", provider.label()));
                self.model_fetch = None;
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => self.model_fetch = None,
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
    }

    fn fetch_models(&mut self, endpoint: &Endpoint) {
        let provider = endpoint.provider;
        let base_url = endpoint.base_url();
        let key = self
            .provider_keys
            .get(&provider)
            .cloned()
            .unwrap_or_else(|| endpoint.api_key(&self.cfg));
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(providers::list_models(&base_url, &key).map_err(|e| e.to_string()));
        });
        self.model_fetch = Some((provider, FetchKind::Models, rx));
        self.toast(format!("Читаю модели с {}…", provider.label()));
    }

    /// Ключ поставщика: поле, кнопка сохранения и ссылка, где его взять.
    fn ui_provider_key(&mut self, ui: &mut egui::Ui, provider: Provider) {
        if !provider.needs_key() {
            ui.weak("Ollama работает локально, ключ не нужен.");
            return;
        }
        let stored = self.cfg.key_for(provider.key_account());
        let entry = self.provider_keys.entry(provider).or_insert(stored);
        let mut value = entry.clone();
        let hint = if value.is_empty() {
            "не задан"
        } else {
            ""
        };

        ui.horizontal(|ui| {
            ui.label("Ключ:");
            ui.add(
                egui::TextEdit::singleline(&mut value)
                    .password(true)
                    .desired_width(260.0)
                    .hint_text(hint),
            );
        });
        self.provider_keys.insert(provider, value.clone());

        // Видно, лежит ли ключ на самом деле: набранное в поле ещё не сохранено.
        let stored_now = self.cfg.key_for(provider.key_account());
        if stored_now.is_empty() {
            ui.colored_label(
                egui::Color32::from_rgb(220, 130, 40),
                "Ключ не сохранён — наберите его и нажмите «Сохранить ключ»",
            );
        } else if stored_now != value.trim() {
            ui.colored_label(
                egui::Color32::from_rgb(220, 130, 40),
                "В поле не то, что сохранено — нажмите «Сохранить ключ»",
            );
        }

        ui.horizontal(|ui| {
            if ui.button("Сохранить ключ").clicked() {
                let account = provider.key_account();
                match self.cfg.set_key_for(account, value.trim()) {
                    Ok(()) => self.toast(format!("Ключ {} сохранён", provider.label())),
                    Err(e) => self.toast(format!("Не сохранить ключ: {e}")),
                }
            }
            if ui.button("Проверить ключ").clicked() {
                self.verify_key_now(provider);
            }
            if let Some(url) = provider.key_url() {
                if ui.button("Где взять").clicked() {
                    let _ = open::that(url);
                }
            }
        });
    }

    /// Проверяет ключ настоящим запросом к модели, а не чтением списка.
    fn verify_key_now(&mut self, provider: Provider) {
        let Some(action) = self
            .cfg
            .actions
            .iter()
            .find(|a| a.endpoint.provider == provider)
            .cloned()
        else {
            self.toast("Сначала выберите модель для этого поставщика");
            return;
        };
        let key = self.cfg.key_for(provider.key_account());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(
                providers::verify_key(&action.endpoint, &key)
                    .map(|()| Vec::new())
                    .map_err(|e| e.to_string()),
            );
        });
        self.model_fetch = Some((provider, FetchKind::KeyCheck, rx));
        self.toast(format!("Проверяю ключ {}…", provider.label()));
    }

    /// Открывает список истории буфера поверх экрана.
    ///
    /// Отдельного окна под него нет: главное всё равно скрыто, и проще снять
    /// с него рамку и поднять наверх, чем заводить второй viewport со своей
    /// поверхностью отрисовки.
    ///
    /// Запоминает, кто был впереди: после выбора фокус надо вернуть туда,
    /// иначе вставка уйдёт в наше же окно.
    fn open_picker(&mut self, ctx: &egui::Context) {
        self.picker_return_pid = macos::frontmost_app_pid();
        self.picker = true;
        self.picker_query.clear();
        self.picker_index = 0;
        self.picker_opened = Instant::now();

        let size = self.picker_size();
        ctx.send_viewport_cmd(egui::ViewportCommand::Decorations(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(
            egui::WindowLevel::AlwaysOnTop,
        ));
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(self.picker_pos(size)));
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        macos::activate();
    }

    /// Высота — по числу показываемых записей: список на десять строк не
    /// должен занимать пол-экрана, а на полсотни — ползать в щели.
    fn picker_size(&self) -> egui::Vec2 {
        let rows = self.cfg.general.clipboard_recent.clamp(1, 100) as f32;
        egui::vec2(560.0, (120.0 + rows * 24.0).clamp(200.0, 560.0))
    }

    /// Ставим список под курсором, но так, чтобы он целиком влез в экран.
    fn picker_pos(&self, size: egui::Vec2) -> egui::Pos2 {
        let (sw, sh) = macos::screen_size().unwrap_or((1440.0, 900.0));
        let (cx, cy) = macos::cursor_position().unwrap_or((sw / 2.0, sh / 3.0));
        let (x, y) = (cx - size.x / 2.0, cy + 24.0);
        // Границы известны только для главного экрана. Если курсор не на нём,
        // подгонка увела бы список на соседний монитор — там ставим как есть.
        if (0.0..sw).contains(&cx) && (0.0..sh).contains(&cy) {
            return egui::pos2(
                x.clamp(12.0, (sw - size.x - 12.0).max(12.0)),
                y.clamp(12.0, (sh - size.y - 12.0).max(12.0)),
            );
        }
        egui::pos2(x, y)
    }

    fn close_picker(&mut self, ctx: &egui::Context) {
        self.picker = false;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        // Обычный вид возвращаем сразу, пока окно скрыто: иначе настройки,
        // открытые из трея, показались бы плашкой без рамки поверх всего.
        ctx.send_viewport_cmd(egui::ViewportCommand::Decorations(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(
            egui::WindowLevel::Normal,
        ));
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(self.main_size));
        if let Some(pid) = self.picker_return_pid.take() {
            macos::activate_app(pid);
        }
    }

    /// Кладёт выбранное в буфер и вставляет туда, откуда пришёл пользователь.
    /// Буфер после вставки не восстанавливается: запись должна остаться в нём,
    /// чтобы её можно было вставить ещё раз руками.
    fn paste_from_picker(&mut self, ctx: &egui::Context, text: String) {
        self.close_picker(ctx);
        let clipboard = self.shared.clipboard.clone();
        std::thread::spawn(move || {
            // Ждём, пока система вернёт фокус прежней программе: вставка
            // раньше этого уйдёт в никуда.
            std::thread::sleep(Duration::from_millis(220));
            clipboard.mark_ours(&text);
            if let Err(e) = insert::insert_restoring(&text, None) {
                log::warn!("вставка из истории буфера: {e}");
            }
        });
    }

    /// Только в буфер, без вставки: бывает нужно унести кусок в программу,
    /// куда ⌘V не проходит.
    fn copy_from_picker(&mut self, ctx: &egui::Context, text: String) {
        self.close_picker(ctx);
        self.shared.clipboard.mark_ours(&text);
        let _ = insert::write_clipboard(&text);
    }

    /// Плашка списка: полупрозрачная, со скруглением, без рамки окна.
    fn ui_picker_window(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        // Клик мимо списка должен его убирать. Первые полсекунды потерю
        // фокуса не считаем: он приходит не в том же кадре, в котором окно
        // показали, и список закрывался бы сразу после открытия.
        let focused = ctx.input(|i| i.viewport().focused.unwrap_or(true));
        if !focused && self.picker_opened.elapsed() > Duration::from_millis(600) {
            self.close_picker(&ctx);
            return;
        }

        let frame = egui::Frame::new()
            .fill(egui::Color32::from_rgba_unmultiplied(20, 20, 24, 234))
            .stroke(egui::Stroke::new(
                1.0,
                egui::Color32::from_rgba_unmultiplied(255, 255, 255, 38),
            ))
            .corner_radius(egui::CornerRadius::same(14))
            .inner_margin(egui::Margin::same(12));
        egui::CentralPanel::default()
            .frame(frame)
            .show(ui, |ui| self.ui_picker(ui));
    }

    fn ui_picker(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();

        // Плашка тёмная всегда, а тема приложения может быть светлой —
        // цвета текста и полей задаём явно, иначе на тёмном будет тёмное.
        {
            let v = &mut ui.style_mut().visuals;
            v.override_text_color = Some(egui::Color32::from_rgb(232, 232, 238));
            v.extreme_bg_color = egui::Color32::from_rgba_unmultiplied(255, 255, 255, 18);
            v.selection.bg_fill = egui::Color32::from_rgb(58, 104, 190);
            v.widgets.hovered.weak_bg_fill =
                egui::Color32::from_rgba_unmultiplied(255, 255, 255, 22);
        }

        let recent = self.cfg.general.clipboard_recent.clamp(1, 100);

        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Буфер обмена").strong());
            ui.label(
                egui::RichText::new("↑ ↓ — выбор,  Enter — вставить,  ⇧Enter — в буфер,  Esc")
                    .size(11.0)
                    .color(egui::Color32::from_rgb(150, 150, 162)),
            );
        });
        ui.add_space(6.0);

        let search = ui.add(
            egui::TextEdit::singleline(&mut self.picker_query)
                .desired_width(f32::INFINITY)
                .hint_text("поиск по всей истории"),
        );
        // Фокус в поле поиска: окно открылось по горячей клавише, и печатать
        // пользователь начнёт сразу.
        if !search.has_focus() {
            search.request_focus();
        }

        let entries = self.shared.clipboard.entries.lock().unwrap().clone();
        let query = self.picker_query.trim().to_lowercase();
        // Без запроса — ровно столько последних записей, сколько попросили в
        // настройках. С запросом ищем по всей истории: иначе поиск отвечал бы
        // «ничего нет» о том, что лежит парой строк ниже предела.
        let found: Vec<&crate::clipboard::Entry> = if query.is_empty() {
            entries.iter().take(recent).collect()
        } else {
            entries
                .iter()
                .filter(|e| e.text.to_lowercase().contains(&query))
                .take(200)
                .collect()
        };

        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.close_picker(&ctx);
            return;
        }
        if ctx.input(|i| i.key_pressed(egui::Key::ArrowDown)) && !found.is_empty() {
            self.picker_index = (self.picker_index + 1).min(found.len() - 1);
        }
        if ctx.input(|i| i.key_pressed(egui::Key::ArrowUp)) {
            self.picker_index = self.picker_index.saturating_sub(1);
        }
        if self.picker_index >= found.len() {
            self.picker_index = found.len().saturating_sub(1);
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
            if let Some(entry) = found.get(self.picker_index) {
                let text = entry.text.clone();
                if ctx.input(|i| i.modifiers.shift) {
                    self.copy_from_picker(&ctx, text);
                } else {
                    self.paste_from_picker(&ctx, text);
                }
                return;
            }
        }

        ui.add_space(6.0);
        if found.is_empty() {
            ui.label(if entries.is_empty() {
                "История пуста. Скопируйте что-нибудь, и оно появится здесь."
            } else {
                "Ничего не нашлось."
            });
            return;
        }

        let mut chosen: Option<String> = None;
        egui::ScrollArea::vertical().show(ui, |ui| {
            for (i, entry) in found.iter().enumerate() {
                let selected = i == self.picker_index;
                // Номер строки помогает попасть глазом: в списке из десяти
                // одинаковых на вид обрезков время не отличает одно от другого.
                let label = format!(
                    "{:>2}.  {}   {}",
                    i + 1,
                    entry.at.format("%d.%m %H:%M"),
                    entry.preview(84)
                );
                let response = ui.selectable_label(selected, label);
                if response.clicked() {
                    chosen = Some(entry.text.clone());
                }
                if selected {
                    response.scroll_to_me(None);
                    if let Some(source) = &entry.source {
                        ui.label(
                            egui::RichText::new(format!("       из {source}"))
                                .size(11.0)
                                .color(egui::Color32::from_rgb(150, 150, 162)),
                        );
                    }
                }
            }
        });

        if let Some(text) = chosen {
            self.paste_from_picker(&ctx, text);
        }
    }

    fn ui_actions(&mut self, ui: &mut egui::Ui) {
        self.poll_model_fetch();

        ui.add_space(6.0);
        ui.label(egui::RichText::new("Действия над выделенным текстом").strong());
        ui.weak(
            "Выделяете текст, нажимаете сочетание — результат ложится в буфер обмена, \
             и у курсора появляется плашка «в буфере». Вставляете сами, ⌘V.",
        );
        ui.add_space(8.0);

        ui.horizontal(|ui| {
            if ui.button("+  Новое действие").clicked() {
                let action = TextAction::default();
                self.editing_action = Some(action.id.clone());
                self.cfg.actions.push(action);
            }
            if ui.button("+  Ответ по моим данным").clicked() {
                let action = crate::actions::answer_action(Endpoint::default());
                self.editing_action = Some(action.id.clone());
                self.cfg.actions.push(action);
            }
        });
        ui.weak(
            "«Ответ по моим данным» — заготовка: указываете файл со сведениями о себе, \
             выделяете чужое сообщение, жмёте сочетание и получаете готовый ответ на \
             языке этого сообщения.",
        );
        ui.add_space(6.0);

        let ids: Vec<String> = self.cfg.actions.iter().map(|a| a.id.clone()).collect();
        let mut to_remove: Option<String> = None;
        let mut to_move: Option<(usize, isize)> = None;

        egui::ScrollArea::vertical().show(ui, |ui| {
            for (index, id) in ids.iter().enumerate() {
                let Some(pos) = self.cfg.actions.iter().position(|a| &a.id == id) else {
                    continue;
                };
                let expanded = self.editing_action.as_deref() == Some(id.as_str());

                ui.push_id(id, |ui| {
                    ui.horizontal(|ui| {
                        // Галочка без подписи читалась как непонятный квадратик.
                        let mut e = self.cfg.actions[pos].enabled;
                        let response = ui.checkbox(&mut e, "вкл").on_hover_text(
                            "Выключенное действие не срабатывает по своему сочетанию \
                                 и не занимает его",
                        );
                        if response.changed() {
                            self.cfg.actions[pos].enabled = e;
                        }
                        let name = self.cfg.actions[pos].name.clone();
                        let hotkey = self.cfg.actions[pos].hotkey.clone();
                        let after = self.cfg.actions[pos].after_dictation;
                        let mut title = if hotkey.is_empty() {
                            format!("{name}  —  сочетание не задано")
                        } else {
                            format!("{name}  —  {}", hotkey.label())
                        };
                        if after {
                            title.push_str("  ·  после диктовки");
                        }
                        if ui.selectable_label(expanded, title).clicked() {
                            self.editing_action = if expanded { None } else { Some(id.clone()) };
                        }
                        if index > 0 && ui.small_button("↑").clicked() {
                            to_move = Some((pos, -1));
                        }
                        if index + 1 < ids.len() && ui.small_button("↓").clicked() {
                            to_move = Some((pos, 1));
                        }
                        if ui.small_button("Удалить").clicked() {
                            to_remove = Some(id.clone());
                        }
                    });

                    if expanded {
                        ui.add_space(4.0);
                        egui::Frame::group(ui.style()).show(ui, |ui| {
                            self.ui_action_editor(ui, pos);
                        });
                        ui.add_space(6.0);
                    }
                });
            }
        });

        if let Some((pos, delta)) = to_move {
            let target = (pos as isize + delta).clamp(0, self.cfg.actions.len() as isize - 1);
            self.cfg.actions.swap(pos, target as usize);
        }
        if let Some(id) = to_remove {
            self.cfg.actions.retain(|a| a.id != id);
            if self.editing_action.as_deref() == Some(id.as_str()) {
                self.editing_action = None;
            }
            if self.capturing_action.as_deref() == Some(id.as_str()) {
                self.set_capturing(false);
                self.capturing_action = None;
            }
        }
    }

    fn ui_action_editor(&mut self, ui: &mut egui::Ui, pos: usize) {
        let id = self.cfg.actions[pos].id.clone();

        labeled(ui, "Название", |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.cfg.actions[pos].name).desired_width(280.0),
            );
        });

        // --- сочетание клавиш ---
        let capturing = self.capturing_action.as_deref() == Some(id.as_str());
        ui.horizontal(|ui| {
            ui.add_sized([130.0, 20.0], egui::Label::new("Сочетание"));
            if capturing {
                let shown = if self.capture_preview.is_empty() {
                    "нажмите клавиши…".to_string()
                } else {
                    format!(
                        "{}  ({})",
                        Binding::new(self.capture_preview.clone()).label(),
                        self.capture_preview.len()
                    )
                };
                ui.colored_label(egui::Color32::from_rgb(90, 140, 240), shown);
                if ui
                    .add_enabled(!self.capture_preview.is_empty(), egui::Button::new("ОК"))
                    .clicked()
                {
                    self.cfg.actions[pos].hotkey = Binding::new(self.capture_preview.clone());
                    self.set_capturing(false);
                    self.capturing_action = None;
                }
                if ui.button("Отмена").clicked() {
                    self.set_capturing(false);
                    self.capturing_action = None;
                }
            } else {
                ui.label(self.cfg.actions[pos].hotkey.label());
                if ui.button("Задать").clicked() {
                    self.set_capturing(true);
                    self.capturing_action = Some(id.clone());
                }
                if !self.cfg.actions[pos].hotkey.is_empty() && ui.button("Убрать").clicked() {
                    self.cfg.actions[pos].hotkey = Binding::new(Vec::new());
                }
            }
        });

        // Сочетание действия не должно совпадать с диктовкой и с другими действиями.
        let own = self.cfg.actions[pos].hotkey.clone();
        if !own.is_empty() {
            if own == self.cfg.general.hotkey {
                ui.colored_label(
                    egui::Color32::from_rgb(220, 80, 80),
                    "Занято — это сочетание уже стоит на диктовке",
                );
            } else if let Some(other) = self
                .cfg
                .actions
                .iter()
                .enumerate()
                .find(|(i, a)| *i != pos && a.hotkey == own && !a.hotkey.is_empty())
            {
                ui.colored_label(
                    egui::Color32::from_rgb(220, 80, 80),
                    format!("Занято — это сочетание уже у действия «{}»", other.1.name),
                );
            } else {
                match conflicts::check(&own) {
                    conflicts::Verdict::Free => {
                        ui.colored_label(
                            egui::Color32::from_rgb(60, 160, 90),
                            "Сочетание свободно",
                        );
                    }
                    conflicts::Verdict::Taken(why) => {
                        ui.colored_label(
                            egui::Color32::from_rgb(220, 80, 80),
                            format!("Занято — {why}"),
                        );
                    }
                    conflicts::Verdict::Risky(why) => {
                        ui.colored_label(
                            egui::Color32::from_rgb(220, 130, 40),
                            format!("Осторожно — {why}"),
                        );
                    }
                }
            }
        }

        // --- поставщик и модель ---
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.add_sized([130.0, 20.0], egui::Label::new("Поставщик"));
            let mut provider = self.cfg.actions[pos].endpoint.provider;
            egui::ComboBox::from_id_salt(format!("prov-{id}"))
                .width(200.0)
                .selected_text(provider.label())
                .show_ui(ui, |ui| {
                    for p in Provider::ALL {
                        ui.selectable_value(&mut provider, p, p.label());
                    }
                });
            self.cfg.actions[pos].endpoint.set_provider(provider);
        });

        let provider = self.cfg.actions[pos].endpoint.provider;
        if provider == Provider::Custom
            || !self.cfg.actions[pos].endpoint.base_url_override.is_empty()
        {
            labeled(ui, "Адрес API", |ui| {
                ui.add(
                    egui::TextEdit::singleline(
                        &mut self.cfg.actions[pos].endpoint.base_url_override,
                    )
                    .desired_width(320.0)
                    .hint_text(provider.default_base_url()),
                );
            });
        }

        // У локального поставщика поля модели нет: она одна на всё
        // приложение и выбирается в настройках распознавания. Иначе два
        // действия могли бы указать разные модели, и в памяти оказались бы
        // обе — по три гигабайта каждая.
        if provider.is_local() {
            let chosen = crate::models::find(&self.cfg.local_llm.model);
            labeled(ui, "Модель", |ui| match chosen {
                Some(spec) if spec.is_installed() => {
                    ui.label(spec.title);
                }
                Some(spec) => {
                    ui.colored_label(
                        egui::Color32::from_rgb(220, 130, 40),
                        format!("{} — не скачана", spec.title),
                    );
                }
                None => {
                    ui.colored_label(
                        egui::Color32::from_rgb(220, 130, 40),
                        "не выбрана в настройках распознавания",
                    );
                }
            });
            return;
        }

        ui.horizontal(|ui| {
            ui.add_sized([130.0, 20.0], egui::Label::new("Модель"));
            let known = self
                .provider_models
                .get(&provider)
                .cloned()
                .unwrap_or_default();
            if known.is_empty() {
                ui.add(
                    egui::TextEdit::singleline(&mut self.cfg.actions[pos].endpoint.model)
                        .desired_width(240.0),
                );
            } else {
                let current = self.cfg.actions[pos].endpoint.model.clone();
                egui::ComboBox::from_id_salt(format!("model-{id}"))
                    .width(240.0)
                    .selected_text(current)
                    .show_ui(ui, |ui| {
                        for m in &known {
                            ui.selectable_value(
                                &mut self.cfg.actions[pos].endpoint.model,
                                m.clone(),
                                m,
                            );
                        }
                    });
            }
            if ui.button("Считать модели").clicked() {
                let endpoint = self.cfg.actions[pos].endpoint.clone();
                self.fetch_models(&endpoint);
            }
        });
        ui.weak(
            "Список моделей у части поставщиков отдаётся без ключа, поэтому \
             успешное чтение ещё не значит, что ключ рабочий — для этого есть \
             отдельная проверка ниже.",
        );

        ui.add_space(4.0);
        self.ui_provider_key(ui, provider);

        // --- промпт и вывод ---
        ui.add_space(8.0);
        ui.label("Промпт:");
        ui.add(
            egui::TextEdit::multiline(&mut self.cfg.actions[pos].prompt)
                .desired_rows(4)
                .desired_width(f32::INFINITY),
        );
        ui.weak("Выделенный текст приходит модели отдельным сообщением, вставлять его в промпт не нужно.");

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.add_sized([130.0, 20.0], egui::Label::new("Результат"));
            for o in [Output::Clipboard, Output::Replace] {
                ui.selectable_value(&mut self.cfg.actions[pos].output, o, o.label());
            }
        });
        ui.weak(self.cfg.actions[pos].output.hint());

        // --- файл со сведениями ---
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.add_sized([130.0, 20.0], egui::Label::new("Файл с данными"));
            ui.add(
                egui::TextEdit::singleline(&mut self.cfg.actions[pos].context_file)
                    .desired_width(240.0)
                    .hint_text("не задан"),
            );
            if ui.button("Выбрать").clicked() {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("Текст и Markdown", &["md", "markdown", "txt"])
                    .pick_file()
                {
                    self.cfg.actions[pos].context_file = path.to_string_lossy().to_string();
                }
            }
            if !self.cfg.actions[pos].context_file.is_empty() && ui.button("Убрать").clicked()
            {
                self.cfg.actions[pos].context_file.clear();
            }
        });
        if self.cfg.actions[pos].context_file.trim().is_empty()
            && self.cfg.actions[pos].prompt.contains("сведения")
        {
            ui.colored_label(
                egui::Color32::from_rgb(220, 130, 40),
                "Промпт ссылается на сведения, но файл не выбран — отвечать будет не по чему",
            );
        }
        // Обратный случай к проверке выше: файл выбран, а промпт про него
        // молчит. Тогда сведения уходят в каждый запрос впустую, а
        // пользователь считает, что действие ими пользуется, — именно так
        // файл и оказывается прикреплённым не к тому действию.
        if !self.cfg.actions[pos].context_file.trim().is_empty() {
            let prompt = self.cfg.actions[pos].prompt.to_lowercase();
            let mentions = ["сведени", "данны", "файл", "информаци", "контекст"]
                .iter()
                .any(|w| prompt.contains(w));
            if !mentions {
                ui.colored_label(
                    egui::Color32::from_rgb(220, 130, 40),
                    "Файл выбран, но промпт на него не ссылается — сведения уходят \
                     в каждый запрос впустую",
                );
            }
        }
        if !self.cfg.actions[pos].context_file.is_empty() {
            match self.cfg.actions[pos].load_context() {
                Ok(Some(text)) => {
                    ui.colored_label(
                        egui::Color32::from_rgb(60, 160, 90),
                        format!("Файл читается: {} символов", text.chars().count()),
                    );
                }
                Ok(None) => {}
                Err(e) => {
                    ui.colored_label(egui::Color32::from_rgb(220, 80, 80), e.to_string());
                }
            }
            ui.weak(
                "Содержимое уходит в каждый запрос отдельным сообщением. Файл \
                 перечитывается при каждом запуске, так что правки подхватываются \
                 без перезапуска приложения.",
            );
        }

        ui.add_space(6.0);
        ui.checkbox(
            &mut self.cfg.actions[pos].after_dictation,
            "Использовать после диктовки",
        );
        ui.weak(
            "Надиктованный текст пройдёт через этот промпт до вставки. Результат \
             и сочетание клавиш здесь ни при чём — они для ручного запуска.",
        );
        if self.cfg.actions[pos].after_dictation {
            ui.colored_label(
                egui::Color32::from_rgb(220, 130, 40),
                "Время ответа модели добавляется к каждой диктовке",
            );
            ui.weak(
                "Крупная модель здесь обойдётся дорого по ожиданию: разница между \
                 быстрой и большой на короткой фразе — секунды против десятка секунд.",
            );
        }

        ui.add_space(6.0);
        let has_context = !self.cfg.actions[pos].context_file.trim().is_empty();
        ui.checkbox(
            &mut self.cfg.actions[pos].fallback_local,
            "Обработать локальной моделью, если поставщик не ответил",
        );
        if has_context {
            ui.weak(
                "Файл сведений уйдёт и локальной модели: контекст подбирается под \
                 запрос. Если файл окажется слишком велик, действие скажет об этом, \
                 а текст вставится без обработки.",
            );
        }
        if self.cfg.actions[pos].fallback_local {
            if self.cfg.local_llm.model.is_empty() {
                ui.colored_label(
                    egui::Color32::from_rgb(220, 130, 40),
                    "Локальная модель не выбрана — выберите её в настройках распознавания",
                );
            } else if !crate::models::find(&self.cfg.local_llm.model)
                .is_some_and(|m| m.is_installed())
            {
                ui.colored_label(
                    egui::Color32::from_rgb(220, 130, 40),
                    "Выбранная локальная модель не скачана — до этого дело не дойдёт",
                );
            }
        }
    }

    fn ui_update_block(&mut self, ui: &mut egui::Ui) {
        let mut install: Option<updater::Release> = None;
        let mut relaunch: Option<std::path::PathBuf> = None;
        let mut recheck = false;

        match &self.update {
            UpdateState::Idle => {
                if ui.button("Проверить обновления").clicked() {
                    recheck = true;
                }
            }
            UpdateState::Checking(_) => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Проверяю…");
                });
            }
            UpdateState::UpToDate => {
                ui.colored_label(
                    egui::Color32::from_rgb(60, 160, 90),
                    "Установлена последняя версия",
                );
                if ui.button("Проверить ещё раз").clicked() {
                    recheck = true;
                }
            }
            UpdateState::Available(r) => {
                ui.colored_label(
                    egui::Color32::from_rgb(90, 140, 240),
                    format!("Доступна версия {}", r.version),
                );
                if !r.notes.trim().is_empty() {
                    ui.collapsing("Что нового", |ui| {
                        ui.weak(r.notes.trim());
                    });
                }
                if ui.button("Установить и перезапустить").clicked() {
                    install = Some(r.clone());
                }
                ui.weak(
                    "Образ скачивается самим приложением, поэтому карантин на него \
                     не ставится и снимать его вручную не придётся.",
                );
            }
            UpdateState::Installing(_) => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Скачиваю и ставлю…");
                });
            }
            UpdateState::Installed(path) => {
                ui.colored_label(
                    egui::Color32::from_rgb(60, 160, 90),
                    "Обновление установлено",
                );
                if ui.button("Перезапустить сейчас").clicked() {
                    relaunch = Some(path.clone());
                }
            }
            UpdateState::Failed(e) => {
                ui.colored_label(egui::Color32::from_rgb(220, 80, 80), format!("Ошибка: {e}"));
                if ui.button("Попробовать снова").clicked() {
                    recheck = true;
                }
            }
        }

        if recheck {
            self.start_update_check();
        }
        if let Some(r) = install {
            self.start_update_install(r);
        }
        if let Some(path) = relaunch {
            updater::relaunch(&path);
        }
    }

    fn ui_history(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.filter, HistoryFilter::All, "Все");
            ui.selectable_value(&mut self.filter, HistoryFilter::Transformed, "С обработкой");
            ui.selectable_value(&mut self.filter, HistoryFilter::Errors, "Ошибки");
            ui.separator();
            if ui.button("Очистить историю").clicked() {
                let _ = history::clear();
                self.shared.history.lock().unwrap().clear();
                self.toast("История очищена");
            }
        });
        ui.add_space(6.0);

        let entries = self.shared.history.lock().unwrap().clone();
        let filtered: Vec<&history::Entry> = entries
            .iter()
            .filter(|e| match self.filter {
                HistoryFilter::All => true,
                HistoryFilter::Transformed => e.was_transformed(),
                HistoryFilter::Errors => e.error.is_some(),
            })
            .collect();

        if filtered.is_empty() {
            ui.weak("Пока пусто.");
            return;
        }

        let mut copy: Option<String> = None;
        egui::ScrollArea::vertical().show(ui, |ui| {
            for (i, e) in filtered.iter().enumerate() {
                ui.push_id(i, |ui| {
                    ui.horizontal(|ui| {
                        ui.weak(e.at.format("%d.%m %H:%M:%S").to_string());
                        ui.weak("·");
                        ui.weak(&e.mode);
                        ui.weak("·");
                        if let Some(engine) = &e.engine {
                            ui.weak(engine);
                            ui.weak("·");
                        }
                        ui.weak(format!("{:.1} с речи", e.duration_secs));
                        ui.weak("·");
                        ui.weak(format!("{} мс", e.latency_ms));
                    });

                    if let Some(err) = &e.error {
                        ui.colored_label(egui::Color32::from_rgb(220, 80, 80), err);
                    } else {
                        ui.label(&e.final_text);
                        if e.was_transformed() {
                            ui.collapsing("Исходное распознавание", |ui| {
                                ui.weak(&e.raw_text);
                            });
                        }
                        ui.horizontal(|ui| {
                            if ui.small_button("Копировать").clicked() {
                                copy = Some(e.final_text.clone());
                            }
                            if e.was_transformed()
                                && ui.small_button("Копировать оригинал").clicked()
                            {
                                copy = Some(e.raw_text.clone());
                            }
                        });
                    }
                    ui.separator();
                });
            }
        });

        if let Some(text) = copy {
            let _ = insert::write_clipboard(&text);
            self.toast("Скопировано");
        }
    }

    fn ui_clipboard(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("История буфера обмена").strong());
            if !self.cfg.general.clipboard_hotkey.is_empty() {
                ui.weak(format!(
                    "быстрый выбор: {}",
                    self.cfg.general.clipboard_hotkey.label()
                ));
            }
        });
        if !self.cfg.general.clipboard_history {
            ui.colored_label(
                egui::Color32::from_rgb(220, 130, 40),
                "Запоминание выключено — включите в настройках.",
            );
        }
        ui.add_space(6.0);

        ui.horizontal(|ui| {
            if ui.button("Очистить историю").clicked() {
                let _ = crate::clipboard::clear();
                self.shared.clipboard.entries.lock().unwrap().clear();
                self.toast("История буфера очищена");
            }
            let days = self.cfg.general.clipboard_days;
            if days > 0 {
                ui.weak(format!("хранится {days} дн."));
            }
        });
        ui.add_space(6.0);

        let entries = self.shared.clipboard.entries.lock().unwrap().clone();
        if entries.is_empty() {
            ui.weak("Пока пусто. Скопируйте что-нибудь, и оно появится здесь.");
            return;
        }

        let mut copy: Option<String> = None;
        egui::ScrollArea::vertical().show(ui, |ui| {
            for (i, entry) in entries.iter().take(300).enumerate() {
                ui.push_id(i, |ui| {
                    ui.horizontal(|ui| {
                        ui.weak(entry.at.format("%d.%m %H:%M").to_string());
                        if let Some(source) = &entry.source {
                            ui.weak("·");
                            ui.weak(source);
                        }
                    });
                    ui.label(entry.preview(160));
                    if ui.small_button("Вернуть в буфер").clicked() {
                        copy = Some(entry.text.clone());
                    }
                    ui.separator();
                });
            }
        });

        if let Some(text) = copy {
            self.shared.clipboard.mark_ours(&text);
            let _ = insert::write_clipboard(&text);
            self.toast("Скопировано");
        }
    }

    fn ui_permissions(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.label(egui::RichText::new("Системные разрешения").strong());
        ui.add_space(8.0);

        let (ax, mic) = self.perms;

        perm_row(
            ui,
            "Универсальный доступ",
            ax,
            "нужен, чтобы слышать горячую клавишу и вставлять текст",
        );
        ui.horizontal(|ui| {
            if ui.button("Запросить").clicked() {
                permissions::prompt_accessibility();
            }
            if ui.button("Открыть настройки").clicked() {
                permissions::open_accessibility_settings();
            }
            if ui.button("Сбросить и выдать заново").clicked() {
                match permissions::reset_accessibility() {
                    Ok(()) => {
                        self.toast("Запись сброшена — выдайте доступ заново");
                        permissions::prompt_accessibility();
                    }
                    Err(e) => self.toast(format!("tccutil: {e}")),
                }
            }
        });
        if !ax.is_ok() {
            ui.add_space(2.0);
            ui.weak(
                "Если в Системных настройках тумблер выглядит включённым, а здесь \
                 написано «запрещён» — там осталась запись от прежней сборки с другой \
                 подписью. Нажмите «Сбросить и выдать заново», затем перезапустите \
                 приложение.",
            );
        }

        ui.add_space(12.0);
        let listen = permissions::input_monitoring();
        perm_row(
            ui,
            "Мониторинг ввода",
            listen,
            "нужен, чтобы получать клавиши раньше других программ",
        );
        ui.horizontal(|ui| {
            if ui.button("Запросить").clicked() {
                permissions::prompt_input_monitoring();
            }
            if ui.button("Открыть настройки").clicked() {
                permissions::open_input_monitoring_settings();
            }
            if ui.button("Сбросить и выдать заново").clicked() {
                match permissions::reset_input_monitoring() {
                    Ok(()) => {
                        self.toast("Запись сброшена — выдайте доступ заново");
                        permissions::prompt_input_monitoring();
                    }
                    Err(e) => self.toast(format!("tccutil: {e}")),
                }
            }
        });
        if !listen.is_ok() {
            ui.weak(
                "Если тумблер в настройках включён, а здесь написано «запрещён» — \
                 там осталась запись от прежней сборки. Нажмите «Сбросить и выдать \
                 заново», удалите строку LLM-dict из системного списка кнопкой −, \
                 затем перезапустите приложение.",
            );
        }
        if !listen.is_ok() {
            ui.weak(
                "Без него перехватчик работает уровнем выше, и сочетания, которые \
                 забирает другая программа, до нас не доходят.",
            );
        }

        ui.add_space(12.0);
        perm_row(ui, "Микрофон", mic, "нужен для записи речи");
        ui.horizontal(|ui| {
            if ui.button("Запросить").clicked() {
                permissions::prompt_microphone();
            }
            if ui.button("Открыть настройки").clicked() {
                permissions::open_microphone_settings();
            }
            if ui.button("Сбросить").clicked() {
                match permissions::reset_microphone() {
                    Ok(()) => self.toast("Запись сброшена"),
                    Err(e) => self.toast(format!("tccutil: {e}")),
                }
            }
        });

        // Дубликаты ищем не каждый кадр: mdfind — это запуск процесса.
        if self.duplicates_checked.elapsed() > Duration::from_secs(30) {
            self.duplicates = permissions::duplicate_bundles();
            self.duplicates_checked = Instant::now();
        }
        if !self.duplicates.is_empty() {
            ui.add_space(14.0);
            ui.colored_label(
                egui::Color32::from_rgb(220, 130, 40),
                "Найдены другие копии приложения",
            );
            ui.weak(
                "У них тот же идентификатор, но другая подпись. macOS заводит на них \
                 отдельные записи разрешений, а в списке показывает один пункт — \
                 отсюда «доступ выдан, но не работает». Оставьте одну копию.",
            );
            for path in &self.duplicates {
                ui.weak(format!("• {path}"));
            }
        }

        ui.add_space(16.0);
        ui.separator();
        ui.add_space(6.0);
        ui.label(egui::RichText::new("Что видит приложение").strong());
        ui.add_space(4.0);

        let (held, events, regular) = self.shared.hotkey_state.diagnostics();

        // Уровень перехватчика решает, увидим ли мы сочетания, занятые
        // другими программами, — и увидим ли вообще нажатия клавиш.
        if self.shared.hotkey_state.is_hid_level() {
            ui.colored_label(
                egui::Color32::from_rgb(60, 160, 90),
                "Перехватчик на HID-уровне — видит клавиши раньше других программ",
            );
        } else {
            ui.colored_label(
                egui::Color32::from_rgb(220, 130, 40),
                "Перехватчик на уровне сессии",
            );
            ui.weak(
                "Сочетания, которые забирает другая программа, до нас не дойдут. \
                 Выдайте «Мониторинг ввода» выше и перезапустите приложение.",
            );
        }
        ui.add_space(6.0);
        if events == 0 {
            ui.colored_label(
                egui::Color32::from_rgb(220, 80, 80),
                "Ни одного события клавиатуры не получено — слежение не работает. \
                 Выдайте «Универсальный доступ» и перезапустите приложение.",
            );
        } else {
            let shown = if held.is_empty() {
                "ничего не зажато".to_string()
            } else {
                format!(
                    "{}  ({} шт.)",
                    held.iter()
                        .map(|k| crate::binding::key_label(*k))
                        .collect::<Vec<_>>()
                        .join(" + "),
                    held.len()
                )
            };
            ui.horizontal(|ui| {
                ui.label("Сейчас зажато:");
                ui.colored_label(egui::Color32::from_rgb(90, 140, 240), shown);
            });
            ui.weak(format!(
                "Событий получено: {events}. Зажмите нужные клавиши — если здесь \
                 они не появляются, до приложения они не доходят."
            ));

            ui.add_space(4.0);
            // Считаем только количество, без кодов: журнал не должен
            // превращаться в запись того, что печатают.
            let color = if regular == 0 {
                egui::Color32::from_rgb(220, 80, 80)
            } else {
                egui::Color32::from_rgb(60, 160, 90)
            };
            ui.colored_label(color, format!("Нажатий обычных клавиш получено: {regular}"));
            if regular == 0 {
                ui.weak(
                    "Напечатайте несколько букв в любом окне. Если счётчик не растёт, \
                     значит обычные клавиши до перехватчиков не доходят — сочетание с \
                     буквой задать не получится, берите сочетание из модификаторов.",
                );
            }

            let recent = self.shared.hotkey_state.recent_events();
            if !recent.is_empty() {
                ui.add_space(4.0);
                ui.collapsing(
                    "Последние события клавиатуры",
                    |ui| {
                        ui.weak(
                            "Сырой список до всякого разбора. Если здесь нет строк \
                         «нажатие» для обычных клавиш, значит система их \
                         перехватчикам не отдаёт.",
                        );
                        for line in recent.iter().take(14) {
                            ui.monospace(line);
                        }
                    },
                );
            }
        }

        ui.add_space(10.0);
        ui.label(egui::RichText::new("Проверка захвата выделенного").strong());
        ui.weak(
            "Выделите текст в любой программе, вернитесь сюда и нажмите кнопку. \
             Так видно, доходит ли выделение до приложения.",
        );
        if ui.button("Прочитать выделенное").clicked() {
            match crate::insert::copy_selection() {
                Ok((text, _)) => {
                    let preview: String = text.chars().take(120).collect();
                    self.toast(format!(
                        "Прочитано {} симв.: {preview}",
                        text.chars().count()
                    ));
                }
                Err(e) => self.toast(format!("Не прочитать: {e}")),
            }
        }

        ui.add_space(16.0);
        ui.separator();
        ui.add_space(6.0);
        ui.weak(
            "После выдачи «Универсального доступа» приложение нужно перезапустить: \
             macOS не отдаёт разрешение уже запущенному процессу.",
        );
        ui.add_space(4.0);
        ui.weak(format!(
            "Запущено из: {}",
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent()?.parent()?.parent().map(|p| p.to_path_buf()))
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "неизвестно".into())
        ));
    }
}

fn perm_row(ui: &mut egui::Ui, name: &str, status: permissions::Status, hint: &str) {
    ui.horizontal(|ui| {
        let color = if status.is_ok() {
            egui::Color32::from_rgb(60, 160, 90)
        } else {
            egui::Color32::from_rgb(220, 130, 40)
        };
        let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
        ui.painter().circle_filled(rect.center(), 4.0, color);
        ui.label(egui::RichText::new(name).strong());
        ui.weak(format!("— {}", status.label()));
    });
    ui.weak(hint);
}

fn labeled(ui: &mut egui::Ui, label: &str, content: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.add_sized([130.0, 20.0], egui::Label::new(label));
        content(ui);
    });
}

/// Список моделей появляется после успешной проверки ключа; до этого — обычное поле.
fn model_picker(ui: &mut egui::Ui, id: &str, value: &mut String, models: &[String], filter: &str) {
    if models.is_empty() {
        ui.add(egui::TextEdit::singleline(value).desired_width(320.0));
        return;
    }
    egui::ComboBox::from_id_salt(id)
        .width(320.0)
        .selected_text(value.clone())
        .show_ui(ui, |ui| {
            for m in models
                .iter()
                .filter(|m| filter.is_empty() || m.contains(filter))
            {
                ui.selectable_value(value, m.clone(), m);
            }
        });
}
