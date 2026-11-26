//! Исправление ошибок в русском тексте GigaAM.
//!
//! Модель — дообученная `sage-fredt5-distilled-95m` (T5, 95M параметров),
//! экспортированная в ONNX INT8: `davnozdubv/sage-ru-dictation-onnx`. Она
//! переписывает фразу целиком, поэтому её ответ проходит через
//! [`crate::text_guard::merge`]: принимаются только знаки, регистр, окончания,
//! опечатки и названия латиницей. Без скачанной модели остаются заглавные буквы
//! в начале предложений.

use anyhow::{Context, Result};
use ort::{session::Session, value::Tensor};
use std::path::Path;
use tokenizers::Tokenizer;

pub const ENCODER: &str = "encoder_model.int8.onnx";
pub const DECODER: &str = "decoder_model.int8.onnx";

const DECODER_START: i64 = 0;
const EOS: u32 = 2;
/// Модель училась на кусках до 160 токенов: длинная диктовка режется по
/// предложениям, примерно по 30–60 слов.
const CHUNK_MIN_WORDS: usize = 30;
const CHUNK_MAX_WORDS: usize = 60;
const MAX_INPUT_TOKENS: usize = 400;

pub struct Formatter {
    tokenizer: Tokenizer,
    encoder: Session,
    decoder: Session,
}

#[derive(Clone, Copy, Default)]
struct Prediction {
    case: Option<usize>,
    mark: Option<&'static str>,
}

impl Formatter {
    pub fn load(dir: &Path) -> Result<Self> {
        let tokenizer = Tokenizer::from_file(dir.join("tokenizer.json"))
            .map_err(|e| anyhow::anyhow!("токенизатор модели правки: {e}"))?;
        let session = |name: &str| -> Result<Session> {
            Ok(Session::builder()?
                .with_intra_threads(4)
                .map_err(|e| anyhow::anyhow!("настройка потоков модели правки: {e}"))?
                .commit_from_file(dir.join(name))?)
        };
        Ok(Self {
            tokenizer,
            encoder: session(ENCODER)?,
            decoder: session(DECODER)?,
        })
    }

    pub fn format(&mut self, text: &str) -> Result<String> {
        if text.trim().is_empty() {
            return Ok(text.to_owned());
        }
        let mut out = Vec::new();
        for chunk in chunks(text) {
            let fixed = self.correct(&chunk)?;
            out.push(crate::text_guard::merge(&chunk, &fixed));
        }
        Ok(out.join(" "))
    }

    /// Жадное декодирование T5. Экспорт без кэша ключей, поэтому декодер на
    /// каждом шаге получает всю выданную последовательность — на фразах
    /// диктовки это 50–200 мс.
    fn correct(&mut self, text: &str) -> Result<String> {
        let encoding = self
            .tokenizer
            .encode(text, true)
            .map_err(|e| anyhow::anyhow!("токенизация модели правки: {e}"))?;
        let ids: Vec<i64> = encoding.get_ids().iter().map(|&id| i64::from(id)).collect();
        let len = ids.len();
        if len == 0 || len > MAX_INPUT_TOKENS {
            return Ok(text.to_owned());
        }
        let mask = vec![1_i64; len];
        let (hidden, width) = {
            let outputs = self.encoder.run(ort::inputs![
                "input_ids" => Tensor::from_array(([1, len], ids))?,
                "attention_mask" => Tensor::from_array(([1, len], mask.clone()))?
            ])?;
            let (shape, hidden) = outputs["last_hidden_state"].try_extract_tensor::<f32>()?;
            let width = *shape.get(2).context("энкодер вернул неожиданную форму")? as usize;
            (hidden.to_vec(), width)
        };

        let mut decoded = vec![DECODER_START];
        for _ in 0..(2 * len + 16).min(256) {
            let steps = decoded.len();
            let outputs = self.decoder.run(ort::inputs![
                "decoder_input_ids" => Tensor::from_array(([1, steps], decoded.clone()))?,
                "encoder_hidden_states" => Tensor::from_array(([1, len, width], hidden.clone()))?,
                "encoder_attention_mask" => Tensor::from_array(([1, len], mask.clone()))?
            ])?;
            let (shape, logits) = outputs["logits"].try_extract_tensor::<f32>()?;
            let vocab = *shape.get(2).context("декодер вернул неожиданную форму")? as usize;
            let last = logits
                .get((steps - 1) * vocab..steps * vocab)
                .context("декодер вернул неожиданный размер")?;
            // При равенстве берём первый, как numpy.argmax в замерах.
            let next = last
                .iter()
                .enumerate()
                .fold(0, |best, (i, v)| if *v > last[best] { i } else { best })
                as u32;
            if next == EOS {
                break;
            }
            decoded.push(i64::from(next));
        }
        let ids: Vec<u32> = decoded[1..].iter().map(|&id| id as u32).collect();
        let text = self
            .tokenizer
            .decode(&ids, true)
            .map_err(|e| anyhow::anyhow!("сборка текста модели правки: {e}"))?;
        Ok(text.trim().to_owned())
    }
}

/// Режет текст на куски по концам предложений, не короче 30 и не длиннее 60 слов.
fn chunks(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for word in text.split_whitespace() {
        current.push(word);
        let sentence_end = word
            .trim_end_matches(['"', '»', ')'])
            .ends_with(['.', '?', '!', '…']);
        if (sentence_end && current.len() >= CHUNK_MIN_WORDS) || current.len() >= CHUNK_MAX_WORDS {
            out.push(current.join(" "));
            current.clear();
        }
    }
    if !current.is_empty() {
        out.push(current.join(" "));
    }
    out
}

pub fn is_russian_letter(c: char) -> bool {
    matches!(c, 'а'..='я' | 'А'..='Я' | 'ё' | 'Ё')
}

