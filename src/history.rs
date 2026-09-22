//! История диктовок. Хранится построчным JSON рядом с конфигом —
//! файл читается глазами и правится руками, без БД.

use anyhow::Result;
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::io::{BufReader, Write};
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
    /// Какой движок распознал. Важно, когда сработал откат на запасной.
    #[serde(default)]
    pub engine: Option<String>,
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
    crate::persistence::recent_json(BufReader::new(file), limit)
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

/// Writes the already bounded in-memory snapshot, newest first.
pub fn replace(entries: &[std::sync::Arc<Entry>]) -> Result<()> {
    let mut out = String::new();
    for e in entries.iter().rev() {
        out.push_str(&serde_json::to_string(e.as_ref())?);
        out.push('\n');
    }
    crate::persistence::atomic_write(&history_path(), out.as_bytes())?;
    Ok(())
}
