//! История диктовок. Хранится построчным JSON рядом с конфигом —
//! файл читается глазами и правится руками, без БД.

use anyhow::Result;
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub at: DateTime<Local>,
    pub duration_secs: f32,
    /// Что вернуло распознавание, до пост-обработки.
    pub raw_text: String,
    /// Что реально вставилось. Совпадает с raw_text, если обработка выключена.
    pub final_text: String,
    pub mode: String,
    pub stt_model: String,
    pub llm_model: Option<String>,
    pub latency_ms: u64,
    /// Что лежало в буфере обмена до вставки — чтобы можно было вернуть вручную.
    pub clipboard_before: Option<String>,
    pub error: Option<String>,
}

impl Entry {
    /// Была ли пост-обработка, то есть отличается ли вставленное от распознанного.
    pub fn was_transformed(&self) -> bool {
        self.raw_text != self.final_text && !self.raw_text.is_empty()
    }
}

pub fn history_path() -> PathBuf {
    crate::config::config_dir().join("history.jsonl")
}

pub fn load(limit: usize) -> Vec<Entry> {
    let Ok(file) = std::fs::File::open(history_path()) else {
        return Vec::new();
    };
    let mut entries: Vec<Entry> = BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(&l).ok())
        .collect();
    // Новые сверху.
    entries.reverse();
    entries.truncate(limit);
    entries
}

pub fn append(entry: &Entry) -> Result<()> {
    let dir = crate::config::config_dir();
    std::fs::create_dir_all(&dir)?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(history_path())?;
    writeln!(file, "{}", serde_json::to_string(entry)?)?;
    Ok(())
}

pub fn clear() -> Result<()> {
    let path = history_path();
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

/// Обрезает файл до последних `limit` записей.
pub fn trim(limit: usize) -> Result<()> {
    let mut entries = load(usize::MAX);
    if entries.len() <= limit {
        return Ok(());
    }
    entries.truncate(limit);
    entries.reverse(); // обратно в хронологический порядок
    let mut out = String::new();
    for e in &entries {
        out.push_str(&serde_json::to_string(e)?);
        out.push('\n');
    }
    std::fs::write(history_path(), out)?;
    Ok(())
}