fn split_mark(word: &str) -> (&str, &str) {
    let core = word.trim_end_matches(['.', ',', '?', '!', ';', ':', '…']);
    (core, &word[core.len()..])
}

fn ends_sentence(word: &str) -> bool {
    let word = word.trim_end_matches(['"', '»', '”', '\'', ')', ']', '}']);
    let (core, mark) = split_mark(word);
    word.ends_with(['.', '?', '!', '…']) && !is_abbreviation(core, mark)
}

fn is_abbreviation(core: &str, mark: &str) -> bool {
    mark == "."
        && (matches!(
            core.to_lowercase().as_str(),
            "г" | "ул" | "им" | "стр" | "руб" | "коп"
        ) || core.contains('.')
            && core
                .split('.')
                .all(|part| part.chars().count() == 1 && part.chars().all(is_russian_letter)))
}

pub fn capitalize_sentences(text: &str) -> String {
    render(text, &[])
}

fn render(text: &str, predictions: &[Prediction]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut remaining = text;
    let mut sentence_start = true;
    let mut after_comma = false;
    let mut joined_sentence = false;
    for (index, word) in text.split_whitespace().enumerate() {
        let offset = remaining.find(word).expect("слово из исходного текста");
        out.push_str(&remaining[..offset]);
        remaining = &remaining[offset + word.len()..];

        let (core, old_mark) = split_mark(word);
        let letters = core.trim_start_matches(['«', '“', '"', '(', '[']);
        let russian = !letters.is_empty() && letters.chars().all(is_russian_letter);
        let prediction = predictions.get(index).copied().unwrap_or_default();
        let mut mark = old_mark;
        let multi_letter = letters.chars().count() > 1;
        if russian
            && multi_letter
            && matches!(old_mark, "" | "," | ".")
            && !is_abbreviation(letters, old_mark)
        {
            if let Some(new) = prediction
                .mark
                .filter(|&new| new != "-" && (!new.is_empty() || old_mark == ","))
            {
                mark = new;
            }
        }

        let mut chars = letters.chars();
        let first = chars.next();
        if let Some(first) = first.filter(|_| russian && chars.clone().all(char::is_lowercase)) {
            out.push_str(&core[..core.len() - letters.len()]);
            if sentence_start || (multi_letter && !after_comma && prediction.case == Some(0)) {
                out.extend(first.to_uppercase());
            } else if (multi_letter && prediction.case == Some(1))
                || (joined_sentence && matches!(letters, "А" | "И"))
            {
                out.extend(first.to_lowercase());
            } else {
                out.push(first);
            }
            out.push_str(chars.as_str());
        } else {
            out.push_str(core);
        }
        out.push_str(mark);
        if core.chars().any(char::is_alphanumeric) || ends_sentence(mark) {
            sentence_start = ends_sentence(if mark == old_mark { word } else { mark });
        }
        after_comma = mark == ",";
        joined_sentence = old_mark == "." && mark == ",";
    }
    out.push_str(remaining);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_numbers_spacing_and_embedded_product_names() {
        let input = "привет!  записал в GitHub в 15:30 на 2 350,75₽. готово";
        assert_eq!(
            capitalize_sentences(input),
            "Привет!  Записал в GitHub в 15:30 на 2 350,75₽. Готово"
        );
        assert_eq!(
            capitalize_sentences("адрес в г. Брно и т.д. сохранён. проверь"),
            "Адрес в г. Брно и т.д. сохранён. Проверь"
        );
        let predicted = Prediction {
            mark: Some(","),
            case: Some(0),
        };
        assert_eq!(
            render("проверил GitHub в 15:30, ок?", &[predicted; 5]),
            "Проверил, GitHub в 15:30, ок?"
        );
    }

    #[test]
    fn updates_existing_marks_and_case_without_changing_words() {
        let predictions = [
            Prediction {
                mark: Some(","),
                case: Some(1),
            },
            Prediction {
                mark: Some("."),
                case: Some(1),
            },
            Prediction {
                mark: Some(","),
                case: Some(1),
            },
            Prediction {
                mark: Some(""),
                case: Some(1),
            },
        ];
        assert_eq!(
            render("слушай подожди готово МОСКВА?", &predictions),
            "Слушай, подожди. Готово, МОСКВА?"
        );
        assert_eq!(
            render("Проверь. А потом запиши.", &predictions[..1]),
            "Проверь, а потом запиши."
        );
    }

    #[test]
    #[ignore = "нужна скачанная модель правки (CORRECTOR_DIR) и эталон из speechcorrector (CORRECTOR_EXPECTED)"]
    fn matches_python_reference() {
        // CORRECTOR_EXPECTED — autotrain/results/<модель>.history.json: исходник и
        // результат Python-версии с той же защитой.
        let dir = std::env::var_os("CORRECTOR_DIR").unwrap();
        let expected =
            std::fs::read_to_string(std::env::var_os("CORRECTOR_EXPECTED").unwrap()).unwrap();
        let rows: Vec<serde_json::Value> = serde_json::from_str(&expected).unwrap();
        let mut formatter = Formatter::load(Path::new(&dir)).unwrap();
        let mut mismatches = 0;
        for row in &rows {
            let raw = row["raw"].as_str().unwrap();
            if raw.split_whitespace().count() > CHUNK_MIN_WORDS {
                continue; // Python-замер не режет текст на куски
            }
            let got = formatter.format(raw).unwrap();
            if got != row["guarded"].as_str().unwrap() {
                mismatches += 1;
                eprintln!("IN : {raw}\nRS : {got}\nPY : {}\n", row["guarded"]);
            }
        }
        assert_eq!(mismatches, 0);
    }
}
