//! Каталог локальных моделей и их загрузка.
//!
//! Модели весят сотни мегабайт, поэтому в бандл они не кладутся: приложение
//! качает выбранную модель в Application Support и держит её там.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
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
    /// GigaAM v3 e2e CTC через ONNX Runtime: только русский и английский,
    /// зато на русском заметно точнее Parakeet и сам расставляет знаки
    /// препинания. Считается своим кодом — см. `gigaam.rs`.
    GigaAm,
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
            Engine::GigaAm => "GigaAM (локально)",
            Engine::Llm => "Локальная языковая модель",
        }
    }

    pub fn is_local(self) -> bool {
        !matches!(self, Engine::Cloud)
    }

    pub const ALL: [Engine; 3] = [Engine::Cloud, Engine::GigaAm, Engine::Parakeet];
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
        id: "gigaam-v3-e2e-ctc-int8",
        engine: Engine::GigaAm,
        title: "GigaAM v3 e2e CTC (int8)",
        note: "Только русский и английский, зато на русском ошибается втрое реже \
               Parakeet и сама ставит знаки препинания. Самая быстрая из локальных.",
        files: &[
            ModelFile {
                name: "v3_e2e_ctc.int8.onnx",
                url: concat!(
                    "https://huggingface.co/istupakov/gigaam-v3-onnx/resolve/",
                    "322c3b29492673eb7d0b434bfa9dfb8653e34d02/v3_e2e_ctc.int8.onnx"
                ),
                size: 224_893_347,
            },
            ModelFile {
                name: "v3_e2e_ctc_vocab.txt",
                url: concat!(
                    "https://huggingface.co/istupakov/gigaam-v3-onnx/resolve/",
                    "322c3b29492673eb7d0b434bfa9dfb8653e34d02/v3_e2e_ctc_vocab.txt"
                ),
                size: 2_007,
            },
        ],
    },
    ModelSpec {
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
    },
];

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
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(download_async(spec, &progress))
}

/// Отмена проверяется и при отсутствии байтов от сервера. Таймаут считается
/// заново для каждого чтения, поэтому большая загрузка не ограничена целиком.
async fn interruptible<F: std::future::Future>(
    future: F,
    progress: &Progress,
    timeout: std::time::Duration,
) -> Result<F::Output> {
    let mut future = std::pin::pin!(future);
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if progress.cancel.load(Ordering::Relaxed) {
            bail!("загрузка отменена");
        }
        let remaining = deadline
            .checked_duration_since(std::time::Instant::now())
            .ok_or_else(|| anyhow!("сервер загрузки не передаёт данные"))?;
        let poll = remaining.min(std::time::Duration::from_millis(100));
        if let Ok(result) = tokio::time::timeout(poll, future.as_mut()).await {
            if progress.cancel.load(Ordering::Relaxed) {
                bail!("загрузка отменена");
            }
            return Ok(result);
        }
    }
}

async fn download_async(spec: &ModelSpec, progress: &Progress) -> Result<()> {
    let dir = spec.dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
    progress.total.store(spec.total_size(), Ordering::Relaxed);
    progress.downloaded.store(0, Ordering::Relaxed);
    let timeout = std::time::Duration::from_secs(30);
    let client = reqwest::Client::builder()
        .connect_timeout(timeout)
        .user_agent(concat!("LLM-dict/", env!("CARGO_PKG_VERSION")))
        .build()?;
    for file in spec.files {
        if progress.cancel.load(Ordering::Relaxed) {
            bail!("загрузка отменена");
        }
        let target = dir.join(file.name);
        let done = std::fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
        if file.size > 0 && done == file.size {
            progress.downloaded.fetch_add(file.size, Ordering::Relaxed);
            continue;
        }
        let mut resp = interruptible(client.get(file.url).send(), progress, timeout)
            .await??
            .error_for_status()?;
        // RAII удаляет неполный файл при отмене, таймауте и ошибке диска.
        let mut out = tempfile::NamedTempFile::new_in(&dir)?;
        let mut got = 0u64;
        while let Some(bytes) = interruptible(resp.chunk(), progress, timeout).await?? {
            got += bytes.len() as u64;
            if file.size > 0 && got > file.size {
                bail!("{}: сервер передал больше ожидаемого размера", file.name);
            }
            out.write_all(&bytes)?;
            progress
                .downloaded
                .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        }
        if file.size > 0 && got != file.size {
            bail!("{}: получено {} байт вместо {}", file.name, got, file.size);
        }
        out.as_file().sync_all()?;
        if progress.cancel.load(Ordering::Relaxed) {
            bail!("загрузка отменена");
        }
        out.persist(&target).map_err(|e| e.error)?;
    }
    if !spec.is_installed() {
        bail!("после загрузки часть файлов модели отсутствует");
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

    #[test]
    fn stalled_download_can_be_cancelled_or_time_out() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let progress = Arc::new(Progress::default());
        let cancel = progress.clone();
        let worker = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            cancel.cancel();
        });
        let error = rt
            .block_on(interruptible(
                std::future::pending::<()>(),
                &progress,
                std::time::Duration::from_secs(1),
            ))
            .unwrap_err();
        worker.join().unwrap();
        assert!(error.to_string().contains("отменена"));
        let error = rt
            .block_on(interruptible(
                std::future::pending::<()>(),
                &Progress::default(),
                std::time::Duration::from_millis(10),
            ))
            .unwrap_err();
        assert!(error.to_string().contains("не передаёт"));
    }

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
    fn гигаам_читается_из_настроек() {
        let engine: Engine = serde_json::from_str("\"GigaAm\"").unwrap();
        assert_eq!(engine, Engine::GigaAm);
    }

    /// У каждого локального движка своя загрузка, и модель одного движка в
    /// другом не заработает: перепутанный движок в каталоге означал бы
    /// падение при загрузке.
    #[test]
    fn модель_гигаам_числится_за_своим_движком() {
        let spec = find("gigaam-v3-e2e-ctc-int8").expect("модели нет в каталоге");
        assert_eq!(spec.engine, Engine::GigaAm);
        let names: Vec<&str> = spec.files.iter().map(|f| f.name).collect();
        assert!(
            names.contains(&crate::gigaam::MODEL_FILE),
            "нет файла модели"
        );
        assert!(names.contains(&crate::gigaam::VOCAB_FILE), "нет словаря");
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
