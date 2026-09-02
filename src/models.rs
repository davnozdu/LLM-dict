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
    ///
    /// Псевдоним `Whisper` оставлен намеренно: локального whisper.cpp больше
    /// нет, и без него настройка со старым значением не прочиталась бы —
    /// сбросив заодно всё остальное. Такие конфиги молча переезжают сюда.
    #[serde(alias = "Whisper")]
    Parakeet,
    /// Локальная языковая модель через llama.cpp. Не распознаёт речь —
    /// обрабатывает уже распознанный текст, поэтому в списке движков
    /// распознавания (`ALL`) её нет.
    Llm,
}

impl Engine {
    pub fn label(self) -> &'static str {
        match self {
            Engine::Cloud => "Groq (облако)",
            Engine::Parakeet => "Parakeet (локально)",
            Engine::Llm => "Локальная языковая модель",
        }
    }

    pub fn is_local(self) -> bool {
        !matches!(self, Engine::Cloud)
    }

    pub const ALL: [Engine; 2] = [Engine::Cloud, Engine::Parakeet];
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

pub static CATALOG: &[ModelSpec] = &[ModelSpec {
    id: "parakeet-tdt-0.6b-v3-int8",
    engine: Engine::Parakeet,
    title: "Parakeet TDT 0.6B v3 (int8)",
    note: "25 языков с автоопределением, включая русский. \
               Считает только реальную длину записи, поэтому на коротких фразах отвечает быстро.",
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
}];

/// Языковые модели для локальной обработки текста.
///
/// Отобраны замером на реальных диктовках: в каталог попали только те, что
/// в среднем приближают текст к результату облачной модели. Проверенные и
/// отвергнутые — Gemma 3, EuroLLM, Ministral — портили текст чаще, чем
/// улучшали, и здесь их нет намеренно.
pub static LLM_CATALOG: &[ModelSpec] = &[
    ModelSpec {
        id: "gemma-4-e2b-q4",
        engine: Engine::Llm,
        title: "Gemma 4 E2B (Q4_K_M)",
        note: "Лучшая по качеству правки: на замере десять реплик из сорока шести \
               исправила ровно так же, как облачная модель. Около 3 ГБ памяти.",
        files: &[ModelFile {
            name: "gemma-4-e2b-q4.gguf",
            url: concat!(
                "https://huggingface.co/unsloth/gemma-4-E2B-it-GGUF/resolve/",
                "0314792d7f1f7e229411f620751375812bb9faf2/gemma-4-E2B-it-Q4_K_M.gguf"
            ),
            size: 3_106_738_272,
        }],
    },
    ModelSpec {
        id: "qwen3-4b-q4",
        engine: Engine::Llm,
        title: "Qwen3 4B (Q4_K_M)",
        note: "Чуть слабее Gemma 4 и вдвое медленнее, но осторожнее: реже меняет \
               то, что менять не следовало. Около 2.8 ГБ памяти.",
        files: &[ModelFile {
            name: "qwen3-4b-q4.gguf",
            url: concat!(
                "https://huggingface.co/unsloth/Qwen3-4B-GGUF/resolve/",
                "22c9fc8a8c7700b76a1789366280a6a5a1ad1120/Qwen3-4B-Q4_K_M.gguf"
            ),
            size: 2_497_281_312,
        }],
    },
    ModelSpec {
        id: "qwen3-1.7b-q6",
        engine: Engine::Llm,
        title: "Qwen3 1.7B (Q6_K)",
        note: "Для машин, где памяти жалко: около 1.9 ГБ. Правит заметно меньше, \
               зато почти не портит.",
        files: &[ModelFile {
            name: "qwen3-1.7b-q6.gguf",
            url: concat!(
                "https://huggingface.co/unsloth/Qwen3-1.7B-GGUF/resolve/",
                "d7f544eead698dbd1f15126ef60b45a1e1933222/Qwen3-1.7B-Q6_K.gguf"
            ),
            size: 1_417_755_200,
        }],
    },
];

pub fn find(id: &str) -> Option<&'static ModelSpec> {
    CATALOG.iter().chain(LLM_CATALOG).find(|m| m.id == id)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Настройки, сделанные до удаления whisper.cpp, должны читаться.
    ///
    /// Без псевдонима serde отказался бы разобрать `"Whisper"`, и приложение
    /// молча сбросило бы все настройки к значениям по умолчанию — вместе с
    /// сочетаниями клавиш и действиями.
    #[test]
    fn старый_выбор_whisper_переезжает_на_parakeet() {
        let engine: Engine = serde_json::from_str("\"Whisper\"").expect("должно читаться");
        assert_eq!(engine, Engine::Parakeet);
    }

    #[test]
    fn паракит_читается_как_прежде() {
        let engine: Engine = serde_json::from_str("\"Parakeet\"").unwrap();
        assert_eq!(engine, Engine::Parakeet);
    }

    /// Идентификаторы в каталоге уникальны: по ним ищется папка модели,
    /// и совпадение означало бы, что две модели пишут в одно место.
    #[test]
    fn идентификаторы_моделей_уникальны() {
        let mut ids: Vec<&str> = CATALOG.iter().chain(LLM_CATALOG).map(|m| m.id).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "в каталоге повторяются идентификаторы");
    }
}
