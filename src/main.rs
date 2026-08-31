// Скрывает окно консоли — приложение фоновое.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod audio;
mod autostart;
mod config;
mod engine;
mod history;
mod hotkey;
mod insert;
mod macos;
mod permissions;
mod providers;

use std::sync::Arc;

fn main() -> eframe::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let cfg = config::Config::load();
    let show_in_dock = cfg.general.show_in_dock;
    let shared = engine::Shared::new(cfg);

    // Слушатель клавиши и обработчик поднимаются до окна: диктовка должна
    // работать, даже если окно ни разу не открывали.
    let _hotkey_tx = engine::spawn(shared.clone());

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("LLM-dict")
            .with_inner_size([600.0, 680.0])
            .with_min_inner_size([480.0, 440.0]),
        ..Default::default()
    };

    eframe::run_native(
        "LLM-dict",
        options,
        Box::new(move |_cc| {
            macos::set_dock_visible(show_in_dock);
            Ok(Box::new(app::App::new(Arc::clone(&shared))))
        }),
    )
}
