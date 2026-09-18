//! Контракт драйвера LN8000 с пользовательским режимом.
//!
//! Все запросы — `METHOD_BUFFERED`, `FILE_ANY_ACCESS`, тип устройства
//! `FILE_DEVICE_UNKNOWN` (0x22). Коды построены стандартным макросом
//! `CTL_CODE(Type, Function, Method, Access) = (Type << 16) | (Access << 14) | (Function << 2) | Method`.

/// Тип устройства драйвера LN8000.
pub const FILE_DEVICE_LN8000: u32 = 0x22;

/// Собирает код управления по правилам `CTL_CODE`.
#[must_use]
pub const fn ctl_code(function: u32, method: u32, access: u32) -> u32 {
    (FILE_DEVICE_LN8000 << 16) | (access << 14) | (function << 2) | method
}

/// Состояние драйвера: режим, отказы, последний отсчёт, счётчики сеансов.
pub const IOCTL_LN8000_GET_STATUS: u32 = ctl_code(0x810, 0, 0);
/// Прочитать регистр LN8000 напрямую (диагностика).
pub const IOCTL_LN8000_READ_REG: u32 = ctl_code(0x811, 0, 0);
/// Записать регистр LN8000 напрямую (диагностика).
pub const IOCTL_LN8000_WRITE_REG: u32 = ctl_code(0x812, 0, 0);
/// Задать лимиты: входной ток и напряжение заряда.
pub const IOCTL_LN8000_SET_LIMITS: u32 = ctl_code(0x813, 0, 0);
/// Переключить режим: standby / bypass / switching.
pub const IOCTL_LN8000_SET_MODE: u32 = ctl_code(0x814, 0, 0);
/// Получить сведения о сеансах заряда (текущем и последнем завершённом).
pub const IOCTL_LN8000_GET_SESSIONS: u32 = ctl_code(0x815, 0, 0);
/// Выгрузить последние отсчёты телеметрии.
pub const IOCTL_LN8000_GET_SAMPLES: u32 = ctl_code(0x816, 0, 0);

/// Explicit charge start/stop.
///
/// Mirrors the reference `psy_chg_set_charging_enable` sequence: disable
/// reverse-current protection, request the op mode, read the mode back and
/// report what the chip actually answered. Unlike the automatic path this
/// never fails silently: the caller sees the raw `SYS_STS`.
pub const IOCTL_LN8000_SET_CHARGE: u32 = ctl_code(0x817, 0, 0);

/// `error_code` для отказа включить 1:1 вне окна обхода.
///
/// Отдельный код (не `-4`): это не отказ чипа, а запрет политики — 1:1 подаёт
/// вход напрямую на батарею, поэтому при `Vin >= 8 В` (или ниже 4,2 В) режим не
/// включается ни через `SET_MODE`, ни автоматикой.
pub const ERR_BYPASS_VIN_OUT_OF_WINDOW: i32 = -20;

/// Run HVDCP / QC negotiate (SUPERUSER preferred; Usbin RH secondary).
///
/// On stock ACPI (`UsbinConn=0`) opens `\Device\Spmi\SUPERUSER`, grants peri
/// `0x13`, enables `0x1362`, reruns APSD, then QC2 FORCE_9V or QC3 pulses.
/// Returns `error_code = -10` only when **both** SUPERUSER and Usbin RH fail.
pub const IOCTL_LN8000_RUN_HVDCP: u32 = ctl_code(0x818, 0, 0);

/// Идентификатор структуры состояния.
pub const LN8000_STATUS_MAGIC: u32 = 0x4C4E_3830; // "LN80"

/// Версия контракта.
pub const LN8000_STATUS_VERSION: u16 = 1;

/// Состояние драйвера для [`IOCTL_LN8000_GET_STATUS`].
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Ln8000Status {
    /// Магия [`LN8000_STATUS_MAGIC`].
    pub magic: u32,
    /// Версия контракта.
    pub version: u16,
    /// Режим работы (0 — неизвестно, 1 — standby, 2 — bypass, 3 — switching).
    pub op_mode: u8,
    /// Код состояния сессии драйвера (0 — закрыта, 1 — опознан, 2 — настроен,
    /// 3 — switching, 4 — отказ).
    pub state: u8,
    /// Сырое значение `SYS_STS`.
    pub sys_sts: u8,
    /// Сырое значение `FAULT1_STS`.
    pub fault1_sts: u8,
    /// Сырое значение `FAULT2_STS`.
    pub fault2_sts: u8,
    /// Сырое значение `SAFETY_STS`.
    pub safety_sts: u8,
    /// Есть ли критичный отказ.
    pub critical_fault: u8,
    /// Зарезервировано.
    pub reserved: [u8; 2],
    /// Последний измеренный входной ток, мкА.
    pub iin_ua: u32,
    /// Последнее измеренное напряжение батареи, мкВ.
    pub vbat_uv: u32,
    /// Последнее измеренное напряжение входа, мкВ.
    pub vbus_uv: u32,
    /// Последняя температура кристалла, десятые °C.
    pub die_temp_dc: i32,
    /// Всего сеансов заряда.
    pub sessions: u64,
    /// Всего отсчётов телеметрии.
    pub samples: u64,
    /// Сколько записей в устройство выполнено.
    pub writes: u32,
    /// Сколько чтений выполнено.
    pub reads: u32,
    /// Код последней ошибки (0 — нет).
    pub last_error: i32,
}

