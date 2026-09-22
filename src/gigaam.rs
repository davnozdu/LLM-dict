//! Локальное распознавание GigaAM v3 e2e CTC через ONNX Runtime.
//!
//! Модель отдаёт сразу оформленный текст: с пунктуацией, заглавными буквами
//! и числами цифрами. Отдельного прохода языковой моделью ради знаков
//! препинания она не требует.

use anyhow::{bail, Context, Result};
use ort::session::Session;
use ort::value::Tensor;
use std::path::Path;

/// Файлы модели в папке, куда её скачал `models::download`.
pub const MODEL_FILE: &str = "v3_e2e_ctc.int8.onnx";
pub const VOCAB_FILE: &str = "v3_e2e_ctc_vocab.txt";

/// Препроцессор из onnx-asr: считает те самые лог-мел-признаки, на которых
/// модель обучалась. Лежит в бинарнике — 44 КБ, а свой расчёт мел-фильтров
/// был бы лишним риском. Происхождение и лицензия — в `assets/README.md`.
const PREPROCESSOR: &[u8] = include_bytes!("../assets/gigaam_v3.onnx");

/// Меньше одного окна анализа препроцессор не переварит.
const MIN_SAMPLES: usize = 320;

pub struct GigaAm {
    preprocessor: Session,
    model: Session,
    vocab: Vocab,
}

/// Словарь модели: токен по индексу плюс индекс пустого кадра.
pub struct Vocab {
    pub tokens: Vec<String>,
    pub blank: usize,
}

/// Разбирает словарь формата «токен пробел индекс».
///
/// `▁` в начале токена у SentencePiece означает границу слова, поэтому
/// заменяется на пробел прямо при чтении — дальше словарь хранит уже готовые
/// куски текста.
pub fn parse_vocab(text: &str) -> Result<Vocab> {
    let mut tokens = Vec::new();
    let mut blank = None;
    for line in text.lines().filter(|l| !l.is_empty()) {
        // Индекс отделяется последним пробелом: сам токен пробел содержать
        // может, а вот разрезание по первому его бы испортило.
        let (token, id) = line
            .rsplit_once(' ')
            .ok_or_else(|| anyhow::anyhow!("строка словаря без индекса: {line}"))?;
        let id: usize = id
            .parse()
            .with_context(|| format!("нечисловой индекс в словаре: {line}"))?;
        if id != tokens.len() {
            bail!(
                "словарь идёт не подряд: после {} ожидался {id}",
                tokens.len()
            );
        }
        if token == "<blk>" {
            blank = Some(id);
        }
        tokens.push(token.replace('\u{2581}', " "));
    }
    let blank = blank.ok_or_else(|| anyhow::anyhow!("в словаре нет токена <blk>"))?;
    Ok(Vocab { tokens, blank })
}

/// Жадный разбор выхода CTC: по кадру берётся самый вероятный класс, повторы
/// схлопываются, пустые кадры выбрасываются.
pub fn greedy_ctc(log_probs: &[f32], classes: usize, blank: usize) -> Vec<usize> {
    let mut out = Vec::new();
    let mut prev = blank;
    for frame in log_probs.chunks_exact(classes) {
        let best = frame
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.total_cmp(b))
            .map(|(i, _)| i)
            .unwrap_or(blank);
        if best != blank && best != prev {
            out.push(best);
        }
        prev = best;
    }
    out
}

