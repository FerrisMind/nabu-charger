//! Карта регистров и кодирование значений LN8000.
//!
//! Источник — драйвер из эталонных исходников Android:
//! `drivers/power/supply/ti/ln8000_charger.h` и `ln8000_charger.c` (GPL,
//! `Lion Semiconductor` / `XiaoMi`, 2021). Все формулы кодирования повторяют
//! функции этого драйвера, чтобы поведение под Windows совпадало с Android.

/// Адрес регистра LN8000 (однобайтовый).
pub type RegAddr = u8;

// --- Регистры состояния -------------------------------------------------

/// Идентификатор устройства: ожидаемое значение — [`DEVICE_ID_VALUE`].
pub const DEVICE_ID: RegAddr = 0x00;
/// Ожидаемый ответ на чтение [`DEVICE_ID`].
pub const DEVICE_ID_VALUE: u8 = 0x42;
/// Маска прерываний.
pub const INT1: RegAddr = 0x01;
/// Разрешение прерываний.
pub const INT1_MSK: RegAddr = 0x02;
/// Состояние системы: режим работы и активные петли регулирования.
pub const SYS_STS: RegAddr = 0x03;
/// Состояние защит (температуры, обратный ток).
pub const SAFETY_STS: RegAddr = 0x04;
/// Первая группа отказов.
pub const FAULT1_STS: RegAddr = 0x05;
/// Вторая группа отказов.
pub const FAULT2_STS: RegAddr = 0x06;
/// Состояние тока.
pub const CURR1_STS: RegAddr = 0x07;
/// Состояние LDO и окончания заряда.
pub const LDO_STS: RegAddr = 0x08;
/// Первый регистр результата АЦП (каналы 1..10 идут подряд).
pub const ADC_FIRST_STS: RegAddr = 0x09;
/// Последний регистр результата АЦП.
pub const ADC_LAST_STS: RegAddr = 0x12;

// --- Управляющие регистры -----------------------------------------------

/// Ограничение входного тока.
pub const IIN_CTRL: RegAddr = 0x1B;
/// Управление петлями регулирования.
pub const REGULATION_CTRL: RegAddr = 0x1C;
/// Управление питанием.
pub const PWR_CTRL: RegAddr = 0x1D;
/// Режим работы (standby / bypass / switching).
pub const SYS_CTRL: RegAddr = 0x1E;
/// Управление LDO.
pub const LDO_CTRL: RegAddr = 0x1F;
/// Защита от выбросов, порог перенапряжения входа.
pub const GLITCH_CTRL: RegAddr = 0x20;
/// Разрешение защит.
pub const FAULT_CTRL: RegAddr = 0x21;
/// Порог NTC.
pub const NTC_CTRL: RegAddr = 0x22;
/// Управление АЦП (режим, задержка гибернации, старшие биты NTC).
pub const ADC_CTRL: RegAddr = 0x23;
/// Настройка АЦП.
pub const ADC_CFG: RegAddr = 0x24;
/// Автовосстановление после отказов.
pub const RECOVERY_CTRL: RegAddr = 0x25;
/// Таймеры (сторожевой и пауза АЦП).
pub const TIMER_CTRL: RegAddr = 0x26;

