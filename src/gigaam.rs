//! Локальное распознавание GigaAM v3 e2e CTC и RNN-T через ONNX Runtime.
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
pub const RNNT_ENCODER_FILE: &str = "v3_e2e_rnnt_encoder.int8.onnx";
pub const RNNT_DECODER_FILE: &str = "v3_e2e_rnnt_decoder.int8.onnx";
pub const RNNT_JOINT_FILE: &str = "v3_e2e_rnnt_joint.int8.onnx";
pub const RNNT_VOCAB_FILE: &str = "v3_e2e_rnnt_vocab.txt";

/// Препроцессор из onnx-asr: считает те самые лог-мел-признаки, на которых
/// модель обучалась. Лежит в бинарнике — 44 КБ, а свой расчёт мел-фильтров
/// был бы лишним риском. Происхождение и лицензия — в `assets/README.md`.
const PREPROCESSOR: &[u8] = include_bytes!("../assets/gigaam_v3.onnx");

/// Меньше одного окна анализа препроцессор не переварит.
const MIN_SAMPLES: usize = 320;

pub struct GigaAm {
    preprocessor: Session,
    model: AcousticModel,
    vocab: Vocab,
}

enum AcousticModel {
    Ctc(Session),
    Rnnt(Box<Rnnt>),
}

struct Rnnt {
    encoder: Session,
    decoder: Session,
    joint: Session,
}

/// Жадное декодирование transducer: blank переводит на следующий кадр,
/// остальные токены (включая повторы) остаются в тексте. После трёх токенов
/// кадр меняется принудительно, как в onnx-asr. refresh сообщает декодеру,
/// что предыдущий токен принят и нужно обновить prediction/LSTM.
fn greedy_rnnt(
    frames: usize,
    blank: usize,
    mut next: impl FnMut(usize, usize, bool) -> Result<usize>,
) -> Result<Vec<usize>> {
    let mut ids = Vec::new();
    let mut previous = blank;
    let mut refresh = true;
    for frame in 0..frames {
        for _ in 0..3 {
            let token = next(frame, previous, refresh)?;
            refresh = token != blank;
            if token == blank {
                break;
            }
            ids.push(token);
            previous = token;
        }
    }
    Ok(ids)
}

impl Rnnt {
    fn transcribe(
        &mut self,
        features: Tensor<f32>,
        lengths: Tensor<i64>,
        vocab: &Vocab,
    ) -> Result<Vec<usize>> {
        let outputs = self.encoder.run(ort::inputs![
            "audio_signal" => features,
            "length" => lengths
        ])?;
        let (shape, encoded) = outputs["encoded"].try_extract_tensor::<f32>()?;
        let (_, lengths) = outputs["encoded_len"].try_extract_tensor::<i32>()?;
        if shape.len() != 3 || shape[0] != 1 || shape[1] != 768 || shape[2] < 0 {
            bail!("неожиданная размерность энкодера RNN-T: {shape:?}");
        }
        let steps = shape[2] as usize;
        let frames = *lengths.first().context("нет длины выхода RNN-T")?;
        if frames < 0 || frames as usize > steps {
            bail!("неверная длина выхода RNN-T: {frames}, кадров: {steps}");
        }
        let mut h = vec![0.0f32; 320];
        let mut c = vec![0.0f32; 320];
        let mut prediction = Vec::new();
        // Алгоритм и имена ONNX-тензоров сверены с onnx-asr (MIT),
        // src/onnx_asr/models/gigaam.py; см. assets/README.md.
        greedy_rnnt(frames as usize, vocab.blank, |frame, previous, refresh| {
            if refresh {
                let decoded = self.decoder.run(ort::inputs![
                    "x" => Tensor::from_array((vec![1, 1], vec![previous as i64]))?,
                    "h.1" => Tensor::from_array((vec![1, 1, 320], h.clone()))?,
                    "c.1" => Tensor::from_array((vec![1, 1, 320], c.clone()))?
                ])?;
                let (shape, data) = decoded["dec"].try_extract_tensor::<f32>()?;
                if shape.as_ref() != [1, 1, 320] {
                    bail!("неожиданная размерность декодера RNN-T: {shape:?}");
                }
                prediction = data.to_vec();
                h = decoded["h"].try_extract_tensor::<f32>()?.1.to_vec();
                c = decoded["c"].try_extract_tensor::<f32>()?.1.to_vec();
            }
            // Энкодер отдаёт [batch, channel, time], joint принимает один
            // кадр [1, 768, 1]. prediction транспонирован в [1, 320, 1].
            let frame_data: Vec<f32> = (0..768).map(|ch| encoded[ch * steps + frame]).collect();
            let joint = self.joint.run(ort::inputs![
                "enc" => Tensor::from_array((vec![1, 768, 1], frame_data))?,
                "dec" => Tensor::from_array((vec![1, 320, 1], prediction.clone()))?
            ])?;
            let (_, logits) = joint["joint"].try_extract_tensor::<f32>()?;
            if logits.len() != vocab.tokens.len() {
                bail!("выход joint RNN-T не соответствует словарю");
            }
            // В случае равных вероятностей, как numpy.argmax, берём первый.
            let token =
                logits.iter().enumerate().fold(
                    0,
                    |best, (i, value)| {
                        if *value > logits[best] {
                            i
                        } else {
                            best
                        }
                    },
                );
            Ok(token)
        })
    }
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
        // При равных вероятностях берём первый класс — как numpy.argmax и как
        // жадный декодер RNN-T ниже. `max_by` вернул бы последний, и выбор
        // зависел бы от того, какой движок стоит в настройках.
        let best = frame.iter().enumerate().fold(
            0,
            |best, (i, value)| {
                if *value > frame[best] {
                    i
                } else {
                    best
                }
            },
        );
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
        let rnnt = dir.join(RNNT_ENCODER_FILE).is_file();
        let vocab_path = dir.join(if rnnt { RNNT_VOCAB_FILE } else { VOCAB_FILE });
        let vocab = parse_vocab(
            &std::fs::read_to_string(&vocab_path)
                .with_context(|| format!("не прочитать {}", vocab_path.display()))?,
        )?;

