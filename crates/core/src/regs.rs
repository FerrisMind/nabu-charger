//! Карта регистров периферии USBIN зарядника SMB.
//!
//! Значения взяты из эталонного драйвера Android
//! (`drivers/power/supply/qcom/smb5-reg.h`, `smb-reg.h`) ветки 16.0 для `nabu`,
//! чтобы поведение под Windows совпадало с поведением под Android.

/// Адрес регистра в пространстве периферии (16 бит).
pub type RegAddr = u16;

/// База периферии USBIN.
pub const USBIN_BASE: RegAddr = 0x1300;

/// `APSD_STATUS`: состояние автомата детекции адаптера.
pub const APSD_STATUS: RegAddr = USBIN_BASE + 0x07;
/// Бит «детекция завершена».
pub const APSD_DTC_STATUS_DONE: u8 = 1 << 0;
/// Бит «подключён адаптер с поддержкой Quick Charge».
pub const QC_CHARGER: u8 = 1 << 1;
/// Бит «адаптер подключился слишком медленно».
pub const SLOW_PLUGIN_TIMEOUT: u8 = 1 << 5;
/// Бит «проверка HVDCP не уложилась в таймаут».
pub const HVDCP_CHECK_TIMEOUT: u8 = 1 << 6;
/// Восьмой бит `APSD_STATUS` (назначение вендорское, учитывается для полноты).
pub const APSD_STATUS_7: u8 = 1 << 7;

/// `APSD_RESULT_STATUS`: результат детекции (тип адаптера).
pub const APSD_RESULT_STATUS: RegAddr = USBIN_BASE + 0x08;
/// Маска значимых битов результата.
pub const APSD_RESULT_STATUS_MASK: u8 = 0b0111_1111;
/// Служебный восьмой бит результата.
pub const APSD_RESULT_STATUS_7: u8 = 1 << 7;

/// `QC_CHANGE_STATUS`: состояние переговоров Quick Charge.
pub const QC_CHANGE_STATUS: RegAddr = USBIN_BASE + 0x09;

/// `USBIN_CMD_IL`: управление входом (в том числе приостановка).
pub const USBIN_CMD_IL: RegAddr = USBIN_BASE + 0x40;
/// Бит приостановки входа.
pub const USBIN_SUSPEND: u8 = 1 << 0;

/// `CMD_APSD`: команды автомату детекции.
pub const CMD_APSD: RegAddr = USBIN_BASE + 0x41;
/// Бит повторного запуска детекции.
pub const APSD_RERUN: u8 = 1 << 0;

/// `CMD_ICL_OVERRIDE`: принудительный лимит входного тока.
pub const CMD_ICL_OVERRIDE: RegAddr = USBIN_BASE + 0x42;
/// Бит включения принудительного лимита.
pub const ICL_OVERRIDE: u8 = 1 << 0;
/// Бит «применять лимит после завершения APSD».
pub const ICL_OVERRIDE_AFTER_APSD: u8 = 1 << 4;

/// `CMD_HVDCP_2`: управление режимом HVDCP2.
pub const CMD_HVDCP_2: RegAddr = USBIN_BASE + 0x43;

/// `USBIN_ADAPTER_ALLOW_OVERRIDE`: переопределение разрешённых типов адаптера.
pub const USBIN_ADAPTER_ALLOW_OVERRIDE: RegAddr = USBIN_BASE + 0x44;

/// `USB_CMD_PULLDOWN`: управление подтяжками D+/D−.
pub const USB_CMD_PULLDOWN: RegAddr = USBIN_BASE + 0x45;

/// `HVDCP_PULSE_COUNT_MAX`: выбор напряжения для QC2.
pub const HVDCP_PULSE_COUNT_MAX: RegAddr = USBIN_BASE + 0x5B;
/// Маска выбора напряжения QC2 (биты 7:6).
pub const QC2_VOLTAGE_MASK: u8 = 0b1100_0000;

/// `USBIN_ICL_OPTIONS`: дополнительные опции лимита тока.
pub const USBIN_ICL_OPTIONS: RegAddr = USBIN_BASE + 0x66;

/// `USBIN_CURRENT_LIMIT_CFG`: код лимита входного тока (сетка 100 мА).
pub const USBIN_CURRENT_LIMIT_CFG: RegAddr = USBIN_BASE + 0x70;

/// Образец `APSD_RESULT_STATUS` для SDP (стандартный порт).
pub const PATTERN_SDP: u8 = 1 << 0;
/// Образец для OCP (прочий порт).
pub const PATTERN_OCP: u8 = 1 << 1;
/// Образец для CDP (порт зарядки и данных).
pub const PATTERN_CDP: u8 = 1 << 2;
/// Образец для DCP (порт только зарядки).
pub const PATTERN_DCP: u8 = 1 << 3;
/// Образец для FLOAT (нестандартный источник).
pub const PATTERN_FLOAT: u8 = 1 << 4;
/// Бит Quick Charge 2.0 в образце.
pub const PATTERN_QC_2P0: u8 = 1 << 5;
/// Бит Quick Charge 3.0 в образце.
pub const PATTERN_QC_3P0: u8 = 1 << 6;

/// Образец для HVDCP2: DCP плюс признак QC2.0.
pub const PATTERN_HVDCP2: u8 = PATTERN_DCP | PATTERN_QC_2P0;
/// Образец для HVDCP3: DCP плюс признак QC3.0.
pub const PATTERN_HVDCP3: u8 = PATTERN_DCP | PATTERN_QC_3P0;
