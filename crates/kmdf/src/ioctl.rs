//! Контракт драйвера с пользовательским режимом: коды IOCTL и структуры обмена.
//!
//! Все запросы — `METHOD_BUFFERED`, `FILE_ANY_ACCESS`, тип устройства
//! `FILE_DEVICE_UNKNOWN` (0x22). Коды построены через стандартный макрос
//! `CTL_CODE(Type, Function, Method, Access) = (Type << 16) | (Access << 14) | (Function << 2) | Method`.

/// Тип устройства, который объявляет драйвер.
pub const FILE_DEVICE_NABU_CHARGER: u32 = 0x22;

/// Собирает код управления по правилам `CTL_CODE`.
///
/// * `function` — номер функции (0x800..0xFFF для вендорских);
/// * `method` — 0 = `METHOD_BUFFERED`;
/// * `access` — 0 = `FILE_ANY_ACCESS`.
#[must_use]
pub const fn ctl_code(function: u32, method: u32, access: u32) -> u32 {
    (FILE_DEVICE_NABU_CHARGER << 16) | (access << 14) | (function << 2) | method
}

/// Получить состояние драйвера и последний применённый план.
pub const IOCTL_NABU_GET_STATUS: u32 = ctl_code(0x800, 0, 0);
/// Запустить детекцию адаптера (неблокирующе: результат появится в статусе).
pub const IOCTL_NABU_DETECT_START: u32 = ctl_code(0x801, 0, 0);
/// Применить политику тока для распознанного адаптера.
pub const IOCTL_NABU_APPLY_POLICY: u32 = ctl_code(0x802, 0, 0);
/// Принудительно задать лимит входного тока в микроамперax.
pub const IOCTL_NABU_SET_ICL: u32 = ctl_code(0x803, 0, 0);
/// Прочитать регистр периферии зарядника (диагностика).
pub const IOCTL_NABU_READ_REG: u32 = ctl_code(0x804, 0, 0);
/// Записать регистр периферии зарядника (диагностика).
pub const IOCTL_NABU_WRITE_REG: u32 = ctl_code(0x805, 0, 0);
/// Получить снимок журнала операций.
pub const IOCTL_NABU_GET_JOURNAL: u32 = ctl_code(0x806, 0, 0);

/// Идентификатор структуры, чтобы клиент не перепутал версии.
pub const NABU_STATUS_MAGIC: u32 = 0x4E41_4255; // "NABU"

/// Версия контракта обмена.
pub const NABU_STATUS_VERSION: u16 = 1;

/// Возможности драйвера: один поток расширений, как принято в Windows.
pub const NABU_CAPABILITIES: u32 = 1;

/// Состояние сессии драйвера (совпадает с `charger_core::State`).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NabuState {
    /// Сессия не открыта.
    Closed = 0,
    /// Связь есть, детекция не запускалась.
    Idle = 1,
    /// Идёт ожидание APSD.
    Detecting = 2,
    /// Тип определён, политика применена.
    Ready = 3,
    /// Сессия в неисправности.
    Faulted = 4,
}

impl NabuState {
    /// Переводит состояние ядра в контрактное представление.
    #[must_use]
    pub const fn from_core(state: charger_core::State) -> Self {
        match state {
            charger_core::State::Closed => Self::Closed,
            charger_core::State::Idle => Self::Idle,
            charger_core::State::Detecting => Self::Detecting,
            charger_core::State::Ready => Self::Ready,
            charger_core::State::Faulted => Self::Faulted,
        }
    }
}

/// Ответ на [`IOCTL_NABU_GET_STATUS`].
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct NabuStatus {
    /// Магия [`NABU_STATUS_MAGIC`].
    pub magic: u32,
    /// Версия контракта [`NABU_STATUS_VERSION`].
    pub version: u16,
    /// Битовая маска возможностей.
    pub capabilities: u32,
    /// Состояние сессии.
    pub state: u8,
    /// Код типа адаптера (`AdapterType` как число; 255 — неизвестно).
    pub adapter_code: u8,
    /// Допустим ли charge pump.
    pub pump_eligible: u8,
    /// Зарезервировано для выравнивания.
    pub reserved: u8,
    /// Фактический лимит входного тока в микроамперax.
    pub icl_ua: u32,
    /// Код лимита, записанный в регистр.
    pub icl_raw: u8,
    /// Заосервировано.
    pub reserved2: [u8; 3],
    /// Сколько чтений регистров выполнено.
    pub reads: u64,
    /// Сколько записей выполнено.
    pub writes: u64,
    /// Сколько повторов после сбоев.
    pub retries: u64,
    /// Сколько сбросов канала.
    pub resets: u64,
    /// Сколько ошибок зафиксировано.
    pub errors: u64,
}

/// Запрос [`IOCTL_NABU_READ_REG`] и [`IOCTL_NABU_WRITE_REG`].
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct NabuRegRequest {
    /// Адрес регистра.
    pub addr: u16,
    /// Значение: вход для записи, выход для чтения.
    pub value: u8,
    /// Зарезервировано.
    pub reserved: u8,
    /// Код ошибки транспорта, если операция не удалась.
    pub error_code: i32,
}

/// Запрос [`IOCTL_NABU_SET_ICL`].
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct NabuIclRequest {
    /// Требуемый лимит входного тока в микроамперax.
    pub icl_ua: u32,
    /// Фактически выставленный лимит (после квантования по сетке).
    pub applied_ua: u32,
    /// Код регистра, который записали.
    pub icl_raw: u8,
    /// Код ошибки: 0 — успех.
    pub error_code: i32,
}

/// Одна запись журнала в буфере [`IOCTL_NABU_GET_JOURNAL`].
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct NabuJournalEntry {
    /// Порядковый номер.
    pub seq: u64,
    /// Метка времени в миллисекундах монотонных часов ядра.
    pub ts_ms: u64,
    /// Идентификатор запроса.
    pub request_id: u64,
    /// Уровень (`trace`..`error`).
    pub level: u8,
    /// Тип события.
    pub kind: u8,
    /// Адрес регистра, если применимо.
    pub addr: u16,
    /// Значение, если применимо.
    pub value: u8,
    /// Зарезервировано.
    pub reserved: [u8; 3],
}

/// Запрос [`IOCTL_NABU_GET_JOURNAL`].
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct NabuJournalRequest {
    /// Сколько записей вернуть (не больше размера буфера клиента).
    pub count: u32,
    /// Сколько записей реально доступно.
    pub available: u32,
}
