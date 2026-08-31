// Скрывает окно консоли — приложение фоновое.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod audio;
mod autostart;
mod binding;
mod config;
mod conflicts;
mod engine;
mod history;
mod hotkey;
mod insert;
mod logging;
mod macos;
mod models;
mod permissions;
mod providers;
mod stt;
mod updater;

use std::sync::Arc;

/// Замер локальных движков на готовом файле.
///
/// Нужен, чтобы отвечать на «что быстрее» числами с этой машины, а не
/// оценками по описаниям моделей. Заодно это самая быстрая проверка, что
/// движок вообще заводится, — без запуска интерфейса и микрофона.
fn run_bench(path: &str, language: Option<&str>) -> eframe::Result<()> {
    let mut cfg = config::Config::load().normalized();
    if let Some(lang) = language {
        cfg.stt.language = lang.to_string();
    }
    let samples = match audio::read_wav(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("не прочитать {path}: {e}");
            std::process::exit(1);
        }
    };
    println!(
        "Файл: {path}, {:.2} с речи, {} сэмплов\n",
        audio::duration_secs(&samples),
        samples.len()
    );

    let mut local = stt::LocalEngines::default();
    let key = cfg.load_api_key();

    for engine in models::Engine::ALL {
        let label = engine.label();
        if engine.is_local() {
            let id = match engine {
                models::Engine::Whisper => &cfg.stt.whisper_model,
                models::Engine::Parakeet => &cfg.stt.parakeet_model,
                models::Engine::Cloud => "",
            };
            match models::find(id) {
                Some(spec) if !spec.is_installed() => {
                    println!("{label}: модель {id} не скачана, пропускаю\n");
                    continue;
                }
                None => {
                    println!("{label}: модель {id} неизвестна, пропускаю\n");
                    continue;
                }
                _ => {}
            }
            // Загрузку меряем отдельно: она разовая и в задержку диктовки
            // не входит, если модель прогрета заранее.
            let t = std::time::Instant::now();
            local.preload(engine, id);
            println!(
                "{label}: модель загружена за {:.2} с",
                t.elapsed().as_secs_f32()
            );
        }

        let t = std::time::Instant::now();
        match stt::transcribe(&mut local, &cfg, &key, engine, &samples) {
            Ok(text) => {
                let secs = t.elapsed().as_secs_f32();
                let rt = audio::duration_secs(&samples) / secs.max(0.001);
                println!("{label}: {secs:.2} с ({rt:.1}x реального времени)");
                println!("  → {text}\n");
            }
            Err(e) => println!("{label}: ошибка — {e}\n"),
        }
    }
    Ok(())
}

fn main() -> eframe::Result<()> {
    logging::init();

    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 3 && args[1] == "--bench" {
        return run_bench(&args[2], args.get(3).map(|s| s.as_str()));
    }

    let cfg = config::Config::load().normalized();
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