/// Запрос [`IOCTL_LN8000_READ_REG`] и [`IOCTL_LN8000_WRITE_REG`].
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Ln8000RegRequest {
    /// Адрес регистра.
    pub addr: u8,
    /// Значение: вход для записи, выход для чтения.
    pub value: u8,
    /// Зарезервировано.
    pub reserved: [u8; 2],
    /// Код ошибки.
    pub error_code: i32,
}

/// Запрос [`IOCTL_LN8000_SET_LIMITS`].
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Ln8000LimitsRequest {
    /// Лимит входного тока, мкА (0 — не менять).
    pub iin_ua: u32,
    /// Целевое напряжение заряда, мкВ (0 — не менять).
    pub vbat_uv: u32,
    /// Фактически применённый ток, мкА.
    pub applied_iin_ua: u32,
    /// Код ошибки.
    pub error_code: i32,
}

/// Запрос [`IOCTL_LN8000_SET_MODE`].
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Ln8000ModeRequest {
    /// Желаемый режим (1 — standby, 2 — bypass, 3 — switching).
    ///
    /// `2` включается только в окне обхода (`Vin` 4,2…8 В): при повышенном
    /// напряжении возвращается `error_code = -20`
    /// ([`ERR_BYPASS_VIN_OUT_OF_WINDOW`]), потому что 1:1 подаёт вход прямо на
    /// батарею.
    pub mode: u8,
    /// Фактический режим после переключения.
    pub applied_mode: u8,
    /// Зарезервировано.
    pub reserved: [u8; 2],
    /// Код ошибки.
    pub error_code: i32,
}

/// Запрос явного старта/стопа заряда для [`IOCTL_LN8000_SET_CHARGE`].
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Ln8000ChargeRequest {
    /// 1 = start charging (switching mode), 0 = stop charging (standby).
    pub on: u8,
    /// Op mode actually reported by the chip after the attempt.
    pub applied_mode: u8,
    /// Raw `SYS_STS` read back from the chip.
    pub sys_sts: u8,
    /// Reserved.
    pub reserved: u8,
    /// Error code (0 = ok, negative = pump error).
    pub error_code: i32,
}

/// Сведения о сеансах для [`IOCTL_LN8000_GET_SESSIONS`].
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Ln8000Sessions {
    /// Сколько сеансов начато всего.
    pub total: u64,
    /// Длительность текущего сеанса, мс (0 — питания нет).
    pub current_ms: u64,
    /// Пиковый входной ток текущего сеанса, мкА.
    pub current_peak_iin_ua: u32,
    /// Была ли за текущий сеанс ускоренная зарядка.
    pub current_fast: u8,
    /// Зарезервировано.
    pub reserved: [u8; 3],
    /// Длительность последнего завершённого сеанса, мс.
    pub last_ms: u64,
    /// Пиковый ток последнего завершённого сеанса, мкА.
    pub last_peak_iin_ua: u32,
    /// Пиковая температура последнего сеанса, десятые °C.
    pub last_peak_temp_dc: i32,
    /// Была ли в последнем сеансе ускоренная зарядка.
    pub last_fast: u8,
    /// Зарезервировано.
    pub reserved2: [u8; 3],
}

/// Один отсчёт телеметрии в буфере [`IOCTL_LN8000_GET_SAMPLES`].
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Ln8000Sample {
    /// Метка времени, мс.
    pub ts_ms: u64,
    /// Напряжение батареи, мкВ.
    pub vbat_uv: u32,
    /// Напряжение входа, мкВ.
    pub vbus_uv: u32,
    /// Входной ток, мкА.
    pub iin_ua: u32,
    /// Температура кристалла, десятые °C.
    pub die_temp_dc: i32,
    /// Режим работы.
    pub op_mode: u8,
    /// Есть ли питание.
    pub input_present: u8,
    /// Зарезервировано.
    pub reserved: [u8; 2],
}

/// Запрос [`IOCTL_LN8000_GET_SAMPLES`].
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Ln8000SamplesRequest {
    /// Сколько отсчётов вернуть (по размеру буфера клиента).
    pub count: u32,
    /// Сколько отсчётов реально записано.
    pub available: u32,
    /// Первый отсчёт в буфере (идут подряд).
    pub first: Ln8000Sample,
}

/// Запрос [`IOCTL_LN8000_RUN_HVDCP`].
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Ln8000HvdcpRequest {
    /// 1 = run negotiate; 0 = report last soft state only (no bus).
    pub command: u8,
    /// Last APSD_STATUS after a run.
    pub apsd_status: u8,
    /// Last APSD_RESULT after a run.
    pub apsd_result: u8,
    /// Soft pulse count.
    pub pulse_cnt: u8,
    /// Machine phase code (`HvdcpPhase`).
    pub phase: u32,
    /// Target VBUS (µV), `2*VBAT+200mV`.
    pub target_vbus_uv: u32,
    /// Estimated adapter VBUS from soft pulse count (µV).
    pub estimated_vbus_uv: u32,
    /// 0 = ok; -10 = no SUPERUSER+Usbin; -11 open fail; -12 SPMI; -13 APSD timeout; -14 not QC.
    pub error_code: i32,
}
