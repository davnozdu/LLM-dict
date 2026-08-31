//! Окно настроек, статуса и истории.

use crate::binding::Binding;
use crate::config::{secrets, Config, HotKeyMode, PostMode};
use crate::engine::{self, Shared, Stage};
use crate::history;
use crate::insert;
use crate::permissions;
use crate::{autostart, conflicts, macos, providers, updater};

use eframe::egui;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};

#[derive(PartialEq, Eq, Clone, Copy)]
enum Tab {
    Status,
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
    duplicates: Vec<String>,
    duplicates_checked: Instant,
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
            perms_checked: Instant::now() - Duration::from_secs(10),
            capturing: false,
            capture_preview: Vec::new(),
            verdict: conflicts::Verdict::Free,
            verdict_for: Binding::new(Vec::new()),
            update: UpdateState::Idle,
            update_checked: false,
            duplicates: Vec::new(),
            duplicates_checked: Instant::now() - Duration::from_secs(60),
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
    fn draw_overlay(&self, ctx: &egui::Context) {
        let stage = self.shared.stage();
        if !self.cfg.general.show_overlay || !stage.is_busy() {
            return;
        }
        let Some((mx, my)) = macos::cursor_position() else {
            return;
        };
        let size = egui::vec2(132.0, 34.0);
        let pos = egui::pos2(mx - size.x / 2.0, my - size.y - 22.0);

        let level = self.shared.level.get();
        let id = egui::ViewportId::from_hash_of("llm_dict_overlay");
        ctx.show_viewport_deferred(
            id,
            egui::ViewportBuilder::default()
                .with_title("LLM-dict")
                .with_inner_size(size)
                .with_position(pos)
                .with_decorations(false)
                .with_transparent(true)
                .with_resizable(false)
                .with_always_on_top()
                .with_mouse_passthrough(true)
                .with_taskbar(false)
                .with_active(false)
                .with_has_shadow(false),
            move |ctx, _class| {
                let (dot, text) = match stage {
                    Stage::Recording => (egui::Color32::from_rgb(235, 70, 70), "слушаю"),
                    Stage::Transcribing => (egui::Color32::from_rgb(235, 165, 45), "распознаю"),
                    Stage::PostProcessing => (egui::Color32::from_rgb(95, 145, 245), "обрабатываю"),
                    Stage::Inserting => (egui::Color32::from_rgb(65, 195, 115), "вставляю"),
                    Stage::Idle => (egui::Color32::GRAY, ""),
                };
                let frame = egui::Frame::NONE
                    .fill(egui::Color32::from_rgba_unmultiplied(28, 28, 32, 235))
                    .corner_radius(egui::CornerRadius::same(17))
                    .inner_margin(egui::Margin::symmetric(12, 8));
                egui::CentralPanel::default()
                    .frame(egui::Frame::NONE)
                    .show(ctx, |ui| {
                        frame.show(ui, |ui| {
                            ui.horizontal(|ui| {
                                // Кружок пульсирует в такт громкости — сразу
                                // видно, что микрофон действительно слышит.
                                let pulse = 4.0 + level.clamp(0.0, 1.0) * 4.0;
                                let (rect, _) = ui.allocate_exact_size(
                                    egui::vec2(pulse * 2.0 + 4.0, pulse * 2.0 + 4.0),
                                    egui::Sense::hover(),
                                );
                                ui.painter().circle_filled(rect.center(), pulse, dot);
                                ui.label(
                                    egui::RichText::new(text)
                                        .color(egui::Color32::from_gray(235))
                                        .size(13.0),
                                );
                            });
                        });
                    });
                ctx.request_repaint_after(Duration::from_millis(50));
            },
        );
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
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        self.ensure_tray();
        self.sync_tray();
        self.poll_model_check();
        self.poll_capture();
        self.poll_update();
        self.refresh_verdict();
        self.draw_overlay(ctx);

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

        while let Ok(event) = MenuEvent::receiver().try_recv() {
            match event.id.as_ref() {
                MENU_OPEN => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    macos::activate();
                }
                MENU_QUIT => std::process::exit(0),
                _ => {}
            }
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
                ui.selectable_value(&mut self.tab, Tab::Settings, "Настройки");
                ui.selectable_value(&mut self.tab, Tab::History, "История");
                ui.selectable_value(&mut self.tab, Tab::Clipboard, "Буфер");
                let perms_ok = self.perms.0.is_ok() && self.perms.1.is_ok();
                let label = if perms_ok {
                    "Права"
                } else {
                    "Права ⚠"
                };
                ui.selectable_value(&mut self.tab, Tab::Permissions, label);
            });
            ui.add_space(4.0);
        });

        egui::Panel::bottom("status_bar").show(ui, |ui| {
            ui.add_space(3.0);
            ui.horizontal(|ui| {
                let stage = self.shared.stage();
                let dot = match stage {
                    Stage::Idle => egui::Color32::from_gray(140),
                    Stage::Recording => egui::Color32::from_rgb(230, 60, 60),
                    Stage::Transcribing => egui::Color32::from_rgb(230, 160, 40),
                    Stage::PostProcessing => egui::Color32::from_rgb(90, 140, 240),
                    Stage::Inserting => egui::Color32::from_rgb(60, 190, 110),
                };
                ui.colored_label(dot, "●");
                ui.label(stage.label());
                ui.separator();
                ui.label(format!(
                    "{} · {}",
                    self.cfg.general.hotkey.label(),
                    engine::hotkey_mode_label(self.cfg.general.hotkey_mode)
                ));
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
            Tab::Settings => self.ui_settings(ui),
            Tab::History => self.ui_history(ui),
            Tab::Clipboard => self.ui_clipboard(ui),
            Tab::Permissions => self.ui_permissions(ui),
        });

        self.apply_config();

        if self.shared.stage().is_busy() || self.model_check.is_some() {
            ctx.request_repaint_after(Duration::from_millis(60));
        } else {
            ctx.request_repaint_after(Duration::from_millis(500));
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
            let hint = match self.cfg.general.hotkey_mode {
                HotKeyMode::Hold => "Удерживайте",
                HotKeyMode::Toggle => "Нажмите",
            };
            ui.label(format!(
                "{hint} {} и говорите. Текст встанет туда, где стоит курсор.",
                self.cfg.general.hotkey.label()
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
                    Binding::new(self.capture_preview.clone()).label()
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
                            "✔ сочетание свободно",
                        );
                    }
                    conflicts::Verdict::Taken(why) => {
                        ui.colored_label(egui::Color32::from_rgb(220, 80, 80), format!("✖ {why}"));
                    }
                    conflicts::Verdict::Risky(why) => {
                        ui.colored_label(egui::Color32::from_rgb(220, 130, 40), format!("⚠ {why}"));
                    }
                }
            }

            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.selectable_value(
                    &mut self.cfg.general.hotkey_mode,
                    HotKeyMode::Hold,
                    "Удержание",
                );
                ui.selectable_value(
                    &mut self.cfg.general.hotkey_mode,
                    HotKeyMode::Toggle,
                    "Нажатие / повторное нажатие",
                );
            });
            ui.add_space(4.0);
            ui.checkbox(
                &mut self.cfg.general.swallow_hotkey,
                "Перехватывать клавишу — не давать сработать её обычному действию",
            );
            if self.cfg.general.swallow_hotkey {
                ui.weak(
                    "Событие клавиши не уходит дальше в систему, пока приложение работает. \
                     Так можно занять 🌐 или другую уже назначенную клавишу. Перехватывается \
                     только то, что входит в сочетание — остальная клавиатура не затрагивается.",
                );
            } else {
                ui.weak("Клавиши только прослушиваются, их обычное действие сохраняется.");
            }

            ui.add_space(12.0);
            ui.separator();
            ui.add_space(6.0);
            ui.label(egui::RichText::new("Распознавание речи").strong());
            ui.add_space(4.0);
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
            labeled(ui, "Язык", |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.cfg.stt.language)
                        .desired_width(120.0)
                        .hint_text("авто"),
                );
                ui.weak("код вида ru, en; пусто — определять самому");
            });
            labeled(ui, "Подсказка", |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.cfg.stt.prompt)
                        .desired_width(320.0)
                        .hint_text("имена и термины, которые часто путает"),
                );
            });

            ui.add_space(12.0);
            ui.separator();
            ui.add_space(6.0);
            ui.label(egui::RichText::new("Обработка текста").strong());
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                for m in PostMode::ALL {
                    ui.selectable_value(&mut self.cfg.llm.mode, m, m.label());
                }
            });
            if !matches!(self.cfg.llm.mode, PostMode::Raw) {
                labeled(ui, "Адрес API", |ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.cfg.llm.base_url).desired_width(320.0),
                    );
                });
                labeled(ui, "Модель", |ui| {
                    model_picker(ui, "llm_model", &mut self.cfg.llm.model, &self.models, "");
                });
            }
            if matches!(self.cfg.llm.mode, PostMode::Translate) {
                labeled(ui, "Целевой язык", |ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.cfg.llm.target_language)
                            .desired_width(200.0),
                    );
                });
            }
            if matches!(self.cfg.llm.mode, PostMode::Custom) {
                ui.label("Промпт:");
                ui.add(
                    egui::TextEdit::multiline(&mut self.cfg.llm.custom_prompt)
                        .desired_rows(3)
                        .desired_width(f32::INFINITY),
                );
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
            ui.weak(format!(
                "Настройки: {}",
                crate::config::config_path().display()
            ));
            ui.add_space(12.0);
        });
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
        ui.label(egui::RichText::new("Что лежало в буфере обмена перед вставкой").strong());
        ui.weak("Вставка идёт через буфер, поэтому прежнее содержимое сохраняется здесь — на случай, если оно было нужно.");
        ui.add_space(8.0);

        if let Some(current) = insert::read_clipboard() {
            ui.weak("Сейчас в буфере:");
            ui.label(truncate(&current, 300));
            ui.separator();
            ui.add_space(4.0);
        }

        let entries = self.shared.history.lock().unwrap().clone();
        let with_clip: Vec<&history::Entry> = entries
            .iter()
            .filter(|e| {
                e.clipboard_before
                    .as_deref()
                    .is_some_and(|s| !s.trim().is_empty())
            })
            .collect();

        if with_clip.is_empty() {
            ui.weak("Пока пусто.");
            return;
        }

        let mut restore: Option<String> = None;
        egui::ScrollArea::vertical().show(ui, |ui| {
            for (i, e) in with_clip.iter().enumerate() {
                let Some(clip) = &e.clipboard_before else {
                    continue;
                };
                ui.push_id(i, |ui| {
                    ui.weak(e.at.format("%d.%m %H:%M:%S").to_string());
                    ui.label(truncate(clip, 400));
                    if ui.small_button("Вернуть в буфер").clicked() {
                        restore = Some(clip.clone());
                    }
                    ui.separator();
                });
            }
        });

        if let Some(text) = restore {
            let _ = insert::write_clipboard(&text);
            self.toast("Возвращено в буфер");
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
        let (color, mark) = if status.is_ok() {
            (egui::Color32::from_rgb(60, 160, 90), "✔")
        } else {
            (egui::Color32::from_rgb(220, 130, 40), "✖")
        };
        ui.colored_label(color, mark);
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

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}…")
}