/// Склеивает токены в текст.
///
/// Служебный `<unk>` выбрасывается: им модель помечает то, чего не разобрала,
/// и в тексте у пользователя ему делать нечего.
///
/// В словаре пробел приклеен к началу слова, поэтому перед знаком препинания
/// он оказывается лишним: «мир .» вместо «мир.». Лишние пробелы убираются,
/// но перед открывающей кавычкой или скобкой пробел остаётся — иначе она
/// прилипает к предыдущему слову.
pub fn assemble(tokens: &[&str]) -> String {
    const OPENERS: [char; 7] = ['«', '„', '“', '"', '(', '[', '{'];

    let joined: String = tokens.iter().filter(|t| **t != "<unk>").copied().collect();
    let mut out = String::with_capacity(joined.len());
    let mut chars = joined.chars().peekable();
    while let Some(c) = chars.next() {
        if c == ' ' {
            let keep = matches!(chars.peek(), Some(&next) if next.is_alphanumeric() || next == '_' || OPENERS.contains(&next));
            if !keep || out.is_empty() {
                continue;
            }
        }
        out.push(c);
    }
    out
}

impl GigaAm {
    /// Загружает модель из папки, куда её скачал каталог.
    pub fn load(dir: &Path) -> Result<Self> {
        let vocab_path = dir.join(VOCAB_FILE);
        let vocab = parse_vocab(
            &std::fs::read_to_string(&vocab_path)
                .with_context(|| format!("не прочитать {}", vocab_path.display()))?,
        )?;

        let preprocessor = Session::builder()?
            .commit_from_memory(PREPROCESSOR)
            .context("не собрать препроцессор GigaAM")?;
        let model_path = dir.join(MODEL_FILE);
        let model = Session::builder()?
            .commit_from_file(&model_path)
            .with_context(|| format!("не загрузить {}", model_path.display()))?;

        Ok(Self {
            preprocessor,
            model,
            vocab,
        })
    }