        let preprocessor = Session::builder()?
            .commit_from_memory(PREPROCESSOR)
            .context("не собрать препроцессор GigaAM")?;
        let load = |name: &str| -> Result<Session> {
            let path = dir.join(name);
            Session::builder()?
                .commit_from_file(&path)
                .with_context(|| format!("не загрузить {}", path.display()))
        };
        let model = if rnnt {
            AcousticModel::Rnnt(Box::new(Rnnt {
                encoder: load(RNNT_ENCODER_FILE)?,
                decoder: load(RNNT_DECODER_FILE)?,
                joint: load(RNNT_JOINT_FILE)?,
            }))
        } else {
            AcousticModel::Ctc(load(MODEL_FILE)?)
        };

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

        let ids = match &mut self.model {
            AcousticModel::Ctc(model) => {
                let outputs = model.run(ort::inputs![
                    "features" => features,
                    "feature_lengths" => feature_lengths
                ])?;
                let (shape, log_probs) = outputs["log_probs"].try_extract_tensor::<f32>()?;
                let classes = *shape
                    .last()
                    .context("модель вернула тензор без размерности")?
                    as usize;
                if classes != self.vocab.tokens.len() {
                    bail!("выход CTC не соответствует словарю");
                }
                greedy_ctc(log_probs, classes, self.vocab.blank)
            }
            AcousticModel::Rnnt(model) => {
                model.transcribe(features, feature_lengths, &self.vocab)?
            }
        };
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
    fn rnnt_blank_не_меняет_prediction_а_повтор_сохраняется() {
        let mut calls = Vec::new();
        let mut answers = [1, 3, 3, 1, 3].into_iter();
        let ids = greedy_rnnt(3, 3, |frame, previous, refresh| {
            calls.push((frame, previous, refresh));
            Ok(answers.next().unwrap())
        })
        .unwrap();
        assert_eq!(ids, [1, 1]);
        assert_eq!(
            calls,
            [
                (0, 3, true),
                (0, 1, true),
                (1, 1, false),
                (2, 1, false),
                (2, 1, true)
            ]
        );
    }

    #[test]
    fn rnnt_ограничивает_токены_и_передаёт_ошибки() {
        let ids = greedy_rnnt(2, 3, |_, _, refresh| {
            assert!(refresh);
            Ok(1)
        })
        .unwrap();
        assert_eq!(ids, [1; 6]);
        assert!(greedy_rnnt(1, 3, |_, _, _| bail!("ошибка ONNX")).is_err());
        assert!(greedy_rnnt(0, 3, |_, _, _| panic!("нет кадров"))
            .unwrap()
            .is_empty());
    }

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
    fn при_равных_вероятностях_берётся_первый_класс() {
        // Так же поступает numpy.argmax и жадный декодер RNN-T. Иначе выход
        // зависел бы от того, какой движок стоит в настройках.
        let frames = [[0.5, 0.5, 0.0], [0.5, 0.5, 0.0]];
        let flat: Vec<f32> = frames.iter().flatten().copied().collect();
        assert_eq!(greedy_ctc(&flat, 3, 2), vec![0]);
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
        let model_id = std::env::var("LLM_DICT_TEST_MODEL")
            .unwrap_or_else(|_| "gigaam-v3-e2e-ctc-int8".into());
        let spec = crate::models::find(&model_id).expect("модели нет в каталоге");
        let dir = std::env::var_os("LLM_DICT_TEST_MODEL_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| spec.dir());
        let samples = crate::audio::read_wav(&wav).expect("не прочитать запись");
        let mut model = GigaAm::load(&dir).expect("не загрузить модель");
        let started = std::time::Instant::now();
        let text = model
            .transcribe(&samples)
            .expect("распознавание не удалось");

        println!("{model_id}: {:?}; распознано: {text}", started.elapsed());
        // Второй вызов должен начать с чистого состояния LSTM.
        assert_eq!(model.transcribe(&samples).unwrap(), text);
        assert_eq!(text, expected);
    }
}