/// Бит паузы обновления АЦП в регистре [`TIMER_CTRL`] (бит 1).
///
/// Пока бит стоит, преобразования не переписывают результаты. Без паузы два байта
/// отсчёта могут относиться к разным преобразованиям, и температура получится
/// мусорной — а по ней принимается решение о защите. Эталонный драйвер делает
/// так же: ставит бит, читает пару, снимает бит.
pub const TIMER_CTRL_PAUSE_ADC: u8 = 1 << 1;
/// Pulse bit to clear latched fault/status (Android `ln8000_check_status`).
pub const TIMER_CTRL_CLEAR_LATCH: u8 = 1 << 2;
/// `FAULT_CTRL` bit: disable hardware `VIN_OV` (needed for QC 9–12 V bus).
pub const FAULT_CTRL_DISABLE_VIN_OV: u8 = 1 << 2;
/// `FAULT_CTRL` bit: disable hardware `VAC_UV` (`LN8000_BIT_DISABLE_VAC_UV`).
/// Required for saggy 5 V bricks (TA200) where Vin dips near Vbat under load.
pub const FAULT_CTRL_DISABLE_VAC_UV: u8 = 1 << 3;
/// `FAULT_CTRL` bit: disable hardware `VAC_OV` (`LN8000_BIT_DISABLE_VAC_OV`).
pub const FAULT_CTRL_DISABLE_VAC_OV: u8 = 1 << 4;
/// `FAULT_CTRL` bit: disable hardware `VBAT_OV` (`LN8000_BIT_DISABLE_VBAT_OV`).
/// Soft-mask near float so a latched OV does not block `volt_qual` / mode entry.
pub const FAULT_CTRL_DISABLE_VBAT_OV: u8 = 1 << 5;
/// Mask used before 5 V / TA200-class bypass: UV/OV that latch `FAULT1=0x21`.
pub const FAULT_CTRL_MASK_5V_BYPASS: u8 = FAULT_CTRL_DISABLE_VIN_OV
    | FAULT_CTRL_DISABLE_VAC_UV
    | FAULT_CTRL_DISABLE_VAC_OV
    | FAULT_CTRL_DISABLE_VBAT_OV;

/// Пороги.
pub const THRESHOLD_CTRL: RegAddr = 0x27;
/// Целевое напряжение заряда (float).
pub const V_FLOAT_CTRL: RegAddr = 0x28;
/// Признак инициализации и управление зарядом.
pub const CHARGE_CTRL: RegAddr = 0x29;
/// Служебный регистр (разблокировка, soft-reset).
pub const LION_CTRL: RegAddr = 0x30;
/// Операции встроенного контроллера, группа 1.
pub const BC_OP_1: RegAddr = 0x41;
/// Операции встроенного контроллера, группа 2 (soft-reset).
pub const BC_OP_2: RegAddr = 0x42;
/// Состояние встроенного контроллера, группа A.
pub const BC_STS_A: RegAddr = 0x49;
/// Состояние встроенного контроллера, группа E.
pub const BC_STS_E: RegAddr = 0x4D;

/// Значение для разблокировки служебных регистров.
pub const LION_CTRL_UNLOCK: u8 = 0xC6;

// --- Биты SYS_STS -------------------------------------------------------

/// Активна петля регулирования входного тока.
pub const SYS_STS_IIN_LOOP: u8 = 1 << 7;
/// Активна петля регулирования напряжения заряда.
pub const SYS_STS_VFLOAT_LOOP: u8 = 1 << 6;
/// Включён режим bypass (1:1).
pub const SYS_STS_BYPASS_ENABLED: u8 = 1 << 3;
/// Включён режим switching (2:1).
pub const SYS_STS_SWITCHING_ENABLED: u8 = 1 << 2;
/// Устройство в standby.
pub const SYS_STS_STANDBY: u8 = 1 << 1;
/// Устройство выключено.
pub const SYS_STS_SHUTDOWN: u8 = 1 << 0;

// --- Биты SYS_CTRL ------------------------------------------------------

/// Бит разрешения standby.
pub const SYS_CTRL_STANDBY_EN: u8 = 1 << 3;
/// Бит обнаружения обратного тока.
pub const SYS_CTRL_REV_IIN_DET: u8 = 1 << 2;
/// Бит включения режима 1:1 (bypass).
pub const SYS_CTRL_EN_1TO1: u8 = 1 << 0;

// --- Биты SAFETY_STS / FAULT / LDO --------------------------------------

/// Достигнута максимальная температура кристалла.
pub const SAFETY_TEMP_MAX: u8 = 1 << 6;
/// Активна температурная регулировка.
pub const SAFETY_TEMP_REGULATION: u8 = 1 << 5;
/// Сработал аларм NTC.
pub const SAFETY_NTC_ALARM: u8 = 1 << 4;
/// Сработала защита NTC.
pub const SAFETY_NTC_SHUTDOWN: u8 = 1 << 3;
/// Обнаружен обратный входной ток.
pub const SAFETY_REV_IIN: u8 = 1 << 2;

