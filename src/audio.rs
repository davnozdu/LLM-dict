//! Запись с микрофона через cpal, приведение к 16 кГц моно и упаковка в WAV.
//!
//! `cpal::Stream` не `Send`, поэтому поток живёт внутри отдельного треда:
//! наружу торчат только канал остановки и канал с готовыми сэмплами.

use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;

pub const TARGET_RATE: u32 = 16_000;

/// Текущая громкость 0.0..1.0 для индикатора в UI.
#[derive(Debug, Default)]
pub struct Level(AtomicU32);

impl Level {
    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
    fn set(&self, v: f32) {
        self.0.store(v.to_bits(), Ordering::Relaxed);
    }
}

pub struct Recording {
    stop_tx: Sender<()>,
    done_rx: Receiver<Vec<f32>>,
}

impl Recording {
    /// Останавливает запись и отдаёт накопленные сэмплы (16 кГц, моно, f32).
    pub fn finish(self) -> Vec<f32> {
        let _ = self.stop_tx.send(());
        self.done_rx.recv().unwrap_or_default()
    }
}

pub fn input_device_name() -> Option<String> {
    let device = cpal::default_host().default_input_device()?;
    device.description().ok().map(|d| d.name().to_string())
}

/// Приводит поток произвольной частоты и числа каналов к 16 кГц моно
/// усреднением по окну. Для речи этого достаточно, а зависимостей не тянет.
struct Downmix {
    channels: usize,
    ratio: f64,
    pos: f64,
    acc: f32,
    acc_n: u32,
}

impl Downmix {
    fn new(src_rate: u32, channels: u16) -> Self {
        Self {
            channels: channels.max(1) as usize,
            ratio: TARGET_RATE as f64 / src_rate as f64,
            pos: 0.0,
            acc: 0.0,
            acc_n: 0,
        }
    }

    fn push(&mut self, interleaved: &[f32], out: &mut Vec<f32>) {
        for frame in interleaved.chunks(self.channels) {
            let mono = frame.iter().sum::<f32>() / self.channels as f32;
            self.acc += mono;
            self.acc_n += 1;
            self.pos += self.ratio;
            if self.pos >= 1.0 {
                self.pos -= 1.0;
                out.push(self.acc / self.acc_n as f32);
                self.acc = 0.0;
                self.acc_n = 0;
            }
        }
    }
}

pub fn start(level: Arc<Level>) -> Result<Recording> {
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let (done_tx, done_rx) = mpsc::channel::<Vec<f32>>();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();

    std::thread::spawn(move || {
        let build = || -> Result<_> {
            let host = cpal::default_host();
            let device = host
                .default_input_device()
                .ok_or_else(|| anyhow!("не найден микрофон по умолчанию"))?;
            let supported = device.default_input_config()?;
            let sample_format = supported.sample_format();
            let config: cpal::StreamConfig = supported.into();
            Ok((device, config, sample_format))
        };

        let (device, config, sample_format) = match build() {
            Ok(v) => v,
            Err(e) => {
                let _ = ready_tx.send(Err(e.to_string()));
                let _ = done_tx.send(Vec::new());
                return;
            }
        };

        let buffer = Arc::new(std::sync::Mutex::new(Vec::<f32>::with_capacity(
            TARGET_RATE as usize * 30,
        )));
        let mut dm = Downmix::new(config.sample_rate, config.channels);
        let buf_cb = buffer.clone();
        let level_cb = level.clone();

        let mut on_samples = move |data: &[f32]| {
            let peak = data.iter().fold(0.0f32, |m, s| m.max(s.abs()));
            // Плавное затухание, иначе индикатор дёргается.
            let prev = level_cb.get();
            level_cb.set(if peak > prev { peak } else { prev * 0.85 });

            let mut out = Vec::new();
            dm.push(data, &mut out);
            if let Ok(mut b) = buf_cb.lock() {
                b.extend_from_slice(&out);
            }
        };

        let err_fn = |e| log::error!("ошибка потока записи: {e}");
        let stream = match sample_format {
            cpal::SampleFormat::F32 => device.build_input_stream(
                config,
                move |data: &[f32], _: &_| on_samples(data),
                err_fn,
                None,
            ),
            cpal::SampleFormat::I16 => device.build_input_stream(
                config,
                move |data: &[i16], _: &_| {
                    let f: Vec<f32> = data.iter().map(|s| *s as f32 / i16::MAX as f32).collect();
                    on_samples(&f)
                },
                err_fn,
                None,
            ),
            cpal::SampleFormat::U16 => device.build_input_stream(
                config,
                move |data: &[u16], _: &_| {
                    let f: Vec<f32> = data
                        .iter()
                        .map(|s| (*s as f32 - 32768.0) / 32768.0)
                        .collect();
                    on_samples(&f)
                },
                err_fn,
                None,
            ),
            other => {
                let _ = ready_tx.send(Err(format!("неподдерживаемый формат сэмплов: {other:?}")));
                let _ = done_tx.send(Vec::new());
                return;
            }
        };

        let stream = match stream.and_then(|s| s.play().map(|_| s)) {
            Ok(s) => s,
            Err(e) => {
                let _ = ready_tx.send(Err(format!("не удалось открыть микрофон: {e}")));
                let _ = done_tx.send(Vec::new());
                return;
            }
        };

        let _ = ready_tx.send(Ok(()));
        let _ = stop_rx.recv();
        drop(stream);
        level.set(0.0);

        let samples = buffer.lock().map(|b| b.clone()).unwrap_or_default();
        let _ = done_tx.send(samples);
    });

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(Recording { stop_tx, done_rx }),
        Ok(Err(e)) => Err(anyhow!(e)),
        Err(_) => Err(anyhow!("поток записи не запустился")),
    }
}

/// WAV 16 кГц моно 16 бит — принимается и Groq, и whisper.cpp без конвертации.
pub fn to_wav(samples: &[f32]) -> Result<Vec<u8>> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: TARGET_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut cursor = std::io::Cursor::new(Vec::<u8>::new());
    {
        let mut writer = hound::WavWriter::new(&mut cursor, spec)?;
        for s in samples {
            writer.write_sample((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)?;
        }
        writer.finalize()?;
    }
    Ok(cursor.into_inner())
}

pub fn duration_secs(samples: &[f32]) -> f32 {
    samples.len() as f32 / TARGET_RATE as f32
}
