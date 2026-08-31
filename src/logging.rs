//! Журнал в файл рядом с системными логами.
//!
//! Приложение фоновое и запускается из Finder, поэтому stderr уходит в никуда:
//! когда что-то не работает, посмотреть было негде. Пишем и в stderr (для
//! запуска из терминала), и в файл.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

/// Больше этого размера файл уезжает в .1, чтобы журнал не рос без предела.
const MAX_BYTES: u64 = 2 * 1024 * 1024;

pub fn log_path() -> PathBuf {
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("Library/Logs/LLM-dict/llm-dict.log")
}

struct FileLogger {
    file: Mutex<Option<File>>,
    level: log::LevelFilter,
}

impl log::Log for FileLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        // whisper.cpp и ggml на каждом запуске выводят десятки строк про
        // устройство и параметры модели. В журнале от них толку нет, а полезное
        // они топят, поэтому пропускаем только предупреждения и ошибки.
        let target = metadata.target();
        if target.starts_with("whisper") || target.starts_with("ggml") {
            return metadata.level() <= log::Level::Warn;
        }
        metadata.level() <= self.level
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = format!(
            "{} {:5} {} — {}\n",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
            record.level(),
            record.target(),
            record.args()
        );
        eprint!("{line}");
        if let Ok(mut guard) = self.file.lock() {
            if let Some(file) = guard.as_mut() {
                let _ = file.write_all(line.as_bytes());
                let _ = file.flush();
            }
        }
    }

    fn flush(&self) {
        if let Ok(mut guard) = self.file.lock() {
            if let Some(file) = guard.as_mut() {
                let _ = file.flush();
            }
        }
    }
}

fn rotate_if_large(path: &PathBuf) {
    let too_big = std::fs::metadata(path)
        .map(|m| m.len() > MAX_BYTES)
        .unwrap_or(false);
    if too_big {
        let _ = std::fs::rename(path, path.with_extension("log.1"));
    }
}

pub fn init() {
    let level = match std::env::var("RUST_LOG").as_deref() {
        Ok("debug") => log::LevelFilter::Debug,
        Ok("trace") => log::LevelFilter::Trace,
        Ok("warn") => log::LevelFilter::Warn,
        Ok("error") => log::LevelFilter::Error,
        _ => log::LevelFilter::Info,
    };

    let path = log_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    rotate_if_large(&path);
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok();

    let logger = Box::new(FileLogger {
        file: Mutex::new(file),
        level,
    });
    if log::set_boxed_logger(logger).is_ok() {
        log::set_max_level(level);
    }
    log::info!(
        "LLM-dict {} запущен, журнал: {}",
        env!("CARGO_PKG_VERSION"),
        path.display()
    );
}
