//! Окно настроек, статуса и истории.

use crate::config::{secrets, Config, HotKey, HotKeyMode, PostMode};
use crate::engine::{self, Shared, Stage};
use crate::history;
use crate::insert;
use crate::permissions;
use crate::{autostart, macos, providers};

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
            ui.label(format!(
                "Удерживайте {} и говорите. Текст встанет туда, где стоит курсор.",
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
        }

        if self.api_key_input.trim().is_empty() {
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
                if ui.button("Сохранить в Keychain").clicked() {
                    match secrets::set("groq_api_key", self.api_key_input.trim()) {
                        Ok(()) => {
                            *self.shared.api_key.write().unwrap() =
                                self.api_key_input.trim().to_string();
                            self.toast("Ключ сохранён в Keychain");
                        }
                        Err(e) => self.toast(format!("Keychain: {e}")),
                    }
                }
                if ui.button("Проверить ключ").clicked() {
                    self.start_model_check();
                }
            });
            if let Some((msg, ok)) = &self.check_message {
                let color = if *ok {
                    egui::Color32::from_rgb(60, 160, 90)
                } else {
                    egui::Color32::from_rgb(220, 80, 80)
                };
                ui.colored_label(color, msg);
            }
            ui.weak("Ключ хранится в системной связке ключей, не в файле настроек.");

            ui.add_space(12.0);
            ui.separator();
            ui.add_space(6.0);
            ui.label(egui::RichText::new("Горячая клавиша").strong());
            ui.add_space(4.0);
            egui::ComboBox::from_id_salt("hotkey")
                .width(220.0)
                .selected_text(self.cfg.general.hotkey.label())
                .show_ui(ui, |ui| {
                    for k in HotKey::ALL {
                        ui.selectable_value(&mut self.cfg.general.hotkey, k, k.label());
                    }
                });
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
            ui.weak("Клавиша только прослушивается, её обычное действие сохраняется.");

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
            ui.weak(format!(
                "Настройки: {}",
                crate::config::config_path().display()
            ));
            ui.add_space(12.0);
        });
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
        });

        ui.add_space(12.0);
        perm_row(ui, "Микрофон", mic, "нужен для записи речи");
        ui.horizontal(|ui| {
            if ui.button("Запросить").clicked() {
                permissions::prompt_microphone();
            }
            if ui.button("Открыть настройки").clicked() {
                permissions::open_microphone_settings();
            }
        });

        ui.add_space(16.0);
        ui.separator();
        ui.add_space(6.0);
        ui.weak(
            "После выдачи «Универсального доступа» приложение нужно перезапустить: \
             macOS выдаёт разрешение уже запущенному процессу не сразу.",
        );
        ui.add_space(4.0);
        ui.weak(
            "Сборка подписана самоподписанным сертификатом. Если после обновления \
             разрешение слетело — удалите старую запись в списке и добавьте заново.",
        );
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
