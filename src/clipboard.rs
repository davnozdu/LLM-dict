//! История буфера обмена с окном выбора.
//!
//! Отдельный поток следит за счётчиком изменений пастборда и запоминает всё,
//! что туда попадает. Хранится с ротацией по дням: буфер копит переписку и
//! рабочие заметки, и держать это вечно ни к чему.

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

/// Больше этого в историю не берём: в буфер попадают и целые документы,
/// а список должен оставаться списком.
const MAX_ENTRY_BYTES: usize = 64 * 1024;

/// Сколько записей держим в памяти. На диске остаётся всё до ротации, но
/// список без предела за месяц активной работы вырос бы до сотен мегабайт.
const MAX_IN_MEMORY: usize = 500;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub at: DateTime<Local>,
    pub text: String,
    /// Откуда скопировано — помогает вспомнить, что это за кусок.
    #[serde(default)]
    pub source: Option<String>,
    /// Идентификатор бандла программы-источника. По нему разбирается вид
    /// программы: имя для этого не годится — оно переводится и меняется.
    #[serde(default)]
    pub app_id: Option<String>,
    /// Путь к бандлу: по нему система отдаёт значок. Хранится, а не ищется
    /// по идентификатору каждый раз, потому что программу могли и перенести.
    #[serde(default)]
    pub app_path: Option<String>,
}

impl Entry {
    /// Первая строка для списка: в буфер часто попадают целые абзацы.
    pub fn preview(&self, max: usize) -> String {
        let line = self
            .text
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .trim();
        if line.chars().count() <= max {
            line.to_string()
        } else {
            format!("{}…", line.chars().take(max).collect::<String>())
        }
    }
}

pub fn history_path() -> PathBuf {
    crate::config::config_dir().join("clipboard.jsonl")
}

pub fn load() -> Vec<Entry> {
    let Ok(file) = std::fs::File::open(history_path()) else {
        return Vec::new();
    };
    let mut entries: Vec<Entry> = BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(&l).ok())
        .collect();
    entries.reverse(); // новые сверху
    entries
}

/// То же, но только последние записи — для показа в окне.
pub fn load_recent() -> Vec<Entry> {
    let mut entries = load();
    entries.truncate(MAX_IN_MEMORY);
    entries
}

fn append(entry: &Entry) -> anyhow::Result<()> {
    let dir = crate::config::config_dir();
    std::fs::create_dir_all(&dir)?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(history_path())?;
    writeln!(file, "{}", serde_json::to_string(entry)?)?;
    Ok(())
}

/// Выбрасывает записи старше `days` дней. Ноль — не выбрасывать.
pub fn rotate(days: u32) -> anyhow::Result<()> {
    if days == 0 {
        return Ok(());
    }
    let cutoff = Local::now() - chrono::Duration::days(days as i64);
    let kept: Vec<Entry> = load().into_iter().filter(|e| e.at > cutoff).collect();
    let mut out = String::new();
    // На диск пишем в хронологическом порядке, как и читали.
    for e in kept.iter().rev() {
        out.push_str(&serde_json::to_string(e)?);
        out.push('\n');
    }
    crate::persistence::atomic_write(&history_path(), out.as_bytes())?;
    Ok(())
}

pub fn clear() -> anyhow::Result<()> {
    let path = history_path();
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

/// Общая история, которую видит окно.
pub struct History {
    pub entries: Mutex<Vec<Entry>>,
    enabled: AtomicBool,
    days: AtomicU32,
}

impl History {
    pub fn new(enabled: bool, days: u32) -> Arc<Self> {
        let _ = rotate(days);
        let mut entries = load_recent();
        prune(&mut entries, days);
        Arc::new(Self {
            entries: Mutex::new(entries),
            enabled: AtomicBool::new(enabled),
            days: AtomicU32::new(days),
        })
    }

    pub fn set_enabled(&self, v: bool) {
        self.enabled.store(v, Ordering::Relaxed);
    }

    pub fn set_days(&self, days: u32) {
        self.days.store(days, Ordering::Relaxed);
    }

    pub fn clear(&self) -> anyhow::Result<()> {
        let mut entries = self.entries.lock().unwrap();
        clear()?;
        entries.clear();
        Ok(())
    }
}

fn prune(entries: &mut Vec<Entry>, days: u32) {
    if days != 0 {
        let cutoff = Local::now() - chrono::Duration::days(i64::from(days));
        entries.retain(|e| e.at > cutoff);
    }
}

/// Следит за буфером обмена и пополняет историю.
pub fn spawn(history: Arc<History>) {
    std::thread::spawn(move || {
        let mut last_change = crate::insert::pasteboard_change_count();
        // Ротация раз в час: чаще незачем, а на старте важно подчистить.
        let mut applied_days = history.days.load(Ordering::Relaxed);
        let mut next_rotate = std::time::Instant::now() + std::time::Duration::from_secs(3600);

        loop {
            std::thread::sleep(std::time::Duration::from_millis(400));

            let days = history.days.load(Ordering::Relaxed);
            if days != applied_days || std::time::Instant::now() >= next_rotate {
                let mut entries = history.entries.lock().unwrap();
                prune(&mut entries, days);
                if let Err(e) = rotate(days) {
                    log::warn!("не обновить срок хранения буфера: {e}");
                } else {
                    applied_days = days;
                }
                next_rotate = std::time::Instant::now() + std::time::Duration::from_secs(3600);
            }

            if !history.enabled.load(Ordering::Relaxed) {
                last_change = crate::insert::pasteboard_change_count();
                continue;
            }
            let change = crate::insert::pasteboard_change_count();
            if change == last_change {
                continue;
            }
            last_change = change;

            let Some(text) = crate::insert::read_clipboard() else {
                continue;
            };
            if text.trim().is_empty() || text.len() > MAX_ENTRY_BYTES {
                continue;
            }
            // Своё же не запоминаем.
            if crate::insert::is_our_change(change) {
                continue;
            }

            let mut entries = history.entries.lock().unwrap();
            if entries.first().map(|e| e.text.as_str()) == Some(text.as_str()) {
                continue;
            }
            let front = crate::macos::frontmost_app();
            let entry = Entry {
                at: Local::now(),
                text,
                source: front.name,
                app_id: front.id,
                app_path: front.path,
            };
            let _ = append(&entry);
            entries.insert(0, entry);
            entries.truncate(MAX_IN_MEMORY);
        }
    });
}