/// Истёк сторожевой таймер.
pub const FAULT1_WATCHDOG: u8 = 1 << 7;
/// Перенапряжение батареи.
pub const FAULT1_VBAT_OV: u8 = 1 << 6;
/// Блок питания отключён.
pub const FAULT1_VAC_UNPLUG: u8 = 1 << 4;
/// Перенапряжение входа (VAC).
pub const FAULT1_VAC_OV: u8 = 1 << 3;
/// Перенапряжение VIN.
pub const FAULT1_VIN_OV: u8 = 1 << 1;

/// Групповой признак «напряженческих» отказов в `FAULT1` (биты 6:0).
///
/// Вендор тестирует группу целиком (`LN8000_MASK_VFAULTS`, `.h:65`), а не
/// отдельные биты: `volt_qual = !(FAULT1 & 0x7F)` — вход считается годным
/// только при полностью чистых битах 6:0 (`.c:592-604`). Биты 5, 2 и 0 в
/// публичном драйвере не названы, поэтому своих имён мы им не даём: живой
/// `FAULT1=0x21` — это два безымянных бита группы, и любое имя для них было бы
/// выдумкой.
pub const FAULT1_VFAULTS_MASK: u8 = 0x7F;

/// Обнаружено превышение входного тока.
pub const FAULT2_IIN_OC: u8 = 1 << 7;

/// Вторая ступень вендорского теста годности входа: бит 5 в `FAULT2`.
///
/// Проверяется только тогда, когда первая ступень (`FAULT1`) чиста **и** заряд
/// разрешён (`ln8000_check_status`, `.c:592-604`).
pub const FAULT2_VOLT_FAULT: u8 = 1 << 5;

/// Заряд завершён.
pub const LDO_CHARGE_TERM: u8 = 1 << 5;
/// Требуется дозаряд (recharge).
pub const LDO_RECHARGE: u8 = 1 << 4;

// --- Числовые константы -------------------------------------------------

/// Минимум кодирования напряжения заряда, мкВ.
pub const VBAT_FLOAT_MIN_UV: u32 = 3_725_000;
/// Максимум кодирования напряжения заряда, мкВ.
pub const VBAT_FLOAT_MAX_UV: u32 = 5_000_000;
/// Шаг кодирования напряжения заряда, мкВ.
pub const VBAT_FLOAT_STEP_UV: u32 = 5_000;

/// Минимальный входной ток (по документации драйвера), мкА.
pub const IIN_MIN_UA: u32 = 500_000;
/// Шаг кодирования входного тока, мкА.
pub const IIN_STEP_UA: u32 = 50_000;
/// Максимум поля входного тока (7 бит), мкА.
pub const IIN_MAX_UA: u32 = IIN_STEP_UA * 0x7F;

/// Порог перенапряжения входа: 6.5 В.
pub const VAC_OVP_6V5: u8 = 0x0;
/// Порог перенапряжения входа: 11 В.
pub const VAC_OVP_11V: u8 = 0x1;
/// Порог перенапряжения входа: 12 В.
pub const VAC_OVP_12V: u8 = 0x2;
/// Порог перенапряжения входа: 13 В.
pub const VAC_OVP_13V: u8 = 0x3;

/// Настройка защиты NTC по температуре: −16 LSB (≈ −4.3 °C).
pub const NTC_SHUTDOWN_CFG: u8 = 2;
/// Порог аларма NTC по умолчанию (≈ +40 °C).
pub const NTC_ALARM_DEFAULT: u16 = 226;

/// Значение регистра порогов из драйвера (`THRESHOLD_CTRL`).
pub const THRESHOLD_CTRL_DEFAULT: u8 = 0x0E;
/// Задержка перезапуска после сброса, мс (в драйвере — `msleep(5 * 2)`).
pub const SOFT_RESET_DELAY_MS: u64 = 10;
