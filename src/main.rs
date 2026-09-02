// Скрывает окно консоли — приложение фоновое.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod actions;
mod app;
mod audio;
mod autostart;
mod binding;
mod clipboard;
mod config;
mod conflicts;
mod engine;
mod fonts;
mod history;
mod hotkey;
mod insert;
mod local_llm;
mod logging;
mod macos;
mod models;
mod net;
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
                models::Engine::Parakeet => &cfg.stt.parakeet_model,
                models::Engine::Cloud | models::Engine::Llm => "",
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
/// Прогон текста через локальную языковую модель.
///
/// Пользуется тем же модулем, что и диктовка, — поэтому проверяет ровно то,
/// что работает в релизе, а не отдельную копию кода.
fn run_local_test(text: &str) -> eframe::Result<()> {
    let cfg = config::Config::load().normalized();
    let id = cfg.local_llm.model.clone();
    if id.is_empty() {
        eprintln!("локальная модель не выбрана в настройках");
        std::process::exit(1);
    }
    let Some(spec) = models::find(&id) else {
        eprintln!("неизвестная модель: {id}");
        std::process::exit(1);
    };
    println!("Модель: {} ({})", spec.title, id);
    if !spec.is_installed() {
        eprintln!("модель не скачана: {}", spec.dir().display());
        std::process::exit(1);
    }

    // Промпт берём у действия после диктовки — как в настоящем конвейере.
    let prompt = cfg
        .actions
        .iter()
        .find(|a| a.enabled && a.after_dictation)
        .map(|a| a.prompt.clone())
        .unwrap_or_else(|| local_llm::CORRECT_PROMPT.to_string());

    let mut llm = local_llm::LocalLlm::default();
    let loaded = std::time::Instant::now();
    if let Err(e) = llm.ensure(&id) {
        eprintln!("не загрузить: {e}");
        std::process::exit(1);
    }
    println!("Загрузка: {:.2} с", loaded.elapsed().as_secs_f32());

    let started = std::time::Instant::now();
    match llm.run(&id, &prompt, text) {
        Ok(out) => {
            println!("\nБыло:  {text}");
            println!("Стало: {out}");
            println!(
                "\nВремя обработки: {:.2} с",
                started.elapsed().as_secs_f32()
            );
            if out == text {
                println!("Модель ничего не изменила.");
            }
        }
        Err(e) => {
            println!("\nБыло:  {text}");
            println!("Отказ: {e}");
            println!("В диктовке это означает вставку исходного текста без изменений.");
        }
    }
    println!("Загружена в памяти: {}", llm.is_loaded());
    llm.unload();
    println!("После выгрузки: {}", llm.is_loaded());
    Ok(())
}

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
    let context = action.load_context().unwrap_or(None);
    match providers::run_prompt(
        &action.endpoint,
        &key,
        &action.prompt,
        context.as_deref(),
        text,
    ) {
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

/// Печатает всё, что видит перехватчик, разбирая события тем же кодом, что и
/// приложение. Нужен, чтобы отделить «событие не пришло» от «пришло, но мы
/// его выбросили при разборе».
fn run_watch_keys() -> eframe::Result<()> {
    // Полный путь приложения: перехватчик → канал → рабочий поток → общее
    // состояние → опрос из окна. Прямое чтение канала проверяло только
    // половину, а сочетание могло теряться и дальше.
    let cfg = config::Config::load().normalized();
    let shared = engine::Shared::new(cfg);
    let _tx = engine::spawn(shared.clone());
    // Флаг готовности перехватчика выставляется внутри spawn через 400 мс —
    // ждём дольше, иначе читаем его раньше времени.
    std::thread::sleep(std::time::Duration::from_millis(900));

    if !shared
        .tap_running
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        eprintln!("перехватчик не поднялся — нет «Универсального доступа»");
        std::process::exit(1);
    }
    shared.hotkey_state.set_capturing(true);

    println!("Слушаю 20 секунд. Нажимайте сочетания — покажу всё, что дойдёт.\n");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut seen_last = String::new();
    let mut regular_keys = 0usize;
    let mut max_len = 0usize;

    while std::time::Instant::now() < deadline {
        // Сырые события: печатаем только новые, иначе список повторяется.
        if let Some(line) = shared.hotkey_state.recent_events().into_iter().next() {
            if line != seen_last {
                if line.starts_with("нажатие") {
                    regular_keys += 1;
                }
                println!("  {line}");
                seen_last = line;
            }
        }
        // Ровно то же, что делает окно настроек.
        if let Some(keys) = shared.take_captured() {
            let names: Vec<String> = keys.iter().map(|k| binding::key_label(*k)).collect();
            max_len = max_len.max(keys.len());
            println!("НАБОР ИЗ {} КЛАВИШ: {}", keys.len(), names.join(" + "));
        }
        std::thread::sleep(std::time::Duration::from_millis(16));
    }

    println!("\n--- итог ---");
    println!("Самый длинный набор: {max_len} клавиш");
    if regular_keys == 0 {
        println!("Обычных клавиш (букв, цифр, пробела) не нажималось ни разу —");
        println!("до перехватчика дошли только модификаторы.");
    } else {
        println!("Обычных клавиш получено: {regular_keys}. Перехватчик их видит.");
    }
    Ok(())
}

fn main() -> eframe::Result<()> {
    logging::init();

    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 3 && args[1] == "--bench" {
        return run_bench(&args[2], args.get(3).map(|s| s.as_str()));
    }
    if args.len() >= 2 && args[1] == "--watch-keys" {
        return run_watch_keys();
    }
    if args.len() >= 3 && args[1] == "--test-local" {
        return run_local_test(&args[2..].join(" "));
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
    // Без «Мониторинга ввода» macOS отдаёт перехватчику только модификаторы,
    // а нажатия обычных клавиш молча отсекает. Спрашиваем сразу: иначе
    // сочетания с буквами не работают, и понять почему невозможно.
    if permissions::input_monitoring() == permissions::Status::NotAsked {
        permissions::prompt_input_monitoring();
    }

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
