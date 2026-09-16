//! Структурированный журнал операций.
//!
//! Каждое обращение к устройству и каждое решение драйвера попадают в журнал:
//! с порядковым номером, меткой времени, идентификатором запроса и результатом.
//! Хост-уровень превращает эти записи в JSON Lines и в события `tracing`.
//!
//! Все текстовые поля — статические строки, поэтому записи копируемы, не требуют
//! аллокаций и работают в `no_std`.

/// Уровень важности записи.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// Подробности, нужные только при отладке.
    Trace,
    /// Диагностические сведения.
    Debug,
    /// Нормальный ход работы.
    Info,
    /// Нештатная ситуация, работа продолжается.
    Warn,
    /// Операция не выполнена.
    Error,
}

impl Level {
    /// Имя уровня для сериализации.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

impl core::fmt::Display for Level {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Что именно произошло.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum EventKind {
    /// Открытие сессии: проверка связи с периферией.
    Open {
        /// Имя транспорта.
        transport: &'static str,
        /// Удалось ли открыть.
        ok: bool,
    },
    /// Закрытие сессии.
    Close {
        /// Удалось ли вернуть безопасный лимит тока.
        ok: bool,
    },
    /// Чтение регистра.
    Read {
        /// Адрес регистра.
        addr: u16,
        /// Прочитанное значение.
        value: u8,
        /// Длительность операции в микросекундах.
        elapsed_us: u64,
    },
    /// Запись регистра.
    Write {
        /// Адрес регистра.
        addr: u16,
        /// Записанное значение.
        value: u8,
        /// Длительность операции в микросекундах.
        elapsed_us: u64,
    },
    /// Результат детекции адаптера.
    Detect {
        /// Распознанный тип.
        adapter: &'static str,
        /// Сырое значение `APSD_STATUS`.
        raw_status: u8,
        /// Сырое значение `APSD_RESULT_STATUS`.
        raw_result: u8,
        /// Сколько миллисекунд шла детекция.
        waited_ms: u64,
    },
    /// Применённая политика тока.
    Policy {
        /// Тип адаптера.
        adapter: &'static str,
        /// Целевой лимит тока в микроамперax.
        icl_ua: u32,
        /// Код, записанный в регистр.
        icl_raw: u8,
        /// Напряжение QC2, если запрашивалось.
        qc2_voltage: Option<&'static str>,
        /// Допустим ли charge pump.
        pump_eligible: bool,
    },
    /// Повторная попытка.
    Retry {
        /// Что повторяем.
        op: &'static str,
        /// Номер попытки.
        attempt: u8,
        /// Причина.
        reason: &'static str,
    },
    /// Сброс канала связи.
    Reset {
        /// Удалось ли восстановить канал.
        ok: bool,
    },
    /// Смена состояния драйвера.
    StateChange {
        /// Прежнее состояние.
        from: &'static str,
        /// Новое состояние.
        to: &'static str,
    },
    /// Ошибка.
    Error {
        /// Операция.
        op: &'static str,
        /// Код ошибки.
        error: &'static str,
    },
}

impl EventKind {
    /// Стабильное имя типа события для фильтрации и метрик.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Open { .. } => "open",
            Self::Close { .. } => "close",
            Self::Read { .. } => "read",
            Self::Write { .. } => "write",
            Self::Detect { .. } => "detect",
            Self::Policy { .. } => "policy",
            Self::Retry { .. } => "retry",
            Self::Reset { .. } => "reset",
            Self::StateChange { .. } => "state",
            Self::Error { .. } => "error",
        }
    }
}

/// Одна запись журнала.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Event {
    /// Порядковый номер записи, начиная с 1.
    pub seq: u64,
    /// Метка времени в миллисекундах монотонных часов.
    pub ts_ms: u64,
    /// Идентификатор запроса, к которому относится запись.
    pub request_id: u64,
    /// Уровень важности.
    pub level: Level,
    /// Содержание.
    pub kind: EventKind,
}

/// Приёмник записей журнала.
///
/// Реализация не должна паниковать и обязана быть готовой к вызову из `Drop`.
pub trait Journal {
    /// Принимает одну запись.
    fn event(&self, event: &Event);
}

/// Журнал-заглушка: ничего не делает.
#[derive(Debug, Clone, Copy, Default)]
pub struct NullJournal;

impl Journal for NullJournal {
    fn event(&self, _event: &Event) {}
}
