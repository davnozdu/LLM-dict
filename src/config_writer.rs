//! Serial, debounced configuration writes. Flush is a barrier before exit or migration.
use crate::config::Config;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

enum Command {
    Save(Box<Config>),
    Flush(Sender<Result<(), String>>),
}

pub struct Writer {
    tx: Sender<Command>,
    pub errors: Receiver<String>,
}

impl Writer {
    pub fn new() -> Self {
        Self::with_save(|cfg| cfg.save().map_err(|e| e.to_string()))
    }

    fn with_save(mut save: impl FnMut(&Config) -> Result<(), String> + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        let (errors_tx, errors) = mpsc::channel();
        std::thread::spawn(move || {
            let mut pending: Option<Box<Config>> = None;
            let mut delay = Duration::from_millis(400);
            loop {
                let command = if pending.is_some() {
                    rx.recv_timeout(delay)
                } else {
                    rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected)
                };
                match command {
                    Ok(Command::Save(cfg)) => {
                        pending = Some(cfg);
                        delay = Duration::from_millis(400);
                    }
                    Ok(Command::Flush(reply)) => {
                        let result = pending.as_ref().map_or(Ok(()), |cfg| save(cfg));
                        if result.is_ok() {
                            pending = None;
                        }
                        let _ = reply.send(result);
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => match save(pending.as_ref().unwrap()) {
                        Ok(()) => pending = None,
                        Err(error) => {
                            let _ = errors_tx.send(error);
                            delay = Duration::from_secs(2);
                        }
                    },
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        });
        Self { tx, errors }
    }

    pub fn queue(&self, cfg: Config) {
        let _ = self.tx.send(Command::Save(Box::new(cfg)));
    }

    pub fn flush(&self) -> Result<(), String> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(Command::Flush(tx))
            .map_err(|e| e.to_string())?;
        rx.recv().map_err(|e| e.to_string())?
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        if let Err(e) = self.flush() {
            log::error!("не сохранить настройки перед выходом: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flush_coalesces_updates_and_retries_failed_write() {
        let (tx, rx) = mpsc::channel();
        let mut fail = true;
        let writer = Writer::with_save(move |cfg| {
            if std::mem::take(&mut fail) {
                return Err("disk unavailable".into());
            }
            tx.send(cfg.api_key.clone()).unwrap();
            Ok(())
        });
        for key in ["old", "latest"] {
            writer.queue(Config {
                api_key: key.into(),
                ..Config::default()
            });
        }
        assert!(writer.flush().is_err());
        assert!(writer.flush().is_ok());
        assert_eq!(rx.recv().unwrap(), "latest");
        assert!(rx.try_recv().is_err());
    }
}