    /// Распознаёт запись 16 кГц, моно.
    pub fn transcribe(&mut self, samples: &[f32]) -> Result<String> {
        if samples.len() < MIN_SAMPLES {
            bail!("запись слишком короткая для распознавания");
        }

        // Признаки считает отдельная сессия, поэтому её выход надо скопировать:
        // дальше заимствование препроцессора закончится, а данные нужны.
        let (features, feature_lengths) = {
            let samples_len = samples.len() as i64;
            let outputs = self.preprocessor.run(ort::inputs![
                "waveforms" => Tensor::from_array((vec![1, samples_len], samples.to_vec()))?,
                "waveforms_lens" => Tensor::from_array((vec![1], vec![samples_len]))?
            ])?;
            let (shape, data) = outputs["features"].try_extract_tensor::<f32>()?;
            let (_, lens) = outputs["features_lens"].try_extract_tensor::<i64>()?;
            (
                Tensor::from_array((shape.to_vec(), data.to_vec()))?,
                Tensor::from_array((vec![1], lens.to_vec()))?,
            )
        };

        let outputs = self.model.run(ort::inputs![
            "features" => features,
            "feature_lengths" => feature_lengths
        ])?;
        let (shape, log_probs) = outputs["log_probs"].try_extract_tensor::<f32>()?;
        let classes = *shape
            .last()
            .ok_or_else(|| anyhow::anyhow!("модель вернула тензор без размерности"))?
            as usize;

        let ids = greedy_ctc(log_probs, classes, self.vocab.blank);
        let tokens: Vec<&str> = ids
            .iter()
            .map(|&id| self.vocab.tokens[id].as_str())
            .collect();
        Ok(assemble(&tokens))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn словарь_читается_с_подчёркиванием_как_пробелом() {
        let v = parse_vocab("<unk> 0\n▁ 1\n▁и 2\n. 3\n<blk> 4\n").unwrap();
        assert_eq!(v.tokens[1], " ");
        assert_eq!(v.tokens[2], " и");
        assert_eq!(v.tokens[3], ".");
        assert_eq!(v.blank, 4, "пустой кадр ищется по токену <blk>");
    }

    /// Словарь индексируется напрямую номером класса, поэтому дыры в
    /// нумерации превратили бы соседние токены в чужие буквы.
    #[test]
    fn словарь_с_пропущенным_индексом_отвергается() {
        assert!(parse_vocab("а 0\nб 2\n<blk> 3\n").is_err());
    }

    #[test]
    fn повторы_схлопываются_а_пустые_кадры_выбрасываются() {
        // Три класса, пустой — второй. Кадры: а, а, пусто, а, б.
        let frames = [
            [0.9, 0.1, 0.0],
            [0.9, 0.1, 0.0],
            [0.0, 0.1, 0.9],
            [0.9, 0.1, 0.0],
            [0.1, 0.9, 0.0],
        ];
        let flat: Vec<f32> = frames.iter().flatten().copied().collect();
        assert_eq!(greedy_ctc(&flat, 3, 2), vec![0, 0, 1]);
    }

    #[test]
    fn пробелы_перед_знаками_препинания_убираются() {
        // Так выглядит выход модели: пробел приклеен к началу каждого слова.
        let text = assemble(&[" Привет", " мир", ".", " Как", " дела", "?"]);
        assert_eq!(text, "Привет мир. Как дела?");
    }

    /// Отступление от эталонной реализации onnx-asr: там пробел выкидывается
    /// перед любым незначащим символом, и открывающая кавычка приклеивается
    /// к предыдущему слову — «часть«Как избежать».
    #[test]
    fn пробел_перед_открывающей_кавычкой_остаётся() {
        let text = assemble(&[" часть", " «", "Как", " избежать", "»"]);
        assert_eq!(text, "часть «Как избежать»");
    }

    /// Нераспознанный кусок модель помечает служебным токеном, и вставлять
    /// его пользователю в текст нельзя — это не то, что он сказал.
    #[test]
    fn служебный_токен_не_попадает_в_текст() {
        assert_eq!(assemble(&[" сло", "<unk>", "во"]), "слово");
    }

    /// Случайно задетая клавиша даёт запись в доли секунды, и она не должна
    /// ронять распознавание — ни паникой, ни ошибкой.
    #[test]
    #[ignore = "нужна скачанная модель"]
    fn короткая_запись_не_ломает_распознавание() {
        let spec = crate::models::find("gigaam-v3-e2e-ctc-int8").expect("модели нет в каталоге");
        assert!(spec.is_installed(), "модель не скачана");
        let mut model = GigaAm::load(&spec.dir()).expect("не загрузить модель");

        for сэмплов in [MIN_SAMPLES, 480, 1600, 8000] {
            let тишина = vec![0.0f32; сэмплов];
            let text = model
                .transcribe(&тишина)
                .unwrap_or_else(|e| panic!("{сэмплов} сэмплов: {e}"));
            println!("{сэмплов} сэмплов → «{text}»");
        }
    }

    /// Проверка всей цепочки на настоящей записи: препроцессор, модель,
    /// словарь. Требует скачанной модели и файла с речью, поэтому по
    /// умолчанию пропускается.
    ///
    /// Запуск: LLM_DICT_TEST_WAV=запись.wav LLM_DICT_TEST_TEXT='что сказано' \
    ///         cargo test распознаёт_запись -- --ignored --nocapture
    #[test]
    #[ignore = "нужна скачанная модель и файл с речью"]
    fn распознаёт_запись_целиком() {
        let wav = std::env::var("LLM_DICT_TEST_WAV").expect("LLM_DICT_TEST_WAV не задан");
        let expected = std::env::var("LLM_DICT_TEST_TEXT").expect("LLM_DICT_TEST_TEXT не задан");
        let spec = crate::models::find("gigaam-v3-e2e-ctc-int8").expect("модели нет в каталоге");
        assert!(
            spec.is_installed(),
            "модель не скачана: {}",
            spec.dir().display()
        );

        let samples = crate::audio::read_wav(&wav).expect("не прочитать запись");
        let mut model = GigaAm::load(&spec.dir()).expect("не загрузить модель");
        let text = model
            .transcribe(&samples)
            .expect("распознавание не удалось");

        println!("распознано: {text}");
        assert_eq!(text, expected);
    }
}
