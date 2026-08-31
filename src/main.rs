// Скрывает окно консоли — приложение фоновое.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod actions;
mod app;
mod audio;
mod autostart;
mod binding;
mod config;
mod conflicts;
mod engine;
mod fonts;
mod history;
mod hotkey;
mod insert;
mod logging;
mod macos;
mod models;
mod overlay;
mod permissions;
mod provider;
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

/// Прогоняет действие над заданным текстом, минуя горячие клавиши и буфер.
///
/// Отвечает на вопрос «дело в клавишах или в самом запросе к модели»: здесь
/// не нужны ни разрешения, ни выделение — только поставщик, модель и ключ.
fn run_action_test(name: &str, text: &str) -> eframe::Result<()> {
    let cfg = config::Config::load().normalized();
    let Some(action) = cfg
        .actions
        .iter()
        .find(|a| a.name.eq_ignore_ascii_case(name) || a.id == name)
    else {
        eprintln!("не найдено действие «{name}». Есть такие:");
        for a in &cfg.actions {
            eprintln!("  — {}", a.name);
        }
        std::process::exit(1);
    };

    println!(
        "Действие: {}\nПоставщик: {} ({})\nМодель: {}\n",
        action.name,
        action.endpoint.provider.label(),
        action.endpoint.base_url(),
        action.endpoint.model
    );

    let started = std::time::Instant::now();
    let key = action.endpoint.api_key(&cfg);
    match providers::run_prompt(&action.endpoint, &key, &action.prompt, text) {
        Ok(result) => println!(
            "Готово за {:.2} с:\n{result}",
            started.elapsed().as_secs_f32()
        ),
        Err(e) => {
            eprintln!("Ошибка: {e}");
            std::process::exit(1);
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
    if args.len() >= 4 && args[1] == "--test-action" {
        return run_action_test(&args[2], &args[3]);
    }

    let cfg = config::Config::load().normalized();
    // Набор действий по умолчанию создаётся в памяти — сохраняем сразу, иначе
    // файл настроек расходится с тем, что видит пользователь.
    if !config::config_path().exists() {
        let _ = cfg.save();
    }
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
        Box::new(move |cc| {
            fonts::install(&cc.egui_ctx);
            macos::set_dock_visible(show_in_dock);
            // Рабочие потоки должны уметь разбудить интерфейс: пока окно
            // скрыто, eframe сам кадры не выполняет, а плашку рисовать надо.
            let ctx = cc.egui_ctx.clone();
            shared.set_wake(Box::new(move || ctx.request_repaint()));
            Ok(Box::new(app::App::new(Arc::clone(&shared))))
        }),
    )
}
