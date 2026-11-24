//! Восстановление регистра и пунктуации в русском тексте GigaAM.

use anyhow::{bail, Context, Result};
use ort::{session::Session, value::Tensor};
use std::path::Path;
use tokenizers::Tokenizer;

pub const MODEL: &str = "rupunct_small_int8.onnx";

// Порядок классов в config.json закреплённой версии модели. Каждому
// знаку соответствуют три класса регистра: начальный верхний, нижний, верхний.
const MARKS: [&str; 11] = [".", ",", "?", " -", "!", ":", ";", "-", "?!", "...", ""];
const CASE_CONFIDENCE: f32 = 0.97;
const MARK_CONFIDENCE: f32 = 0.95;
const WINDOW: usize = 96;
const CONTEXT: usize = 16;

pub struct Formatter {
    tokenizer: Tokenizer,
    session: Session,
}

#[derive(Clone, Copy, Default)]
struct Prediction {
    case: Option<usize>,
    mark: Option<&'static str>,
}

impl Formatter {
    pub fn load(dir: &Path) -> Result<Self> {
        let tokenizer = Tokenizer::from_file(dir.join("tokenizer.json"))
            .map_err(|e| anyhow::anyhow!("токенизатор RUPunct: {e}"))?;
        let session = Session::builder()?
            .with_intra_threads(2)
            .map_err(|e| anyhow::anyhow!("настройка потоков RUPunct: {e}"))?
            .commit_from_file(dir.join(MODEL))?;
        Ok(Self { tokenizer, session })
    }

    pub fn format(&mut self, text: &str) -> Result<String> {
        let words: Vec<String> = text
            .split_whitespace()
            .map(|word| {
                let (core, _) = split_mark(word);
                if core.is_empty() { word } else { core }.to_lowercase()
            })
            .collect();
        let mut predictions = vec![Prediction::default(); words.len()];
        for start in (0..words.len()).step_by(WINDOW) {
            let end = (start + WINDOW).min(words.len());
            let left = start.saturating_sub(CONTEXT);
            let right = (end + CONTEXT).min(words.len());
            self.predict(
                &words[left..right],
                start - left,
                &mut predictions[start..end],
            )?;
        }
        Ok(render(text, &predictions))
    }

    fn predict(
        &mut self,
        words: &[String],
        context: usize,
        result: &mut [Prediction],
    ) -> Result<()> {
        let encoding = self
            .tokenizer
            .encode(words, true)
            .map_err(|e| anyhow::anyhow!("токенизация RUPunct: {e}"))?;
        let len = encoding.len();
        if len > 2048 {
            return Ok(());
        }
        let tensor = |values: &[u32]| {
            Tensor::from_array((
                [1, len],
                values.iter().map(|&id| i64::from(id)).collect::<Vec<_>>(),
            ))
        };
        let outputs = self.session.run(ort::inputs![
            "input_ids" => tensor(encoding.get_ids())?,
            "attention_mask" => tensor(encoding.get_attention_mask())?,
            "token_type_ids" => tensor(encoding.get_type_ids())?
        ])?;
        let (_, logits) = outputs
            .get("logits")
            .context("RUPunct не вернул logits")?
            .try_extract_tensor::<f32>()?;
        let classes = MARKS.len() * 3;
        if logits.len() != len * classes {
            bail!("неожиданный размер выхода RUPunct");
        }

        let mut previous = None;
        for (word, scores) in encoding
            .get_word_ids()
            .iter()
            .copied()
            .zip(logits.chunks_exact(classes))
        {
            // Для каждого слова модель размечает первый WordPiece.
            if word == previous {
                continue;
            }
            previous = word;
            let index = word.and_then(|id| (id as usize).checked_sub(context));
            if let Some(slot) = index.and_then(|index| result.get_mut(index)) {
                *slot = decode(scores);
            }
        }
        Ok(())
    }
}

fn decode(scores: &[f32]) -> Prediction {
    let max = scores.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let mut cases = [0.0; 3];
    let mut marks = [0.0; MARKS.len()];
    for (index, score) in scores.iter().enumerate() {
        let weight = (score - max).exp();
        cases[index % cases.len()] += weight;
        marks[index / cases.len()] += weight;
    }
    let total = cases.iter().sum::<f32>();
    let best = |weights: &[f32], threshold: f32| {
        weights.iter().position(|&value| value / total >= threshold)
    };
    Prediction {
        case: best(&cases, CASE_CONFIDENCE),
        mark: best(&marks, MARK_CONFIDENCE).map(|index| MARKS[index]),
    }
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
    #[ignore = "нужны скачанные файлы RUPunct, каталог задаёт RUPUNCT_DIR"]
    fn formats_real_russian_text() {
        let dir = std::env::var_os("RUPUNCT_DIR").unwrap();
        let mut formatter = Formatter::load(Path::new(&dir)).unwrap();
        assert_eq!(
            formatter
                .format("я знаю что делать. потом проверю конфигурацию")
                .unwrap(),
            "Я знаю, что делать. Потом проверю конфигурацию."
        );
        assert_eq!(
            formatter
                .format("завтра Мы проверим обновление. потом отправим ответ")
                .unwrap(),
            "Завтра мы проверим обновление. Потом отправим ответ."
        );
    }
}
