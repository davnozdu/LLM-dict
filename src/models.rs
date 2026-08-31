//! Каталог локальных моделей и их загрузка.
//!
//! Модели весят сотни мегабайт, поэтому в бандл они не кладутся: приложение
//! качает выбранную модель в Application Support и держит её там.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

/// Какой движок распознаёт речь.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Engine {
    /// OpenAI-совместимое облако: Groq и всё, что говорит на том же протоколе.
    Cloud,
    /// NVIDIA Parakeet TDT через ONNX Runtime.
    Parakeet,
    /// whisper.cpp с Metal.
    Whisper,
}

impl Engine {
    pub fn label(self) -> &'static str {
        match self {
            Engine::Cloud => "Groq (облако)",
            Engine::Parakeet => "Parakeet (локально)",
            Engine::Whisper => "Whisper (локально)",
        }
    }

    pub fn is_local(self) -> bool {
        !matches!(self, Engine::Cloud)
    }

    pub const ALL: [Engine; 3] = [Engine::Cloud, Engine::Parakeet, Engine::Whisper];
}

pub struct ModelFile {
    pub name: &'static str,
    pub url: &'static str,
    /// Ожидаемый размер. Нужен и для прогресса, и чтобы отличить докачанный
    /// файл от оборванного.
    pub size: u64,
}

pub struct ModelSpec {
    pub id: &'static str,
    pub engine: Engine,
    pub title: &'static str,
    pub note: &'static str,
    pub files: &'static [ModelFile],
}

impl ModelSpec {
    pub fn total_size(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }

    pub fn dir(&self) -> PathBuf {
        models_dir().join(self.id)
    }

    /// Модель считается установленной, когда все файлы на месте и нужного
    /// размера: оборванная закачка иначе выглядела бы как готовая.
    pub fn is_installed(&self) -> bool {
        self.files.iter().all(|f| {
            std::fs::metadata(self.dir().join(f.name))
                .map(|m| m.len() == f.size)
                .unwrap_or(false)
        })
    }

    pub fn remove(&self) -> Result<()> {
        let dir = self.dir();
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        Ok(())
    }
}

pub static CATALOG: &[ModelSpec] = &[
    ModelSpec {
        id: "parakeet-tdt-0.6b-v3-int8",
        engine: Engine::Parakeet,
        title: "Parakeet TDT 0.6B v3 (int8)",
        note: "25 языков с автоопределением, включая русский. \
               Считает только реальную длину записи, поэтому на коротких фразах быстрее Whisper.",
        files: &[
            ModelFile {
                name: "encoder-model.int8.onnx",
                url: concat!(
                    "https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/main",
                    "/encoder-model.int8.onnx"
                ),
                size: 652_183_999,
            },
            ModelFile {
                name: "decoder_joint-model.int8.onnx",
                url: concat!(
                    "https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/main",
                    "/decoder_joint-model.int8.onnx"
                ),
                size: 18_202_004,
            },
            ModelFile {
                name: "vocab.txt",
                url: concat!(
                    "https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/main",
                    "/vocab.txt"
                ),
                size: 93_939,
            },
        ],
    },
    ModelSpec {
        id: "whisper-large-v3-turbo-q5",
        engine: Engine::Whisper,
        title: "Whisper large-v3-turbo (q5_0)",
        note: "Урезанный декодер вместо полного: заметно быстрее large-v3 при близкой точности. \
               Разумный выбор, если локальный движок нужен как запасной.",
        files: &[ModelFile {
            name: "ggml-large-v3-turbo-q5_0.bin",
            url: concat!(
                "https://huggingface.co/ggerganov/whisper.cpp/resolve/main",
                "/ggml-large-v3-turbo-q5_0.bin"
            ),
            size: 574_041_195,
        }],
    },
    ModelSpec {
        id: "whisper-large-v3-q5",
        engine: Engine::Whisper,
        title: "Whisper large-v3 (q5_0)",
        note: "Самая точная из локальных, особенно на шумной записи. \
               И самая медленная: на диктовке пауза заметна.",
        files: &[ModelFile {
            name: "ggml-large-v3-q5_0.bin",
            url: concat!(
                "https://huggingface.co/ggerganov/whisper.cpp/resolve/main",
                "/ggml-large-v3-q5_0.bin"
            ),
            size: 1_081_140_203,
        }],
    },
];

pub fn find(id: &str) -> Option<&'static ModelSpec> {
    CATALOG.iter().find(|m| m.id == id)
}

pub fn models_dir() -> PathBuf {
    crate::config::config_dir().join("models")
}

/// Состояние закачки, за которым следит интерфейс.
#[derive(Default)]
pub struct Progress {
    pub downloaded: AtomicU64,
    pub total: AtomicU64,
    pub cancel: AtomicBool,
}

impl Progress {
    pub fn fraction(&self) -> f32 {
        let total = self.total.load(Ordering::Relaxed);
        if total == 0 {
            return 0.0;
        }
        self.downloaded.load(Ordering::Relaxed) as f32 / total as f32
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Качает недостающие файлы модели. Уже скачанные пропускает, оборванные
/// перекачивает целиком — докачка по диапазонам у зеркал HuggingFace
/// работает не всегда, а тихо получить обрезанный файл хуже, чем подождать.
pub fn download(spec: &ModelSpec, progress: Arc<Progress>) -> Result<()> {
    let dir = spec.dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;

    progress.total.store(spec.total_size(), Ordering::Relaxed);
    progress.downloaded.store(0, Ordering::Relaxed);

    let client = reqwest::blocking::Client::builder()
        .timeout(None)
        .connect_timeout(std::time::Duration::from_secs(30))
        .user_agent(concat!("LLM-dict/", env!("CARGO_PKG_VERSION")))
        .build()?;

    for file in spec.files {
        let target = dir.join(file.name);
        let done = std::fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
        if file.size > 0 && done == file.size {
            progress.downloaded.fetch_add(file.size, Ordering::Relaxed);
            continue;
        }

        let tmp = target.with_extension("part");
        let mut resp = client.get(file.url).send()?.error_for_status()?;
        let mut out =
            std::fs::File::create(&tmp).with_context(|| format!("создать {}", tmp.display()))?;

        let mut buf = vec![0u8; 1 << 20];
        loop {
            if progress.cancel.load(Ordering::Relaxed) {
                drop(out);
                let _ = std::fs::remove_file(&tmp);
                bail!("загрузка отменена");
            }
            let n = resp.read(&mut buf)?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])?;
            progress.downloaded.fetch_add(n as u64, Ordering::Relaxed);
        }
        out.flush()?;
        drop(out);

        let got = std::fs::metadata(&tmp)?.len();
        if file.size > 0 && got != file.size {
            let _ = std::fs::remove_file(&tmp);
            bail!("{}: получено {} байт вместо {}", file.name, got, file.size);
        }
        std::fs::rename(&tmp, &target)?;
    }

    if !spec.is_installed() {
        return Err(anyhow!("после загрузки часть файлов модели отсутствует"));
    }
    Ok(())
}

pub fn human_size(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    let mb = bytes as f64 / MB;
    if mb >= 1024.0 {
        format!("{:.1} ГБ", mb / 1024.0)
    } else {
        format!("{mb:.0} МБ")
    }
}
