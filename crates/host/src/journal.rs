//! Запись журнала: JSON Lines для разбора инцидентов и мост в `tracing`.
//!
//! Формат одной строки:
//!
//! ```json
//! {"seq":42,"ts_ms":1500,"request_id":7,"level":"info","kind":"detect",
//!  "adapter":"HVDCP3","raw_status":3,"raw_result":72,"waited_ms":1500}
//! ```

use charger_core::journal::{Event, EventKind, Journal, Level};
use serde_json::{Map, Value, json};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::Mutex;

/// Журнал в файл JSON Lines.
#[derive(Debug)]
pub struct JsonlJournal {
    writer: Mutex<BufWriter<File>>,
    path: std::path::PathBuf,
}

impl JsonlJournal {
    /// Создаёт файл журнала (каталоги создаются автоматически).
    ///
    /// # Errors
    ///
    /// [`std::io::Error`], если каталог или файл недоступны для записи.
    pub fn create(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            writer: Mutex::new(BufWriter::new(file)),
            path,
        })
    }

    /// Путь к файлу журнала.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Сбрасывает буфер на диск.
    ///
    /// # Errors
    ///
    /// [`std::io::Error`], если запись не удалась.
    pub fn flush(&self) -> std::io::Result<()> {
        let mut guard = self
            .writer
            .lock()
            .map_err(|_| std::io::Error::other("журнал заблокирован"))?;
        guard.flush()
    }

    fn write_record(&self, event: &Event) {
        let line = serde_json::to_string(&render(event)).unwrap_or_else(|_| "{}".to_owned());
        match self.writer.lock() {
            Ok(mut guard) => {
                if guard.write_all(line.as_bytes()).is_err() || guard.write_all(b"\n").is_err() {
                    tracing::error!(target: "nabu::journal", "не удалось записать запись журнала");
                }
            }
            Err(_) => {
                tracing::error!(target: "nabu::journal", "журнал заблокирован");
            }
        }
    }
}

impl Journal for JsonlJournal {
    fn event(&self, event: &Event) {
        self.write_record(event);
    }
}

/// Мост журнала ядра в события `tracing` (структурированные поля).
#[derive(Debug, Clone, Copy, Default)]
pub struct TracingJournal;

impl Journal for TracingJournal {
    fn event(&self, event: &Event) {
        let kind = event.kind.name();
        let ts_ms = event.ts_ms;
        let request_id = event.request_id;
        match event.level {
            Level::Trace => {
                tracing::trace!(target: "nabu::charger", ts_ms, request_id, kind, "запись");
            }
            Level::Debug => {
                tracing::debug!(target: "nabu::charger", ts_ms, request_id, kind, "запись");
            }
            Level::Info => {
                tracing::info!(target: "nabu::charger", ts_ms, request_id, kind, "запись");
            }
            Level::Warn => {
                tracing::warn!(target: "nabu::charger", ts_ms, request_id, kind, "запись");
            }
            Level::Error => {
                tracing::error!(target: "nabu::charger", ts_ms, request_id, kind, "запись");
            }
        }
    }
}

/// Журнал, отправляющий запись двум приёмникам сразу.
#[derive(Debug)]
pub struct Fanout<A, B> {
    first: A,
    second: B,
}

impl<A, B> Fanout<A, B> {
    /// Объединяет два журнала.
    pub const fn new(first: A, second: B) -> Self {
        Self { first, second }
    }
}

impl<A: Journal, B: Journal> Journal for Fanout<A, B> {
    fn event(&self, event: &Event) {
        self.first.event(event);
        self.second.event(event);
    }
}

/// Превращает запись ядра в JSON-объект.
#[must_use]
pub fn render(event: &Event) -> Value {
    let mut map = Map::new();
    map.insert("seq".to_owned(), json!(event.seq));
    map.insert("ts_ms".to_owned(), json!(event.ts_ms));
    map.insert("request_id".to_owned(), json!(event.request_id));
    map.insert("level".to_owned(), json!(event.level.as_str()));
    map.insert("kind".to_owned(), json!(event.kind.name()));
    match event.kind {
        EventKind::Open { transport, ok } => {
            map.insert("transport".to_owned(), json!(transport));
            map.insert("ok".to_owned(), json!(ok));
        }
        EventKind::Close { ok } | EventKind::Reset { ok } => {
            map.insert("ok".to_owned(), json!(ok));
        }
        EventKind::Read {
            addr,
            value,
            elapsed_us,
        }
        | EventKind::Write {
            addr,
            value,
            elapsed_us,
        } => {
            map.insert("addr".to_owned(), json!(addr));
            map.insert("value".to_owned(), json!(value));
            map.insert("elapsed_us".to_owned(), json!(elapsed_us));
        }
        EventKind::Detect {
            adapter,
            raw_status,
            raw_result,
            waited_ms,
        } => {
            map.insert("adapter".to_owned(), json!(adapter));
            map.insert("raw_status".to_owned(), json!(raw_status));
            map.insert("raw_result".to_owned(), json!(raw_result));
            map.insert("waited_ms".to_owned(), json!(waited_ms));
        }
        EventKind::Policy {
            adapter,
            icl_ua,
            icl_raw,
            qc2_voltage,
            pump_eligible,
        } => {
            map.insert("adapter".to_owned(), json!(adapter));
            map.insert("icl_ua".to_owned(), json!(icl_ua));
            map.insert("icl_raw".to_owned(), json!(icl_raw));
            map.insert("qc2_voltage".to_owned(), json!(qc2_voltage));
            map.insert("pump_eligible".to_owned(), json!(pump_eligible));
        }
        EventKind::Retry {
            op,
            attempt,
            reason,
        } => {
            map.insert("op".to_owned(), json!(op));
            map.insert("attempt".to_owned(), json!(attempt));
            map.insert("reason".to_owned(), json!(reason));
        }
        EventKind::StateChange { from, to } => {
            map.insert("from".to_owned(), json!(from));
            map.insert("to".to_owned(), json!(to));
        }
        EventKind::Error { op, error } => {
            map.insert("op".to_owned(), json!(op));
            map.insert("error".to_owned(), json!(error));
        }
        // Enum помечен `non_exhaustive`: новые варианты не должны ломать журнал.
        _ => {}
    }
    Value::Object(map)
}
