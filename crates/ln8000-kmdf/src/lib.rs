//! KMDF-драйвер charge pump LN8000 (Xiaomi Pad 5, `nabu`), Windows on ARM64.
//!
//! # Что делает драйвер
//!
//! 1. Встаёт на существующий узел ACPI `PEIC` (`_HID = QCOM057E`) — это внешняя
//!    ИС LN8000 на шине I²C с адресом 0x51 (см. `09-acpi-nabu/ln8000-acpi-uefi.md`).
//! 2. В `EvtDevicePrepareHardware` разбирает `_CRS`, достаёт идентификатор
//!    подключения и открывает шину через Resource Hub: путь
//!    `\Device\RESOURCE_HUB\<16 hex>` (правило из `reshub.h`).
//! 3. Проверяет `DEVICE_ID`, настраивает пороги и защиты, включает режим 2:1.
//! 4. Таймером периодически снимает телеметрию (напряжения, ток, температуру),
//!    ведёт журнал сеансов заряда и применяет защиту по температуре и току.
//! 5. Отдаёт состояние и журнал пользовательскому режиму через IOCTL.
//!
//! # Почему ARM64
//!
//! Целевое устройство — планшет под Snapdragon 860; сборка идёт только под
//! `aarch64-pc-windows-msvc` (см. `.cargo/config.toml` рядом).
//!
//! # Состояние проверки
//!
//! Ядро логики ([`ln8000`]) покрыто тестами на хосте. Обвязка WDF собирается
//! `cargo wdk build`; проверка на планшете описана в `docs/DEPLOY-LN8000.md`.

#![no_std]
#![deny(missing_docs)]
#![allow(clippy::missing_safety_doc)]

mod battery;
mod hvdcp;
mod ioctl;
mod spb;
mod spb_abi;
mod sysreq;

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Код остановки `MANUALLY_INITIATED_CRASH`: «драйвер сам остановил систему».
///
/// Своё значение не заводим: у этого кода параметры вольные, а номер читается
/// однозначно, в отличие от самодельного.
const BUGCHECK_MANUALLY_INITIATED_CRASH: u32 = 0x0000_00E2;

/// Магия паники (`LN80`) — отличает наш вызов от любого чужого `0xE2`.
const PANIC_MAGIC: usize = 0x4C4E_3830;

unsafe extern "C" {
    /// Останавливает систему с диагностируемым кодом (`ntoskrnl`).
    fn KeBugCheckEx(
        bugcheck_code: u32,
        parameter1: usize,
        parameter2: usize,
        parameter3: usize,
        parameter4: usize,
    ) -> !;
}

/// FNV-1a от имени файла: в параметры остановки влезает число, а не строка.
const fn file_hash(path: &str) -> usize {
    let bytes = path.as_bytes();
    let mut hash: u32 = 0x811C_9DC5;
    let mut index = 0;
    while index < bytes.len() {
        hash ^= bytes[index] as u32;
        hash = hash.wrapping_mul(0x0100_0193);
        index += 1;
    }
    hash as usize
}

/// Обработчик паники: вместо бесконечного цикла — аварийная остановка.
///
/// Штатный `wdk-panic 0.4.1` крутится в `loop {}` (в его исходнике так и
/// написано: `FIXME: Should this trigger Bugcheck via KeBugCheckEx?`). Цикл на
/// месте паники — это зависший процессор без единой зацепки и с удержанными
/// блокировками; дампы 18.09 пришлось разбирать по RVA вручную. Остановка
/// оставляет и модуль, и точное место: `Arg1` — магия `LN80`, `Arg2` — строка,
/// `Arg3` — колонка, `Arg4` — FNV-1a от имени файла.
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    let (line, column, file_hash_value) = match info.location() {
        Some(location) => (
            location.line() as usize,
            location.column() as usize,
            file_hash(location.file()),
        ),
        None => (0, 0, 0),
    };
    // SAFETY: `KeBugCheckEx` вызывается на любом уровне IRQL и не возвращает
    // управление; печать не делаем — на месте паники она может не пройти.
    unsafe {
        KeBugCheckEx(
            BUGCHECK_MANUALLY_INITIATED_CRASH,
            PANIC_MAGIC,
            line,
            column,
            file_hash_value,
        )
    }
}

use ioctl::{
    Ln8000ChargeRequest, Ln8000HvdcpRequest, Ln8000LimitsRequest, Ln8000ModeRequest, Ln8000RegRequest,
    Ln8000Sample, Ln8000SamplesRequest, Ln8000Sessions, Ln8000Status, LN8000_STATUS_MAGIC,
    LN8000_STATUS_VERSION,
};
use ln8000::encoding::decode_iin_limit;
use ln8000::{
    bypass_allowed_by_vin, bypass_strikes_expired, charge_mode, evaluate, regs, resolve_bypass,
    AdcChannel, BypassResolution, GuardAction, GuardLimits, OpMode, Pump, PumpConfig, PumpError,
    PumpState, Telemetry, TelemetrySample, SWITCHING_MIN_VIN_UV,
};
use spb::SpbBus;
use wdk::println;
use wdk_sys::{
    _WDF_EXECUTION_LEVEL::WdfExecutionLevelPassive,
    _WDF_IO_QUEUE_DISPATCH_TYPE::WdfIoQueueDispatchSequential,
    _WDF_REQUEST_SEND_OPTIONS_FLAGS::{
        WDF_REQUEST_SEND_OPTION_SYNCHRONOUS, WDF_REQUEST_SEND_OPTION_TIMEOUT,
    },
    _WDF_SYNCHRONIZATION_SCOPE::WdfSynchronizationScopeNone, _WDF_TRI_STATE::WdfTrue,
    call_unsafe_wdf_function_binding, CmResourceTypeConnection, NTSTATUS, PCUNICODE_STRING,
    PLUGPLAY_REGKEY_DEVICE, PWDFDEVICE_INIT, ULONG, UNICODE_STRING, WDFCMRESLIST, WDFDEVICE, WDFDRIVER,
    WDFIOTARGET, WDFKEY, WDFMEMORY, WDFQUEUE, WDFREQUEST, WDFTIMER, WDF_DRIVER_CONFIG,
    WDF_IO_QUEUE_CONFIG, WDF_NO_HANDLE, WDF_NO_OBJECT_ATTRIBUTES, WDF_OBJECT_ATTRIBUTES,
    WDF_PNPPOWER_EVENT_CALLBACKS, WDF_REQUEST_SEND_OPTIONS, WDF_TIMER_CONFIG,
};

/// Интерфейс устройства для пользовательского режима.
///
/// `{B0A1C3D2-4E5F-4A6B-8C7D-9E0F1A2B3C4D}`
pub const GUID_DEVINTERFACE_LN8000: wdk_sys::GUID = wdk_sys::GUID {
    Data1: 0xB0A1_C3D2,
    Data2: 0x4E5F,
    Data3: 0x4A6B,
    Data4: [0x8C, 0x7D, 0x9E, 0x0F, 0x1A, 0x2B, 0x3C, 0x4D],
};

/// Нижняя граница периода телеметрии, мс (реестр `TelemetryMs`).
const TELEMETRY_MS_MIN: u32 = 100;

/// Период повторных попыток автозапуска, мс.
const CHARGE_RETRY_MS: u64 = 30_000;

/// Изменение входа, при котором попытка повторяется сразу, мкВ.
const CHARGE_RETRY_DELTA_UV: u32 = 300_000;

/// Минимальный выигрыш отклонения шины от цели, считающийся прогрессом, мкВ.
///
/// Меньше шага QC3 (200 мВ) с запасом: 50 мВ отделяет настоящий сдвиг от
/// дрожания АЦП, но не требует угадать реальную крутизну импульса.
const WINDOW_PROGRESS_UV: u32 = 50_000;

/// Сколько коррекций без прогресса терпит петля окна до редкого повтора.
const WINDOW_STALL_MAX: u32 = 4;

/// Интервал коррекции шины в застое, мс.
///
/// Застой означает, что импульсы QC3 не меняют вход (не-QC3 адаптер) либо шаг
/// блока мельче ожидаемого. Редкий повтор оставляет попытку, но снимает
/// постоянную нагрузку на SPMI.
const WINDOW_STALL_MS: u64 = 60_000;

/// Окно наблюдения пика входного тока для метки `MaxIinUa`, мс.
const IIN_WINDOW_MS: u64 = 5_000;
/// Период опроса топливного счётчика PM8150B, мс.
///
/// Счётчик — медленная величина: один сырой шаг это ~0,4 % ёмкости, а одна
/// транзакция SUPERUSER на этой платформе идёт около двух секунд. Опрос каждые
/// 250 мс (сборка `.627`) занимал шину почти постоянно: сторонний user-mode
/// читатель получал `ERROR_GEN_FAILURE` в 79 попытках из 100 (замер 18.09), и
/// приёмка оставалась без `0x1307`/`0x1506`. Раз в 30 с шина свободна ~94 %,
/// а индикатору заряда этого хватает с запасом.
const GAUGE_POLL_MS: u32 = 30_000;

// Короткие числовые коды метки `EngageState` — состояние «есть ли вход и режим».
/// Входа нет (`Vin` ниже порога присутствия).
const ENGAGE_NO_INPUT: u32 = 0;
/// Вход есть, заряда/режима нет (standby).
const ENGAGE_STANDBY: u32 = 1;
/// Включён обход 1:1.
const ENGAGE_BYPASS: u32 = 2;
/// Включён режим 2:1.
const ENGAGE_SWITCHING: u32 = 3;
/// Вход повышен, но 2:1 физически не тянет (`Vin < 2*Vbat + 250 мВ`),
/// а обход при таком напряжении запрещён.
const ENGAGE_NO_HEADROOM: u32 = 4;

/// Сколько отказов подряд считать защёлкнутым состоянием чипа.
///
/// Чип защёлкивает отказ, если режим запрошен при невалидном входе, и после этого
/// отказывает даже при нормальном входе. Штатный выход — программный сброс
/// и повторная настройка, как это делает эталонный драйвер при потере обмена.
const CHARGE_FAILS_BEFORE_RESET: u32 = 3;

/// Значение метки `ProfSel`, когда параметра `ProtectionProfile` в реестре нет.
const PROFILE_NOT_SET: u32 = u32::MAX;

/// Состояние драйвера: единственный экземпляр устройства.
///
/// # Инварианты
///
/// Поля трогает либо последовательная очередь управляющих запросов, либо
/// таймер телеметрии; WDF сериализует их между собой, поэтому одновременного
/// доступа нет. Именно поэтому `Sync` объявляется вручную.
struct DriverState {
    pump: Option<Pump<SpbBus>>,
    telemetry: Telemetry,
    limits: GuardLimits,
    writes: u32,
    reads: u32,
    last_error: i32,
    actions: u32,
    /// Период телеметрии из реестра, мс.
    telemetry_ms: u32,
    /// Время последней попытки автозапуска заряда, мс.
    last_charge_attempt_ms: u64,
    /// Напряжение входа при последней попытке, мкВ.
    last_attempt_vbus_uv: u32,
    /// Сколько раз автозапуск включал заряд.
    auto_starts: u32,
    /// Отказов заряда подряд: считаем, чтобы понять про защёлкнутое состояние.
    failed_attempts: u32,
    /// Сколько всего попыток включить заряд (`ChargeAttemptN`).
    charge_attempts: u32,
    /// Монотонное время последней попытки включить заряд (`LastEnableMs`).
    last_enable_ms: u64,
    /// Пик входного тока за текущее окно наблюдения, мкА (`MaxIinUa`).
    max_iin_ua: u32,
    /// Начало текущего окна наблюдения пика тока, мс.
    iin_window_start_ms: u64,
    /// Сколько тактов подряд защита просит 1:1, а Vin его не допускает.
    ///
    /// Нужен, чтобы на повышенном Vin не крутить standby каждый такт: первый такт
    /// снижает ток, со следующего (`BYPASS_DENIED_STRIKES_BEFORE_STOP`) — останов.
    bypass_denied_strikes: u32,
    /// Usbin SPMI connection id from `_CRS` (`None` on stock ACPI; HVDCP uses SUPERUSER).
    usbin_id: Option<u64>,
    /// Soft HVDCP / QC pulse state (software `pulse_cnt`).
    hvdcp: hvdcp::HvdcpState,
    /// SUPERUSER open failed at PrepareHardware — retry from telemetry.
    hvdcp_retry_pending: bool,
    /// How many SUPERUSER retries have been attempted.
    hvdcp_retry_attempts: u32,
    /// Monotonic deadline (ms) for the next SUPERUSER retry.
    hvdcp_retry_next_ms: u64,
    /// Last cable-present sample (Vin > unplug floor) for re-plug edge detect.
    last_input_present: bool,
    /// Edge detect armed after PrepareHardware autostart (avoids double-negotiate).
    hvdcp_edge_armed: bool,
    /// Монотонное время последней коррекции шины под окно 2:1, мс.
    ///
    /// Окно едет за банкой (полоса `[2*Vbat+200, 2*Vbat+400]` мВ), поэтому без
    /// периодической коррекции шина остаётся там, где её оставило согласование:
    /// при росте Vbat она уходит выше полосы, режим 3 сохраняется, а перенос
    /// падает до 39 мА. Интервал — [`hvdcp::WINDOW_NUDGE_MS`], чтобы не грузить
    /// SPMI на каждом такте.
    last_window_nudge_ms: u64,
    /// Лучшее (наименьшее) отклонение шины от цели в текущем эпизоде, мкВ.
    ///
    /// `u32::MAX` — эпизод не начат (шина в полосе). Нужно, чтобы отличить
    /// коррекцию, которая работает, от застоя: у PD-адаптера импульсы QC3
    /// ничего не меняют, и петля должна перейти на редкий повтор, а не сыпать
    /// импульсами в SPMI вечно.
    window_best_err_uv: u32,
    /// Коррекций без прогресса подряд в текущем эпизоде.
    window_stall_n: u32,
}

// SAFETY: см. инварианты выше — доступ сериализован WDF.
unsafe impl Sync for DriverState {}

impl DriverState {
    const fn new() -> Self {
        Self {
            pump: None,
            telemetry: Telemetry::new(),
            limits: GuardLimits::standard(),
            writes: 0,
            reads: 0,
            last_error: 0,
            actions: 0,
            telemetry_ms: 250,
            last_charge_attempt_ms: 0,
            last_attempt_vbus_uv: 0,
            auto_starts: 0,
            failed_attempts: 0,
            charge_attempts: 0,
            last_enable_ms: 0,
            max_iin_ua: 0,
            iin_window_start_ms: 0,
            bypass_denied_strikes: 0,
            usbin_id: None,
            hvdcp: hvdcp::HvdcpState::new(),
            hvdcp_retry_pending: false,
            hvdcp_retry_attempts: 0,
            hvdcp_retry_next_ms: 0,
            last_input_present: false,
            hvdcp_edge_armed: false,
            last_window_nudge_ms: 0,
            window_best_err_uv: u32::MAX,
            window_stall_n: 0,
        }
    }
}

/// Единственный экземпляр состояния драйвера.
static mut STATE: DriverState = DriverState::new();

/// Доступ к состоянию драйвера.
///
/// # Safety
///
/// Вызывается только пока драйвер держит [`lock_state`]: очередь сериализует
/// одни IOCTL, а таймер телеметрии и `EvtDevicePrepareHardware` идут своими
/// контекстами.
unsafe fn state() -> &'static mut DriverState {
    // SAFETY: см. инварианты `DriverState`.
    unsafe { &mut *core::ptr::addr_of_mut!(STATE) }
}

/// Мьютекс состояния: шина и `STATE` — на одного пользователя.
///
/// KMUTEX, а не WDFWAITLOCK: уровень обратного вызова устройства не задан
/// (`WdfDeviceInitSetExecutionLevel` в биндингах нет), а KMUTEX документирован
/// и для `PASSIVE_LEVEL`, и для `APC_LEVEL` — вызовы драйвера заведомо не выше,
/// они делают синхронную отправку WDF.
static mut STATE_LOCK: wdk_sys::KMUTEX = unsafe { core::mem::zeroed() };

/// Мьютекс инициализирован. До инициализации брать его нельзя: нулевая
/// структура диспетчера — ещё не объект ожидания.
static STATE_LOCK_READY: AtomicBool = AtomicBool::new(false);

/// Монотонное время последнего опроса топливного счётчика, мс (0 — не было).
///
/// Отдельный атомик, а не поле [`DriverState`]: счётчик читается **до** захвата
/// [`STATE_LOCK`] — транзакция SUPERUSER идёт около двух секунд, и держать на
/// это время мьютекс значит тормозить и управление зарядом, и `IOCTL_STATUS`,
/// которым пользуется приёмка. Сравнение с обменом (CAS) заодно не даёт двум
/// тактам таймера читать шину одновременно.
static GAUGE_LAST_MS: AtomicU64 = AtomicU64::new(0);

/// Железо подготовлено: счётчик можно опрашивать.
///
/// `read_gauge_if_due` вызывается до захвата [`STATE_LOCK`] и потому не защищён
/// мьютексом от гонки с `evt_release_hardware`; этот флаг и есть та защита.
static GAUGE_READY: AtomicBool = AtomicBool::new(false);

/// Что дал такт опроса счётчика — три разных исхода, которые нельзя смешивать.
enum GaugePoll {
    /// Такт пропущен по расписанию: читать рано.
    Skipped,
    /// Сырое значение счётчика.
    Raw(u8),
    /// Чтение не удалось: шина занята, счётчик не ответил, копии не совпали.
    Failed,
}

/// Читает топливный счётчик не чаще [`GAUGE_POLL_MS`] и только из одного такта.
///
/// # Safety
///
/// Пассивный уровень; вызывается до захвата [`STATE_LOCK`], состояние не трогает.
unsafe fn read_gauge_if_due() -> GaugePoll {
    if !GAUGE_READY.load(Ordering::Acquire) {
        return GaugePoll::Skipped;
    }
    let device = unsafe { DEVICE };
    if device.is_null() {
        return GaugePoll::Skipped;
    }
    let now = monotonic_ms();
    let last = GAUGE_LAST_MS.load(Ordering::Acquire);
    if last != 0 && now.saturating_sub(last) < u64::from(GAUGE_POLL_MS) {
        return GaugePoll::Skipped;
    }
    if GAUGE_LAST_MS
        .compare_exchange(last, now, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        // Другой такт уже взял это окно себе.
        return GaugePoll::Skipped;
    }
    // SAFETY: `device` жив (проверено выше), уровень пассивный.
    match unsafe { crate::spb::read_batt_soc_raw(device) } {
        Some(raw) => GaugePoll::Raw(raw),
        None => GaugePoll::Failed,
    }
}

/// Инициализирует мьютекс состояния. Один раз, из `evt_device_add`.
unsafe fn init_state_lock() {
    if STATE_LOCK_READY.load(Ordering::Acquire) {
        return;
    }
    // SAFETY: `Level` зарезервирован и обязан быть нулём.
    unsafe { wdk_sys::ntddk::KeInitializeMutex(core::ptr::addr_of_mut!(STATE_LOCK), 0) };
    STATE_LOCK_READY.store(true, Ordering::Release);
}

/// Держит мьютекс состояния и отпускает его при выходе из области видимости.
struct StateGuard {
    /// Мьютекс захвачен этим потоком.
    held: bool,
}

impl Drop for StateGuard {
    fn drop(&mut self) {
        if self.held {
            // SAFETY: мьютекс захвачен этим же потоком в `lock_state`.
            unsafe {
                let _ = wdk_sys::ntddk::KeReleaseMutex(core::ptr::addr_of_mut!(STATE_LOCK), 0);
            }
        }
    }
}

/// Мьютекс брошен завершившимся потоком. Владение всё равно достаётся нам,
/// и отпускать его надо так же, как обычный захват.
const STATUS_ABANDONED: i32 = 0x0000_0080;

/// Захватывает мьютекс состояния, дожидаясь другого контекста.
///
/// Без него таймер телеметрии, `EvtDevicePrepareHardware` и обработчики IOCTL
/// ходят в один и тот же кэшированный `WDFREQUEST` внутри `SpbBus`, а WDF
/// запрещает отправлять запрос дважды: 18.09 это дало три дампа
/// `WDF_VIOLATION (0x10D)` с `Arg2 = 3` («запрос уже отправлен I/O-таргету»).
///
/// Возврат ожидания нельзя толковать как «успех — всё, что не отрицательно».
/// Нулевой относительный таймаут (указатель на `QuadPart = 0`) означает не
/// «ждать вечно», а «опросить и вернуться сразу»: при занятом мьютексе вызов
/// отдаёт `STATUS_TIMEOUT` (`0x102`) — положительный код, но владения нет. На
/// этом драйвер и упал 18.09 в 17:08: `KeReleaseMutex` на чужом мьютексе
/// поднимает `STATUS_MUTANT_NOT_OWNED` (`0xC0000046`), что в контексте
/// `powershell.exe` дало `SYSTEM_SERVICE_EXCEPTION (0x3B)` со стеком
/// `nt!KeReleaseMutantEx` ← `nt!KeReleaseMutex` ← `ln8000_kmdf+0xb774`.
/// Поэтому таймаут — `NULL` (ждать, пока держатель отпустит; держатель всегда
/// отпускает сам, а обмен по шине ограничен секундой), а захватом считается
/// только `STATUS_SUCCESS` или `STATUS_ABANDONED`.
fn lock_state() -> StateGuard {
    if !STATE_LOCK_READY.load(Ordering::Acquire) {
        return StateGuard { held: false };
    }
    // SAFETY: мьютекс инициализирован; режим ядра, без APC, пассивный уровень;
    // `NULL` вместо таймаута — ждать без ограничения.
    let status = unsafe {
        wdk_sys::ntddk::KeWaitForSingleObject(
            core::ptr::addr_of_mut!(STATE_LOCK).cast(),
            wdk_sys::_KWAIT_REASON::Executive,
            // `KernelMode` из `_MODE` — `i32`, а `KPROCESSOR_MODE` — `CCHAR`.
            wdk_sys::_MODE::KernelMode as core::ffi::c_char,
            0,
            core::ptr::null_mut(),
        )
    };
    StateGuard {
        held: status == wdk_sys::STATUS_SUCCESS || status == STATUS_ABANDONED,
    }
}

/// Символическая ссылка для пользовательского режима: `\\.\nabu_ln8000`.
///
/// Ширина буфера была 20 символов при имени в 22 — имя молча обрезалось
/// до `\DosDevices\nabu_ln8`, и утилита не могла открыть устройство.
const SYMLINK_CHARS: usize = 24;
const SYMLINK_NAME: &str = "/DosDevices/nabu_ln8000";
const SYMLINK: [u16; SYMLINK_CHARS] = utf16_lit(SYMLINK_NAME);

/// Длина имени симлинка в байтах: без завершающего нуля.
const SYMLINK_BYTES: u16 = (SYMLINK_NAME.len() as u16) * 2;

/// Имя обязано помещаться в буфер — иначе оно обрежется и устройство не откроется.
const _: () = assert!(
    SYMLINK_NAME.len() <= SYMLINK_CHARS,
    "имя симлинка длиннее буфера"
);

/// Этапы добавления устройства: пишутся в реестр меткой, чтобы сбой запуска
/// был виден удалённо, а не только в отладочном выводе драйвера.
const STAGE_ENTER: u32 = 10;const STAGE_DEVICE: u32 = 11;
const STAGE_INTERFACE: u32 = 12;
const STAGE_SYMLINK: u32 = 13;
const STAGE_QUEUE: u32 = 14;
const STAGE_TIMER: u32 = 15;
const STAGE_DONE: u32 = 0xFF;
/// Этапы подготовки железа: видно, где именно отвалилась шина или чип.
const STAGE_PREPARE: u32 = 30;
const STAGE_PREPARE_BUS: u32 = 31;
const STAGE_PREPARE_CHIP: u32 = 32;
const STAGE_PREPARE_CONFIG: u32 = 33;
const STAGE_READY: u32 = 0xFE;

/// Собирает UTF-16 без завершающего нуля: символы `'/'` заменяются на `'\\'`.
const fn utf16_lit(ascii: &str) -> [u16; SYMLINK_CHARS] {
    let bytes = ascii.as_bytes();
    let mut out = [0_u16; SYMLINK_CHARS];
    let mut index = 0;
    while index < bytes.len() && index < SYMLINK_CHARS {
        let byte = bytes[index];
        out[index] = if byte == b'/' { b'\\' as u16 } else { byte as u16 };
        index += 1;
    }
    out
}

/// Права на чтение ключа реестра (`KEY_READ`).
const KEY_READ: ULONG = 0x0002_0019;

/// Имена параметров и их значения из реестра: то, что читает драйвер.
struct DriverParams {
    config: PumpConfig,
    limits: GuardLimits,
    telemetry_ms: u32,
}

/// Заполняет буфер символами строки и возвращает длину.
fn utf16_into(buffer: &mut [u16], text: &str) -> usize {
    let mut len = 0_usize;
    for byte in text.as_bytes() {
        if let Some(slot) = buffer.get_mut(len) {
            *slot = u16::from(*byte);
            len = len.saturating_add(1);
        }
    }
    len
}

/// Читает один параметр типа `REG_DWORD` из открытого ключа.
///
/// # Safety
///
/// `key` — валидный дескриптор ключа, открытый на чтение.
unsafe fn query_ulong(key: WDFKEY, name: &str) -> Option<u32> {
    let mut buffer = [0_u16; 32];
    let len = utf16_into(&mut buffer, name);
    if len == 0 || len > buffer.len() {
        return None;
    }
    let length = u16::try_from(len.saturating_mul(2)).unwrap_or(0);
    let value_name = UNICODE_STRING {
        Length: length,
        MaximumLength: length,
        Buffer: buffer.as_mut_ptr(),
    };
    let mut value: u32 = 0;
    // SAFETY: ключ и буфер имени живут до конца вызова; значение — локальная переменная.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(WdfRegistryQueryULong, key, &raw const value_name, &raw mut value)
    };
    if status < 0 { None } else { Some(value) }
}

/// Права на запись ключа реестра (`KEY_SET_VALUE`).
const KEY_SET_VALUE: ULONG = 0x0002;

/// Пишет одно числовое значение в уже открытый ключ.
fn write_one(key: WDFKEY, name: &str, value: u32) {
    let mut buffer = [0_u16; 32];
    let len = utf16_into(&mut buffer, name);
    if len == 0 {
        return;
    }
    let length = u16::try_from(len.saturating_mul(2)).unwrap_or(0);
    let value_name = UNICODE_STRING {
        Length: length,
        MaximumLength: length,
        Buffer: buffer.as_mut_ptr(),
    };
    // SAFETY: ключ открыт на запись, имя — локальный буфер.
    unsafe {
        let _ = call_unsafe_wdf_function_binding!(
            WdfRegistryAssignULong,
            key,
            &raw const value_name,
            value,
        );
    }
}

/// Пишет пару числовых меток в уже открытый ключ.
fn write_marker(key: WDFKEY, stage_name: &str, status_name: &str, stage: u32, status: NTSTATUS) {
    write_one(key, stage_name, stage);
    write_one(key, status_name, status as u32);
}

/// Метка этапа добавления устройства — в ключ устройства.
///
/// В отладочный вывод драйвера без отладчика не заглянуть, поэтому ход
/// добавления устройства виден в `Device Parameters` узла: `AddStage` —
/// где остановились, `AddStatus` — с каким кодом. Успех — `AddStage = 0xFF`.
fn mark_stage(device: WDFDEVICE, stage: u32, status: NTSTATUS) {
    let mut key: WDFKEY = WDF_NO_HANDLE.cast();
    // SAFETY: устройство создано WDF; ключ открывается на запись.
    let opened = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceOpenRegistryKey,
            device,
            PLUGPLAY_REGKEY_DEVICE,
            KEY_SET_VALUE,
            WDF_NO_OBJECT_ATTRIBUTES,
            &raw mut key,
        )
    };
    if opened < 0 {
        return;
    }
    write_marker(key, "AddStage", "AddStatus", stage, status);
    // SAFETY: ключ открыт выше и больше не нужен.
    unsafe {
        call_unsafe_wdf_function_binding!(WdfRegistryClose, key);
    }
}

/// Метка загрузки драйвера — в ключ `Parameters` службы драйвера.
///
/// `DriverStage = 1` означает, что `DriverEntry` дошёл до конца и вызвал
/// `WdfDriverCreate`. Это отличает «драйвер не загрузился» от «упал при
/// добавлении устройства»: во втором случае метка есть, а устройство — нет.
fn mark_driver(driver: WDFDRIVER, stage: u32, status: NTSTATUS) {
    let mut key: WDFKEY = WDF_NO_HANDLE.cast();
    // SAFETY: дескриптор драйвера создан WDF.
    let opened = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDriverOpenParametersRegistryKey,
            driver,
            KEY_SET_VALUE,
            WDF_NO_OBJECT_ATTRIBUTES,
            &raw mut key,
        )
    };
    if opened < 0 {
        return;
    }
    write_marker(key, "DriverStage", "DriverStatus", stage, status);
    // SAFETY: ключ открыт выше и больше не нужен.
    unsafe {
        call_unsafe_wdf_function_binding!(WdfRegistryClose, key);
    }
}

/// Записывает одно значение в ключ `Parameters` драйвера — для разовой диагностики.
fn mark_driver_value(driver: WDFDRIVER, name: &str, value: u32) {
    let mut key: WDFKEY = WDF_NO_HANDLE.cast();
    // SAFETY: дескриптор драйвера создан WDF.
    let opened = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDriverOpenParametersRegistryKey,
            driver,
            KEY_SET_VALUE,
            WDF_NO_OBJECT_ATTRIBUTES,
            &raw mut key,
        )
    };
    if opened < 0 {
        return;
    }
    write_one(key, name, value);
    // SAFETY: ключ открыт выше и больше не нужен.
    unsafe {
        call_unsafe_wdf_function_binding!(WdfRegistryClose, key);
    }
}

/// Записывает одно значение в ключ устройства — для разовой диагностики.
pub(crate) fn mark_device_value(device: WDFDEVICE, name: &str, value: u32) {
    let mut key: WDFKEY = WDF_NO_HANDLE.cast();
    // SAFETY: устройство создано WDF; ключ открывается на запись.
    let opened = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceOpenRegistryKey,
            device,
            PLUGPLAY_REGKEY_DEVICE,
            KEY_SET_VALUE,
            WDF_NO_OBJECT_ATTRIBUTES,
            &raw mut key,
        )
    };
    if opened < 0 {
        return;
    }
    write_one(key, name, value);
    // SAFETY: ключ открыт выше и больше не нужен.
    unsafe {
        call_unsafe_wdf_function_binding!(WdfRegistryClose, key);
    }
}

/// Длина входа запроса подключения (восемь байт, как у эталонного клиента).
const ATTACH_INPUT_LEN: usize = 8;

/// Отправляет запрос подключения (`0x32C004`) в цель родителя устройства.
///
/// Эталонный клиент делает этот шаг до доступа к регистрам, и отправляет его
/// не в узел Resource Hub, а в собственную цель устройства. Возвращает статус;
/// первые слова ответа пишутся в реестр как доказательство.
unsafe fn parent_attach_probe(device: WDFDEVICE) -> i32 {
    // SAFETY: устройство создано; цель принадлежит WDF.
    let target: WDFIOTARGET = unsafe { call_unsafe_wdf_function_binding!(WdfDeviceGetIoTarget, device) };
    if target.is_null() {
        return -3;
    }

    let mut request: WDFREQUEST = WDF_NO_HANDLE.cast();
    // SAFETY: цель валидна; дескрипторы — локальные переменные под выход.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfRequestCreate,
            WDF_NO_OBJECT_ATTRIBUTES,
            target,
            &raw mut request,
        )
    };
    if status < 0 {
        return status;
    }

    let mut in_ptr: *mut core::ffi::c_void = core::ptr::null_mut();
    let mut in_mem: WDFMEMORY = WDF_NO_HANDLE.cast();
    // SAFETY: вход подключения — восемь байт в невыгружаемом пуле.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfMemoryCreate,
            WDF_NO_OBJECT_ATTRIBUTES,
            wdk_sys::_POOL_TYPE::NonPagedPool,
            0,
            ATTACH_INPUT_LEN,
            &raw mut in_mem,
            &raw mut in_ptr,
        )
    };
    if status < 0 {
        return status;
    }
    let mut out_ptr: *mut core::ffi::c_void = core::ptr::null_mut();
    let mut out_mem: WDFMEMORY = WDF_NO_HANDLE.cast();
    // SAFETY: ответ подключения — 1024 байта в невыгружаемом пуле.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfMemoryCreate,
            WDF_NO_OBJECT_ATTRIBUTES,
            wdk_sys::_POOL_TYPE::NonPagedPool,
            0,
            spb_abi::ATTACH_REPLY_LEN,
            &raw mut out_mem,
            &raw mut out_ptr,
        )
    };
    if status < 0 {
        return status;
    }

    // Эталонный клиент кладёт во вход магию и четырёхбайтовое значение.
    // SAFETY: вход — восемь байт, запись в его пределах.
    unsafe {
        core::ptr::write_volatile(in_ptr.cast::<u32>(), spb_abi::ATTACH_MAGIC);
        core::ptr::write_volatile(in_ptr.add(4).cast::<u32>(), 1);
    }

    // SAFETY: цель, запрос и буферы валидны.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfIoTargetFormatRequestForIoctl,
            target,
            request,
            spb_abi::IOCTL_ATTACH,
            in_mem,
            core::ptr::null_mut(),
            out_mem,
            core::ptr::null_mut(),
        )
    };
    if status < 0 {
        return status;
    }

    let mut options: WDF_REQUEST_SEND_OPTIONS = unsafe { core::mem::zeroed() };
    options.Size = size_of_ulong::<WDF_REQUEST_SEND_OPTIONS>();
    options.Flags =
        (WDF_REQUEST_SEND_OPTION_SYNCHRONOUS | WDF_REQUEST_SEND_OPTION_TIMEOUT) as ULONG;
    options.Timeout = -10_000_000_i64;
    // SAFETY: синхронная отправка на пассивном уровне.
    let sent = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfRequestSend,
            request,
            target,
            &raw mut options,
        )
    };
    if sent == 0 {
        return -1;
    }
    // SAFETY: запрос завершён, читаем статус и первые слова ответа.
    let status = unsafe { call_unsafe_wdf_function_binding!(WdfRequestGetStatus, request) };
    if status >= 0 {
        for (index, name) in [(0_usize, "Par0"), (1, "Par1"), (2, "Par2"), (3, "Par3")] {
            // SAFETY: ответ 1024 байта; читаем первые четыре слова.
            let word = unsafe { core::ptr::read_volatile(out_ptr.add(index * 4).cast::<u32>()) };
            mark_device_value(device, name, word);
        }
    }
    // SAFETY: объекты созданы здесь и больше не нужны.
    unsafe {
        call_unsafe_wdf_function_binding!(WdfObjectDelete, request.cast());
        call_unsafe_wdf_function_binding!(WdfObjectDelete, in_mem.cast());
        call_unsafe_wdf_function_binding!(WdfObjectDelete, out_mem.cast());
    }
    status
}

/// Класс и типы ресурсов подключения из заголовка WDM.
const CONNECTION_CLASS_SERIAL: u32 = 0x02;
const CONNECTION_TYPE_SERIAL_I2C: u32 = 0x01;
const CONNECTION_TYPE_SERIAL_SPI: u32 = 0x02;
/// Vendor SerialBusType SPMI в сыром ACPI (`0xC1`); после трансляции RH
/// часто сохраняется как Type подключения.
const CONNECTION_TYPE_SERIAL_SPMI_VENDOR: u32 = 0xC1;

/// Адрес `APSD_STATUS` на периферии USBIN PM8150B (SID 2).
const USBIN_APSD_STATUS: u16 = 0x1307;

/// Пишет сведения о ресурсе подключения в реестр (разбор на железе).
fn mark_connection(device: WDFDEVICE, index: usize, id: u64, class: u32, kind: u32) {
    let (class_name, type_name, id_name) = match index {
        0 => ("C0Class", "C0Type", "C0Low"),
        1 => ("C1Class", "C1Type", "C1Low"),
        2 => ("C2Class", "C2Type", "C2Low"),
        _ => ("C3Class", "C3Type", "C3Low"),
    };
    mark_device_value(device, class_name, class);
    mark_device_value(device, type_name, kind);
    mark_device_value(device, id_name, id as u32);
}

/// Собирает все ресурсы подключения из `_CRS` в порядке объявления.
///
/// # Safety
///
/// Список ресурсов валиден; `out` — локальный буфер вызывающего.
unsafe fn collect_connections(resources: WDFCMRESLIST, out: &mut [(u64, u32, u32); 4]) -> usize {
    let mut index: ULONG = 0;
    let mut found = 0_usize;
    loop {
        // SAFETY: список ресурсов неизменен на время перебора.
        let descriptor = unsafe {
            call_unsafe_wdf_function_binding!(WdfCmResourceListGetDescriptor, resources, index)
        };
        if descriptor.is_null() {
            break;
        }
        // SAFETY: дескриптор получен из списка ресурсов.
        let kind = unsafe { (*descriptor).Type };
        if u32::from(kind) == CmResourceTypeConnection {
            // SAFETY: для типа Connection поле `Connection` заполнено.
            let class = unsafe { (*descriptor).u.Connection.Class };
            let connection_kind = unsafe { (*descriptor).u.Connection.Type };
            let low = unsafe { (*descriptor).u.Connection.IdLowPart };
            let high = unsafe { (*descriptor).u.Connection.IdHighPart };
            if found < out.len() {
                out[found] = (
                    (u64::from(high) << 32) | u64::from(low),
                    u32::from(class),
                    u32::from(connection_kind),
                );
                found = found.saturating_add(1);
            }
        }
        index = index.saturating_add(1);
    }
    found
}

/// Выбирает идентификатор I²C-подключения для LN8000.
///
/// Классы и типы — из `wdm.h`: `CLASS_SERIAL` = 0x02, `TYPE_SERIAL_I2C` = 0x01.
/// Сначала ищем I²C; если его нет — любой serial (SPI), чтобы не ломать
/// диагностику на нестандартных оверлеях.
fn select_i2c_connection(connections: &[(u64, u32, u32); 4], count: usize) -> Option<u64> {
    for (id, class, kind) in connections.iter().take(count) {
        if *class == CONNECTION_CLASS_SERIAL && *kind == CONNECTION_TYPE_SERIAL_I2C {
            return Some(*id);
        }
    }
    for (id, class, kind) in connections.iter().take(count) {
        if *class == CONNECTION_CLASS_SERIAL && *kind == CONNECTION_TYPE_SERIAL_SPI {
            return Some(*id);
        }
    }
    None
}

/// Выбирает второе serial-подключение — кандидат на USBIN SPMI после ACPI-оверлея.
///
/// В стоковом DSDT у PEIC только I²C → возвращает `None`. После SSDT с дескриптором
/// SID=2 / periph `0x13` появляется второе подключение (часто Type=`0xC1`).
fn select_usbin_connection(
    connections: &[(u64, u32, u32); 4],
    count: usize,
    i2c_id: u64,
) -> Option<u64> {
    for (id, class, kind) in connections.iter().take(count) {
        if *id == i2c_id || *class != CONNECTION_CLASS_SERIAL {
            continue;
        }
        if *kind == CONNECTION_TYPE_SERIAL_SPMI_VENDOR || *kind != CONNECTION_TYPE_SERIAL_I2C {
            return Some(*id);
        }
    }
    for (id, class, _) in connections.iter().take(count) {
        if *class == CONNECTION_CLASS_SERIAL && *id != i2c_id {
            return Some(*id);
        }
    }
    None
}

/// Пробует SPMI-чтение `APSD_STATUS` (0x1307) через второе подключение PEIC.
///
/// Без ACPI-оверлея `usbin_id` = `None` → только метка `UsbinOpen=0xFFFFFFFF`.
///
/// # Safety
///
/// Пассивный уровень; устройство создано.
unsafe fn probe_usbin_spmi(device: WDFDEVICE, usbin_id: Option<u64>) {
    let Some(id) = usbin_id else {
        mark_device_value(device, "UsbinOpen", 0xFFFF_FFFF);
        return;
    };
    // SAFETY: пассивный уровень, устройство создано.
    let mut bus = match unsafe { SpbBus::open(device, id, true) } {
        Ok(bus) => {
            mark_device_value(device, "UsbinOpen", 1);
            bus
        }
        Err(_) => {
            mark_device_value(device, "UsbinOpen", 0);
            return;
        }
    };
    bus.set_variant(0);
    for (big_endian, st_name, val_name) in [
        (true, "UsbinBeSt", "UsbinBeVal"),
        (false, "UsbinLeSt", "UsbinLeVal"),
    ] {
        match bus.transact_spmi16(USBIN_APSD_STATUS, None, big_endian) {
            Ok(value) => {
                mark_device_value(device, st_name, 0);
                mark_device_value(device, val_name, u32::from(value));
            }
            Err(_) => {
                mark_device_value(device, st_name, bus.last_status() as u32);
                mark_device_value(device, val_name, 0xFFFF_FFFF);
            }
        }
    }
}

/// Пробует прочитать регистр чипа, отправив последовательность SPB в цель
/// родителя устройства (стек контроллера), а не в узел Resource Hub.
///
/// Эталонный клиент хранит дескриптор цели в глобальной переменной и строит
/// список передач так же, как мы; вопрос был именно в том, куда идёт запрос.
unsafe fn parent_sequence_probe(device: WDFDEVICE, address: u8) -> i32 {
    // SAFETY: устройство создано; цель принадлежит WDF.
    let target: WDFIOTARGET = unsafe { call_unsafe_wdf_function_binding!(WdfDeviceGetIoTarget, device) };
    if target.is_null() {
        return -3;
    }

    let mut request: WDFREQUEST = WDF_NO_HANDLE.cast();
    // SAFETY: цель валидна; дескриптор — локальная переменная под выход.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfRequestCreate,
            WDF_NO_OBJECT_ATTRIBUTES,
            target,
            &raw mut request,
        )
    };
    if status < 0 {
        return status;
    }

    // Список передач: ровно столько, сколько передач — как в эталоне.
    let list_len = spb_abi::SpbTransferList::area_size(2);
    let mut list_ptr: *mut core::ffi::c_void = core::ptr::null_mut();
    let mut list_mem: WDFMEMORY = WDF_NO_HANDLE.cast();
    // SAFETY: выделяем невыгружаемый буфер под список передач.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfMemoryCreate,
            WDF_NO_OBJECT_ATTRIBUTES,
            wdk_sys::_POOL_TYPE::NonPagedPool,
            0,
            list_len,
            &raw mut list_mem,
            &raw mut list_ptr,
        )
    };
    if status < 0 {
        return status;
    }

    let mut data_ptr: *mut core::ffi::c_void = core::ptr::null_mut();
    let mut data_mem: WDFMEMORY = WDF_NO_HANDLE.cast();
    // SAFETY: буфер данных — регистр плюс байт ответа.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfMemoryCreate,
            WDF_NO_OBJECT_ATTRIBUTES,
            wdk_sys::_POOL_TYPE::NonPagedPool,
            0,
            8,
            &raw mut data_mem,
            &raw mut data_ptr,
        )
    };
    if status < 0 {
        return status;
    }

    // SAFETY: оба буфера выделены выше и имеют достаточный размер.
    unsafe {
        core::ptr::write_volatile(data_ptr.cast::<u8>(), address);
        let list = list_ptr.cast::<spb_abi::SpbTransferList>();
        (*list).size = u32::try_from(spb_abi::SpbTransferList::header_size()).unwrap_or(0);
        (*list).reserved = 0;
        (*list).transfer_count = 2;
        (*list).transfers[0] = spb_abi::entry_init(
            spb_abi::SPB_DIRECTION_TO_DEVICE,
            data_ptr,
            1,
        );
        let second = core::ptr::addr_of_mut!((*list).transfers[0]).add(1);
        *second = spb_abi::entry_init(
            spb_abi::SPB_DIRECTION_FROM_DEVICE,
            data_ptr.add(1),
            1,
        );
    }

    // SAFETY: цель, запрос и буферы валидны.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfIoTargetFormatRequestForIoctl,
            target,
            request,
            spb_abi::IOCTL_SPB_EXECUTE_SEQUENCE,
            list_mem,
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            core::ptr::null_mut(),
        )
    };
    if status < 0 {
        return status;
    }

    let mut options: WDF_REQUEST_SEND_OPTIONS = unsafe { core::mem::zeroed() };
    options.Size = size_of_ulong::<WDF_REQUEST_SEND_OPTIONS>();
    options.Flags =
        (WDF_REQUEST_SEND_OPTION_SYNCHRONOUS | WDF_REQUEST_SEND_OPTION_TIMEOUT) as ULONG;
    options.Timeout = -10_000_000_i64;
    // SAFETY: синхронная отправка на пассивном уровне.
    let sent = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfRequestSend,
            request,
            target,
            &raw mut options,
        )
    };
    if sent == 0 {
        return -1;
    }
    // SAFETY: запрос завершён; читаем статус и прочитанный байт.
    let status = unsafe { call_unsafe_wdf_function_binding!(WdfRequestGetStatus, request) };
    if status >= 0 {
        // SAFETY: буфер данных — 8 байт; ответ лежит вторым.
        let value = unsafe { core::ptr::read_volatile(data_ptr.add(1).cast::<u8>()) };
        mark_device_value(device, "ParSeqValue", u32::from(value));
    }
    // SAFETY: объекты созданы здесь и больше не нужны.
    unsafe {
        call_unsafe_wdf_function_binding!(WdfObjectDelete, request.cast());
        call_unsafe_wdf_function_binding!(WdfObjectDelete, list_mem.cast());
        call_unsafe_wdf_function_binding!(WdfObjectDelete, data_mem.cast());
    }
    status
}

/// Читает параметры профиля из реестра устройства (`HKR, Parameters, ...`).
///
/// Так тот, кто ставит драйвер, может менять пороги, не пересобирая его: INF
/// задаёт значения по умолчанию, а здесь они применяются с проверкой границ.
/// Неверное значение не портит профиль — оно отклоняется и остаётся прежнее.
///
/// # Safety
///
/// Пассивный уровень IRQL, устройство уже создано.
unsafe fn read_parameters(device: WDFDEVICE) -> DriverParams {
    let mut config = PumpConfig::for_qc35_class_b();
    let mut limits = GuardLimits::standard();
    // Профильный лимит тока для возврата после полосы среза: снимок `config`,
    // потому что `set_iin_limit` перезаписывает `config.iin_limit_ua` каждой
    // уставкой (защита, ICL сессии) и «профильное» значение теряется.
    limits.iin_profile_ua = config.iin_limit_ua;
    let mut telemetry_ms = 250_u32;
    // Применённое значение `ProtectionProfile`: `None` — параметра в реестре нет
    // и остаётся профиль кода (`for_qc35_class_b`, петли включены).
    let mut protection_profile: Option<u32> = None;

    let mut device_key: WDFKEY = WDF_NO_HANDLE.cast();
    // SAFETY: устройство создано; ключ читается только на чтение.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceOpenRegistryKey,
            device,
            PLUGPLAY_REGKEY_DEVICE,
            KEY_READ,
            WDF_NO_OBJECT_ATTRIBUTES,
            &raw mut device_key,
        )
    };
    if status < 0 {
        println!("ln8000-kmdf: ключ устройства не открылся ({status:#010X}); беру профиль по умолчанию");
        mark_profile(device, &config, protection_profile);
        return DriverParams {
            config,
            limits,
            telemetry_ms,
        };
    }

    let mut params_key: WDFKEY = WDF_NO_HANDLE.cast();
    let mut buffer = [0_u16; 32];
    let len = utf16_into(&mut buffer, "Parameters");
    let length = u16::try_from(len.saturating_mul(2)).unwrap_or(0);
    let subkey_name = UNICODE_STRING {
        Length: length,
        MaximumLength: length,
        Buffer: buffer.as_mut_ptr(),
    };
    // SAFETY: ключ устройства валиден; имя — локальный буфер.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfRegistryOpenKey,
            device_key,
            &raw const subkey_name,
            KEY_READ,
            WDF_NO_OBJECT_ATTRIBUTES,
            &raw mut params_key,
        )
    };
    if status < 0 {
        println!("ln8000-kmdf: раздел Parameters не открылся ({status:#010X}); беру профиль по умолчанию");
        // SAFETY: ключ открыт выше и больше не нужен.
        unsafe {
            call_unsafe_wdf_function_binding!(WdfRegistryClose, device_key);
        }
        mark_profile(device, &config, protection_profile);
        return DriverParams {
            config,
            limits,
            telemetry_ms,
        };
    }

    for name in [
        // Профиль защиты применяется первым: он задаёт шаблон целиком,
        // и без этого явные уставки (ток, напряжение, порог) затирались бы
        // значениями шаблона — проверено на устройстве.
        "ProtectionProfile",
        "IinLimitUa",
        "VbatFloatUv",
        "VacOvpUv",
        "NtcAlarmCfg",
        "WatchdogEnabled",
    ] {
        // SAFETY: ключ Parameters открыт на чтение.
        if let Some(value) = unsafe { query_ulong(params_key, name) } {
            if config.apply_parameter(name, value) {
                if name == "ProtectionProfile" {
                    protection_profile = Some(value);
                }
            } else {
                println!("ln8000-kmdf: параметр {name} = {value} отклонён, остаётся значение по умолчанию");
            }
        }
    }
    // `IinLimitUa` из реестра — это и есть профильный лимит: к нему защита
    // возвращает ток после того, как напряжение и температура ушли из полосы.
    limits.iin_profile_ua = config.iin_limit_ua;
    // SAFETY: ключ Parameters открыт на чтение.
    if let Some(ms) = unsafe { query_ulong(params_key, "TelemetryMs") } {
        if (100..=60_000).contains(&ms) {
            telemetry_ms = ms;
        } else {
            println!("ln8000-kmdf: период телеметрии {ms} мс вне границ 100..60000, оставляю {telemetry_ms}");
        }
    }

    // Пороги защиты: если набор получится несогласованным, значение отклоняется.
    for name in [
        "TempReduceDc",
        "TempBypassDc",
        "TempStopDc",
        "IinMaxUa",
        "IinTargetUa",
        "IinFloorUa",
        "VbatReduceUv",
    ] {
        // SAFETY: ключ Parameters открыт на чтение.
        if let Some(value) = unsafe { query_ulong(params_key, name) } {
            if !limits.apply_parameter(name, value) {
                println!("ln8000-kmdf: порог {name} = {value} отклонён, остаётся прежний");
            }
        }
    }

    println!(
        "ln8000-kmdf: пороги защиты: {} / {} / {} (0.1 °C), ток {} мкА, цель {} мкА",
        limits.temp_reduce_dc, limits.temp_bypass_dc, limits.temp_stop_dc, limits.iin_max_ua, limits.iin_target_ua
    );

    println!(
        "ln8000-kmdf: профиль из реестра: ток {} мкА, напряжение {} мкВ, защитные петли насоса {}",
        config.iin_limit_ua,
        config.vbat_float_uv,
        if config.tdie_prot_disabled {
            "выключены (как в Device Tree)"
        } else {
            "включены"
        }
    );

    // SAFETY: оба ключа открыты выше и больше не нужны.
    unsafe {
        call_unsafe_wdf_function_binding!(WdfRegistryClose, params_key);
        call_unsafe_wdf_function_binding!(WdfRegistryClose, device_key);
    }

    mark_profile(device, &config, protection_profile);

    DriverParams {
        config,
        limits,
        telemetry_ms,
    }
}

/// Точка входа драйвера.
///
/// # Safety
///
/// Вызывается ядром; аргументы валидны по контракту WDF.
#[unsafe(no_mangle)]
#[unsafe(link_section = "INIT")]
pub unsafe extern "system" fn DriverEntry(
    driver: &mut wdk_sys::DRIVER_OBJECT,
    registry_path: PCUNICODE_STRING,
) -> NTSTATUS {
    println!("ln8000-kmdf: DriverEntry");

    // BattC WMI QueryWmiRegInfo needs a durable registry path (simbatt).
    // SAFETY: `registry_path` is valid for the duration of DriverEntry; we copy.
    unsafe { battery::set_registry_path(registry_path) };

    let mut config = WDF_DRIVER_CONFIG {
        Size: size_of_ulong::<WDF_DRIVER_CONFIG>(),
        EvtDriverDeviceAdd: Some(evt_device_add),
        ..unsafe { core::mem::zeroed() }
    };
    config.Size = size_of_ulong::<WDF_DRIVER_CONFIG>();

    let mut driver_handle: WDFDRIVER = WDF_NO_HANDLE.cast();
    // SAFETY: конфигурация заполнена, дескриптор — локальная переменная.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDriverCreate,
            core::ptr::from_mut(driver).cast::<wdk_sys::DRIVER_OBJECT>(),
            registry_path,
            WDF_NO_OBJECT_ATTRIBUTES,
            &raw mut config,
            &raw mut driver_handle,
        )
    };
    if status < 0 {
        println!("ln8000-kmdf: WdfDriverCreate не удался: {status:#010X}");
    } else {
        // В поле состояния пишем секунды с загрузки системы: по ним видно,
        // относится метка к текущему запуску или осталась с прошлого.
        let mut stamp: u64 = 0;
        // SAFETY: KeQueryInterruptTimePrecise — документированная функция ядра.
        let ticks = unsafe { wdk_sys::ntddk::KeQueryInterruptTimePrecise(&raw mut stamp) };
        mark_driver(driver_handle, 1, (ticks / 10_000_000) as i32);
    }
    status
}

/// Создаёт устройство, интерфейс и очередь управляющих запросов.
///
/// # Safety
///
/// Вызывается WDF на пассивном уровне.
unsafe extern "C" fn evt_device_add(
    driver: WDFDRIVER,
    mut device_init: PWDFDEVICE_INIT,
) -> NTSTATUS {
    // SAFETY: самый первый обратный вызов драйвера: мьютекс нужен раньше, чем
    // заработают таймер телеметрии и очередь IOCTL.
    unsafe { init_state_lock() };
    // Первая метка идёт в ключ драйвера: он доступен ещё до создания
    // устройства, поэтому виден даже отказ самого первого вызова.
    mark_driver(driver, STAGE_ENTER, 0);
    // Проба: размер структуры атрибутов, которую строит драйвер. Значение
    // нужно, чтобы видеть, чем именно наша структура не подошла KMDF.
    mark_driver_value(driver, "AttrSize", size_of_ulong::<WDF_OBJECT_ATTRIBUTES>());

    // Обработчики PnP: подготовка и освобождение ресурсов.
    let mut pnp = WDF_PNPPOWER_EVENT_CALLBACKS {
        Size: size_of_ulong::<WDF_PNPPOWER_EVENT_CALLBACKS>(),
        EvtDevicePrepareHardware: Some(evt_prepare_hardware),
        EvtDeviceReleaseHardware: Some(evt_release_hardware),
        ..unsafe { core::mem::zeroed() }
    };
    pnp.Size = size_of_ulong::<WDF_PNPPOWER_EVENT_CALLBACKS>();
    // SAFETY: `device_init` предоставлен WDF до создания устройства.
    unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceInitSetPnpPowerEventCallbacks,
            device_init,
            &raw mut pnp,
        );
    }

    // BattC must see DEVICE_CONTROL + SYSTEM_CONTROL before the WDF queue (simbatt).
    // SAFETY: `device_init` still owned by the driver here.
    let status = unsafe { battery::assign_ioctl_preprocess(device_init) };
    if status < 0 {
        println!("ln8000-kmdf: battery preprocess failed: {status:#010X}");
        mark_driver(driver, STAGE_DEVICE, status);
        return status;
    }

    let mut device: WDFDEVICE = WDF_NO_HANDLE.cast();
    // Атрибуты не передаём: NULL — штатный вариант WDF. Нашу структуру KMDF
    // здесь отвергает со STATUS_WDF_OBJECT_ATTRIBUTES_INVALID (0xC0200209),
    // проверено на планшете; ни контекст, ни колбэки устройству не нужны.
    // SAFETY: `device_init` валиден.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceCreate,
            &raw mut device_init,
            WDF_NO_OBJECT_ATTRIBUTES,
            &raw mut device,
        )
    };
    if status < 0 {
        println!("ln8000-kmdf: WdfDeviceCreate не удался: {status:#010X}");
        mark_driver(driver, STAGE_DEVICE, status);
        return status;
    }
    mark_driver(driver, STAGE_DEVICE, 0);

    // Интерфейс, по которому пользовательский режим находит драйвер.
    // SAFETY: устройство создано; GUID — статическая константа.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceCreateDeviceInterface,
            device,
            &GUID_DEVINTERFACE_LN8000,
            core::ptr::null_mut(),
        )
    };
    if status < 0 {
        println!("ln8000-kmdf: не удалось создать интерфейс: {status:#010X}");
        mark_driver(driver, STAGE_INTERFACE, status);
        return status;
    }

    // Символическая ссылка: диагностической утилите достаточно открыть
    // \\.\nabu_ln8000 — без перечисления интерфейсов через SetupAPI.
    let mut link = UNICODE_STRING {
        Length: SYMLINK_BYTES,
        MaximumLength: SYMLINK_BYTES,
        Buffer: SYMLINK.as_ptr().cast_mut(),
    };
    // SAFETY: имя — статический буфер UTF-16, устройство создано.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceCreateSymbolicLink,
            device,
            &raw mut link,
        )
    };
    if status < 0 {
        // Симлинк — удобство, а не условие работы: драйвер живёт и без него.
        println!("ln8000-kmdf: символическая ссылка не создана: {status:#010X}");
        mark_driver(driver, STAGE_SYMLINK, status);
        // Отдельное имя: основная метка перезаписывается следующими этапами.
        mark_driver_value(driver, "LinkStatus", status as u32);
    }

    // Очередь управляющих запросов и таймер телеметрии.
    let mut queue_config = WDF_IO_QUEUE_CONFIG {
        Size: size_of_ulong::<WDF_IO_QUEUE_CONFIG>(),
        // Очередь по умолчанию: сюда попадают управляющие запросы клиента.
        // Без этого флага они уходят в очередь WDF по умолчанию и завершаются
        // статусом «неизвестная функция» (клиент видит код ошибки 1).
        DefaultQueue: 1,
        DispatchType: WdfIoQueueDispatchSequential,
        PowerManaged: WdfTrue,
        EvtIoDeviceControl: Some(evt_io_device_control),
        ..unsafe { core::mem::zeroed() }
    };
    queue_config.Size = size_of_ulong::<WDF_IO_QUEUE_CONFIG>();

    let mut queue: WDFQUEUE = WDF_NO_HANDLE.cast();
    // SAFETY: конфигурация заполнена, устройство создано.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfIoQueueCreate,
            device,
            &raw mut queue_config,
            WDF_NO_OBJECT_ATTRIBUTES,
            &raw mut queue,
        )
    };
    if status < 0 {
        println!("ln8000-kmdf: WdfIoQueueCreate не удался: {status:#010X}");
        mark_driver(driver, STAGE_QUEUE, status);
        return status;
    }

    // One-shot timer: Period is fixed at Create and cannot track registry
    // `TelemetryMs`. Re-arm each tick from `st.telemetry_ms` (was: Period=1000
    // while DueTime used 250 → UI saw plug/unplug only ~once per second).
    let mut timer_config = WDF_TIMER_CONFIG {
        Size: size_of_ulong::<WDF_TIMER_CONFIG>(),
        Period: 0,
        EvtTimerFunc: Some(evt_telemetry_timer),
        ..unsafe { core::mem::zeroed() }
    };
    timer_config.Size = size_of_ulong::<WDF_TIMER_CONFIG>();
    // Автоматическая сериализация: колбэк не пересекается с очередью.
    timer_config.AutomaticSerialization = 1;

    let mut timer_attributes = WDF_OBJECT_ATTRIBUTES {
        Size: size_of_ulong::<WDF_OBJECT_ATTRIBUTES>(),
        ParentObject: device.cast(),
        ExecutionLevel: WdfExecutionLevelPassive,
        SynchronizationScope: WdfSynchronizationScopeNone,
        ..unsafe { core::mem::zeroed() }
    };
    timer_attributes.Size = size_of_ulong::<WDF_OBJECT_ATTRIBUTES>();
    // Владелец обязателен: без ParentObject WDF отказывает со
    // STATUS_WDF_PARENT_NOT_SPECIFIED (0xC0200212) — проверено на планшете.

    let mut timer: WDFTIMER = WDF_NO_HANDLE.cast();
    // SAFETY: конфигурация и атрибуты заполнены.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfTimerCreate,
            &raw mut timer_config,
            &raw mut timer_attributes,
            &raw mut timer,
        )
    };
    if status < 0 {
        // Таймер — не условие работы: без него управление и диагностика живы,
        // опрос идёт по запросу. Причина отказа в заголовке wdftimer.h:
        // автоматическая сериализация требует совместимости с DISPATCH, а обмену
        // по шине нужен PASSIVE. Правильный вариант — таймер уровня DISPATCH со
        // отложенной работой на PASSIVE; это отдельная доработка.
        println!("ln8000-kmdf: WdfTimerCreate не удался: {status:#010X}");
        mark_driver(driver, STAGE_TIMER, status);
    }

    // Запоминаем таймер в статике: его запускает prepare_hardware, когда шина готова.
    // SAFETY: единственный экземпляр устройства, доступ сериализован.
    unsafe {
        TIMER = timer;
        DEVICE = device;
    }

    println!("ln8000-kmdf: устройство готово");
    mark_driver(driver, STAGE_DONE, 0);
    mark_stage(device, STAGE_DONE, 0);
    wdk_sys::STATUS_SUCCESS
}

/// Таймер телеметрии: единственный экземпляр на драйвер.
static mut TIMER: WDFTIMER = core::ptr::null_mut();
/// Устройство для колбэка таймера (родитель таймера = device; храним явно).
static mut DEVICE: WDFDEVICE = core::ptr::null_mut();

/// Разбирает `_CRS`, открывает шину и настраивает LN8000.
///
/// # Safety
///
/// Вызывается WDF на пассивном уровне; списки ресурсов валидны.
unsafe extern "C" fn evt_prepare_hardware(
    device: WDFDEVICE,
    _resources_raw: WDFCMRESLIST,
    resources_translated: WDFCMRESLIST,
) -> NTSTATUS {
    // Держим состояние и шину до конца подготовки: таймер телеметрии уже
    // создан и может тикать параллельно.
    let _state = lock_state();
    // 1. Ищем ресурс подключения (I²C) и забираем идентификатор.
    mark_stage(device, STAGE_PREPARE, 0);
    let mut connections = [(0_u64, 0_u32, 0_u32); 4];
    // SAFETY: список ресурсов валиден, буфер — локальный.
    let connection_count = unsafe { collect_connections(resources_translated, &mut connections) };
    mark_device_value(device, "ConnCount", u32::try_from(connection_count).unwrap_or(0));
    for (index, (id, class, kind)) in connections.iter().take(connection_count).enumerate() {
        mark_connection(device, index, *id, *class, *kind);
    }
    let peripheral_id = match select_i2c_connection(&connections, connection_count) {
        Some(id) => id,
        None => {
            println!("ln8000-kmdf: в _CRS нет последовательного подключения (I2C/SPI)");
            mark_stage(device, STAGE_PREPARE_BUS, wdk_sys::STATUS_DEVICE_NOT_READY);
            return wdk_sys::STATUS_DEVICE_NOT_READY;
        }
    };
    let usbin_id = select_usbin_connection(&connections, connection_count, peripheral_id);
    match usbin_id {
        Some(id) => {
            mark_device_value(device, "UsbinConn", 1);
            mark_device_value(device, "UsbinLow", id as u32);
            mark_device_value(device, "UsbinHigh", (id >> 32) as u32);
        }
        None => mark_device_value(device, "UsbinConn", 0),
    }
    println!("ln8000-kmdf: подключение {peripheral_id:#018X}");
    // SAFETY: пассивный уровень, устройство создано.
    let mut bus = match unsafe { SpbBus::open(device, peripheral_id, false) } {
        Ok(bus) => {
            let path = bus.path_string();
            let text = core::str::from_utf8(&path).unwrap_or("?");
            println!("ln8000-kmdf: шина {text}");
            bus
        }
        Err(err) => {
            println!("ln8000-kmdf: шина недоступна: {err}");
            let st = unsafe { state() };
            st.last_error = -1;
            mark_stage(device, STAGE_PREPARE_BUS, wdk_sys::STATUS_DEVICE_NOT_READY);
            return wdk_sys::STATUS_DEVICE_NOT_READY;
        }
    };

    // 3. Опознаём чип и настраиваем его.
    // Профиль берётся из реестра устройства: `HKR, Parameters, ...` из INF.
    // Значения проверяются по границам; неверное значение не меняет профиль.
    // SAFETY: пассивный уровень, устройство создано.
    let params = unsafe { read_parameters(device) };
    let config = params.config;
    let guard_limits = params.limits;

    // Проба чипа до опознания: сырой обмен с регистром DEVICE_ID (0x00).
    // Перебираем варианты оформления запроса к узлу шины и пишем результат
    // каждого в реестр: без отладчика это единственный способ понять, что
    // именно отклоняет узел, — и заодно найти работающий вариант.
    mark_device_value(device, "ConnLow", peripheral_id as u32);
    mark_device_value(device, "ConnHigh", (peripheral_id >> 32) as u32);
    mark_device_value(device, "RegAddr", u32::from(ln8000::regs::DEVICE_ID));
    // Подключение к периферии через цель родителя устройства: именно так это
    // делает эталонный клиент Qualcomm, и именно туда (а не в узел) уходит
    // запрос 0x32C004.
    let parent_attach = unsafe { parent_attach_probe(device) };
    mark_device_value(device, "ParAttachStatus", parent_attach as u32);
    mark_device_value(device, "ParAttachOk", if parent_attach >= 0 { 1 } else { 0 });
    // Последовательность в цель родителя: та же операция, но другой адресат.
    let parent_sequence = unsafe { parent_sequence_probe(device, ln8000::regs::DEVICE_ID) };
    mark_device_value(device, "ParSeqStatus", parent_sequence as u32);
    mark_device_value(device, "ParSeqOk", if parent_sequence >= 0 { 1 } else { 0 });
    let attach = bus.attach();
    mark_device_value(device, "AttachStatus", attach as u32);    for (index, name) in [(0_usize, "Att0"), (1, "Att1"), (2, "Att2"), (3, "Att3")] {
        mark_device_value(device, name, bus.attach_word(index));
    }
    let mut working = None;
    for (variant, names) in [
        (0_u8, ("Ok0", "St0")),
        (1_u8, ("Ok1", "St1")),
        (2_u8, ("Ok2", "St2")),
        (3_u8, ("Ok3", "St3")),
        (4_u8, ("Ok4", "St4")),
        (5_u8, ("Ok5", "St5")),
    ] {
        bus.set_variant(variant);
        let result = bus.transact(ln8000::regs::DEVICE_ID, None);
        let status = bus.last_status() as u32;
        match result {
            Ok(value) => {
                mark_device_value(device, names.0, 1);
                mark_device_value(device, "ProbeValue", u32::from(value));
                if value == ln8000::regs::DEVICE_ID_VALUE && working.is_none() {
                    working = Some(variant);
                }
            }
            Err(_) => mark_device_value(device, names.0, 0),
        }
        mark_device_value(device, names.1, status);
    }
    if let Some(variant) = working {
        bus.set_variant(variant);
        mark_device_value(device, "Variant", u32::from(variant));
    }

    // Проверяем, влияет ли удержание соединения: та же последовательность,
    // но уже на занятом соединении. Если занятие ломает обмен — увидим разницу.
    let lock = bus.lock_connection();
    mark_device_value(device, "LockStatus", lock as u32);
    mark_device_value(device, "LockOk", if lock >= 0 { 1 } else { 0 });
    bus.set_variant(0);
    let locked = bus.transact(ln8000::regs::DEVICE_ID, None);
    match locked {
        Ok(value) => {
            mark_device_value(device, "LockedOk", 1);
            mark_device_value(device, "LockedValue", u32::from(value));
        }
        Err(_) => mark_device_value(device, "LockedOk", 0),
    }
    mark_device_value(device, "LockedStatus", bus.last_status() as u32);

    // Читаем несколько регистров напрямую: если чип отвечает, увидим его
    // опознавательный код (ожидаем 0x42 в регистре 0x00).
    for (address, name) in [(0x00_u8, "Reg00"), (0x03, "Reg03"), (0x31, "Reg31")] {
        bus.set_variant(0);
        match bus.transact(address, None) {
            Ok(value) => mark_device_value(device, name, u32::from(value)),
            Err(_) => mark_device_value(device, name, 0xFFFF_FFFF),
        }
    }

    // Матрица возможностей узла: какие управляющие запросы SPB он принимает.
    // Это отделяет «узел не умеет последовательности» от «недоволен запросом».
    for (name, code) in [
        ("Io0", spb_abi::IOCTL_SPB_LOCK_CONTROLLER),
        ("Io1", spb_abi::IOCTL_SPB_UNLOCK_CONTROLLER),
        ("Io2", spb_abi::IOCTL_SPB_EXECUTE_SEQUENCE),
        ("Io3", spb_abi::IOCTL_SPB_LOCK_CONNECTION),
        ("Io4", spb_abi::IOCTL_SPB_UNLOCK_CONNECTION),
        ("Io5", spb_abi::IOCTL_SPB_FULL_DUPLEX),
        ("Io6", spb_abi::IOCTL_SPB_MULTI_SPI_TRANSFER),
    ] {
        let status = bus.probe_ioctl(code);
        mark_device_value(device, name, status as u32);
    }

    // Матрица на узле Resource Hub с маской доступа штатного драйвера:
    // если узел теперь принимает последовательности, это и есть рабочий путь.
    let mut hub_bus = unsafe { SpbBus::open(device, peripheral_id, true) }.ok();
    let mut hub_working: Option<u8> = None;
    if let Some(hub) = hub_bus.as_mut() {
        for (variant, names) in [
            (0_u8, ("Hub0", "HubS0")),
            (1_u8, ("Hub1", "HubS1")),
            (2_u8, ("Hub2", "HubS2")),
            (3_u8, ("Hub3", "HubS3")),
            (4_u8, ("Hub4", "HubS4")),
            (5_u8, ("Hub5", "HubS5")),
        ] {
            hub.set_variant(variant);
            let result = hub.transact(ln8000::regs::DEVICE_ID, None);
            let status = hub.last_status() as u32;
            match result {
                Ok(value) => {
                    mark_device_value(device, names.0, 1);
                    mark_device_value(device, "HubValue", u32::from(value));
                    if value == ln8000::regs::DEVICE_ID_VALUE && hub_working.is_none() {
                        hub_working = Some(variant);
                    }
                }
                Err(_) => mark_device_value(device, names.0, 0),
            }
            mark_device_value(device, names.1, status);
        }
    } else {
        mark_device_value(device, "Hub0", 0xFFFF_FFFF);
    }

    // Перебор идентификаторов узла ресурсов: какие подключения хаб отдаёт
    // вообще. Наш узел получил идентификатор 1 — проверяем соседние и адреса
    // периферий ADC из ACPI (0x131/0x135 = VADC/ADC_TM на SID2 PM8150B), чтобы
    // понять, есть ли среди них подключение к регистрам зарядника.
    // Результат каждого шага пишем в реестр: иначе с устройства этого не
    // увидеть, а без ответа дальше двигаться нечем.
    if hub_bus.is_some() {
        const CANDIDATES: [(u64, (&str, &str, &str)); 13] = [
            (2, ("Sc2Open", "Sc2St", "Sc2Val")),
            (3, ("Sc3Open", "Sc3St", "Sc3Val")),
            (4, ("Sc4Open", "Sc4St", "Sc4Val")),
            (5, ("Sc5Open", "Sc5St", "Sc5Val")),
            (6, ("Sc6Open", "Sc6St", "Sc6Val")),
            (8, ("Sc8Open", "Sc8St", "Sc8Val")),
            (16, ("Sc16Open", "Sc16St", "Sc16Val")),
            (56, ("Sc56Open", "Sc56St", "Sc56Val")),
            (0x13, ("Sc13Open", "Sc13St", "Sc13Val")),
            (0x31, ("Sc31Open", "Sc31St", "Sc31Val")),
            (0x131, ("Sc131Open", "Sc131St", "Sc131Val")),
            (0x135, ("Sc135Open", "Sc135St", "Sc135Val")),
            (0x213, ("Sc213Open", "Sc213St", "Sc213Val")),
        ];
        // 0x13/0x131/0x135 — peripheral ID из ACPI, не ConnectionId RH.
        for (candidate, names) in CANDIDATES {
            // SAFETY: пассивный уровень, устройство создано.
            match unsafe { SpbBus::open(device, candidate, true) } {
                Ok(mut probe) => {
                    mark_device_value(device, names.0, 1);
                    probe.set_variant(0);
                    let result = probe.transact(ln8000::regs::DEVICE_ID, None);
                    mark_device_value(device, names.1, probe.last_status() as u32);
                    match result {
                        Ok(value) => mark_device_value(device, names.2, u32::from(value)),
                        Err(_) => mark_device_value(device, names.2, 0xFFFF_FFFF),
                    }
                }
                Err(_) => {
                    mark_device_value(device, names.0, 0);
                    mark_device_value(device, names.1, 0xFFFF_FFFF);
                    mark_device_value(device, names.2, 0xFFFF_FFFF);
                }
            }
        }
    }

    // Рабочий маршрут к насосу — через узел ресурсов: только он несёт адрес
    // устройства (0x51). Родительский маршрут запросы принимает, но данных не
    // отдаёт, поэтому насос на нём не опознаётся. Если узел недоступен,
    // остаёмся на прежнем маршруте, чтобы не терять диагностику.
    // Узел ресурсов уже открыт выше; повторно его не открываем — второе
    // открытие не проходит. Берём готовую цель, иначе остаёмся на прежнем
    // маршруте, чтобы не терять диагностику.
    let pump_bus = match hub_bus {
        Some(hub) => {
            mark_device_value(device, "PumpBusOpen", 1);
            hub
        }
        None => {
            mark_device_value(device, "PumpBusOpen", 0);
            bus
        }
    };
    // Проба перебирала варианты оформления и остановилась на последнем.
    // Перед работой с насосом возвращаем рабочий вариант: иначе опознание
    // идёт заведомо неподдерживаемым запросом и падает.
    // Проба доступа к шине SPMI: единственная неисследованная дорога к регистрам
    // PMIC. Из пользовательского режима такие объекты не видны — проверяем из ядра.
    // Коды штатные: 0 — открылось, 0xC0000034 — объекта нет,
    // 0xC0000022 — доступ запрещён, 0xC0000001 — прочий отказ.
    for (mark, name) in [
        ("SpmiProbeSuperuser", "\\Device\\Spmi\\SUPERUSER"),
        ("SpmiProbeSpmi", "\\Device\\Spmi"),
        ("SpmiProbeUpperName", "\\Device\\SPMI"),
        ("SpmiProbeLowerName", "\\Device\\spmi"),
        ("SpmiProbeArb", "\\Device\\SpmiArb"),
    ] {
        // SAFETY: устройство создано, уровень пассивный.
        let status = unsafe { crate::spb::probe_named_target(device, name, 0x001F_01FF) };
        mark_device_value(device, mark, status as u32);
    }

    // Объект шины SPMI для периферии PMIC: его открывают драйверы qcpmic8150,
    // qcpmicext8150 и qcpmicgpio8150, значит он есть в системе. Прежняя проба
    // вернула отказ — проверяем, не в маске ли доступа дело.
    for (mark, access) in [
        ("SpmiSuAcc0", 0x0000_0000_u32),
        ("SpmiSuAcc1", 0x0000_0001_u32),
        ("SpmiSuAcc2", 0x0000_0002_u32),
        ("SpmiSuAcc3", 0x0000_0003_u32),
        ("SpmiSuAcc4", 0x8000_0000_u32),
        ("SpmiSuAcc5", 0x4000_0000_u32),
        ("SpmiSuAcc6", 0xC000_0000_u32),
        ("SpmiSuAcc7", 0x0001_0000_u32),
    ] {
        // SAFETY: устройство создано, уровень пассивный.
        let status = unsafe {
            crate::spb::probe_named_target(device, "\\Device\\Spmi\\SUPERUSER", access)
        };
        mark_device_value(device, mark, status as u32);
    }

    // Следующий рычаг QC: SUPERUSER уже отвергнут при ShareAccess=0. Штатные
    // qcpmic/qcADC держат свои объекты — пробуем с FILE_SHARE_* и публичные
    // символические имена, через которые идёт доступ к PMIC/ADC на этой платформе.
    // Если любое открытие вернёт 0 — это канал к SID2 / CMD_HVDCP_2 (0x1343).
    for (mark, name) in [
        ("PmicOpenQcompmic", "\\DosDevices\\Global\\QCOMPMIC"),
        ("PmicOpenBattmgr", "\\DosDevices\\Global\\QCOMBATTMGR"),
        ("PmicOpenPmictcc", "\\DosDevices\\Global\\QCOMPMICTCC"),
        ("PmicOpenPmicapps", "\\DosDevices\\Global\\QCOMPMICAPPS"),
        ("PmicOpenMiceic", "\\DosDevices\\Global\\QCOMPMICEIC"),
        ("PmicOpenBattmini", "\\DosDevices\\Global\\QCBatteryMiniclass"),
        ("AdcOpenQcomAdc", "\\??\\QCOM_ADC"),
        ("AdcOpenQcomAdc2", "\\??\\QCOM_ADC2"),
        ("AdcOpenQcomAdc3", "\\??\\QCOM_ADC3"),
        ("HubOpenBare", "\\Device\\RESOURCE_HUB"),
    ] {
        // SAFETY: устройство создано, уровень пассивный.
        let status = unsafe {
            crate::spb::probe_named_target_ex(
                device,
                name,
                0x001F_01FF,
                crate::spb::PROBE_SHARE_ALL,
            )
        };
        mark_device_value(device, mark, status as u32);
    }
    // SUPERUSER ещё раз — с совместным доступом (если qcpmic уже держит объект).
    // SAFETY: устройство создано, уровень пассивный.
    let su_share = unsafe {
        crate::spb::probe_named_target_ex(
            device,
            "\\Device\\Spmi\\SUPERUSER",
            0x001F_01FF,
            crate::spb::PROBE_SHARE_ALL,
        )
    };
    mark_device_value(device, "SpmiSuShare", su_share as u32);

    // Если слот SUPERUSER свободен (<3 держателей) — читаем APSD_STATUS (0x1307).
    // SAFETY: пассивный уровень; устройство создано.
    let apsd = unsafe { crate::spb::probe_superuser_apsd(device) };
    mark_device_value(device, "SuOpen", apsd.open_status as u32);
    mark_device_value(device, "SuGrant", apsd.grant_status as u32);
    mark_device_value(device, "SuApsdSt", apsd.read_status as u32);
    mark_device_value(device, "SuApsdVal", u32::from(apsd.value));

    // Если ACPI-оверлей добавил SPMI USBIN (SID2 / 0x13) на PEIC — пробуем
    // прочитать APSD_STATUS (0x1307). Успех (UsbinBeSt/UsbinLeSt = 0) = путь к
    // CMD_HVDCP_2. Без оверлея UsbinConn=0 и этот блок только пишет UsbinOpen=0xFFFFFFFF.
    // SAFETY: пассивный уровень; устройство создано.
    unsafe { probe_usbin_spmi(device, usbin_id) };
    // Gate for WS-C HVDCP: stock ACPI keeps `None`; negotiate prefers SUPERUSER.
    // SAFETY: prepare is serialized with IOCTL/timer by WDF.
    unsafe { state().usbin_id = usbin_id };

    let mut pump_bus = pump_bus;
    let pump_variant = hub_working.unwrap_or(0);
    pump_bus.set_variant(pump_variant);
    mark_device_value(device, "PumpVariant", u32::from(pump_variant));
    let mut pump = match Pump::open(pump_bus, config) {
        Ok(pump) => pump,
        Err(err) => {
            println!("ln8000-kmdf: LN8000 не опознан: {err}");
            let st = unsafe { state() };
            st.last_error = -2;
            mark_device_value(device, "PumpOpen", 0);
            mark_device_value(device, "PumpFailStatus", 0xC000_0001);
            mark_stage(device, STAGE_PREPARE_CHIP, 0xC000_0001u32 as i32);
            return wdk_sys::STATUS_SUCCESS;
        }
    };
    if let Err(err) = pump.configure() {
        println!("ln8000-kmdf: конфигурация не удалась: {err}");
        let st = unsafe { state() };
        st.last_error = -3;
        mark_stage(device, STAGE_PREPARE_CONFIG, 0);
        return wdk_sys::STATUS_DEVICE_NOT_READY;
    }

    // 4. Do not force a charge mode here: Vin is usually still ~5 V before
    //    HVDCP, and a failed 2:1 attempt can latch VIN_OV. Mode is chosen after
    //    HVDCP / by the telemetry timer via Vin-aware `set_charging`.
    let _ = pump.standby();

    let st = unsafe { state() };
    mark_device_value(device, "PumpOpen", 1);
    st.pump = Some(pump);
    st.telemetry_ms = params.telemetry_ms;
    st.limits = guard_limits;

    // 4b. Autostart HVDCP via SUPERUSER (Usbin RH secondary if overlay present).
    try_autostart_hvdcp(device);

    // 4c. Publish GUID_DEVICE_BATTERY via BattC (tray / Settings SoC).
    // Xiaomi qcbattminiclass never enables the interface; we estimate SoC from VBAT.
    // SAFETY: FDO exists; PASSIVE_LEVEL.
    let batt_st = unsafe { battery::initialize(device) };
    if batt_st >= 0 {
        // Re-borrow after HVDCP (it also touches `state()`).
        let st = unsafe { state() };
        let mut vbat = 0i32;
        let mut vbus = 0i32;
        let mut iin = 0i32;
        // First ADC right after HVDCP can return 0; retry — a zero sample must
        // not pin tray SoC at 0% (see battery::update_from_telemetry).
        for _ in 0..3 {
            if let Some(pump) = st.pump.as_mut() {
                vbat = pump.read_adc(AdcChannel::Vbat).unwrap_or_default();
                vbus = pump.read_adc(AdcChannel::Vin).unwrap_or_default();
                iin = pump.read_adc(AdcChannel::Iin).unwrap_or_default();
            }
            if vbat > 0 {
                break;
            }
        }
        unsafe {
            battery::update_from_telemetry(
                u32::try_from(vbat.max(0)).unwrap_or(0),
                u32::try_from(vbus.max(0)).unwrap_or(0),
                u32::try_from(iin.max(0)).unwrap_or(0),
                // Окно пика ещё не начато: единственный отсчёт и есть пик.
                st.max_iin_ua
                    .max(u32::try_from(iin.max(0)).unwrap_or(0)),
                monotonic_ms(),
            );
        }
        mark_device_value(device, "BattPct", battery::last_percent());
        mark_device_value(device, "BattVbat", u32::try_from(vbat.max(0)).unwrap_or(0) / 1000);
        mark_device_value(device, "BattPwr", battery::last_power_state());
    }

    // 4d. Держим систему в S0, пока устройство работает: экран гаснет по своему
    //     таймауту, но Connected Standby не наступает (см. `sysreq`). Без этого
    //     планшет умирал на idle через 4-12 минут (`Kernel-Power 41`,
    //     `BugcheckCode=0`, `ConnectedStandbyInProgress=true`).
    // SAFETY: prepare-hardware идёт на PASSIVE_LEVEL, FDO существует.
    let req_st = unsafe { sysreq::acquire(device) };
    mark_device_value(device, "SysReqSt", req_st as u32);
    mark_device_value(device, "SysReqOk", u32::from(req_st >= 0));

    // 5. Запускаем телеметрию с периодом из реестра (one-shot + re-arm).
    arm_telemetry_timer();
    // Железо готово: с этого такта счётчик можно опрашивать. Флаг ставится
    // после шины и до первого тика — `read_gauge_if_due` идёт **до** мьютекса,
    // то есть не защищён от гонки с `evt_release_hardware`.
    GAUGE_READY.store(true, Ordering::Release);

    mark_stage(device, STAGE_READY, 0);
    wdk_sys::STATUS_SUCCESS
}

/// Start / re-arm the telemetry one-shot from `st.telemetry_ms`.
///
/// WDF locks `Period` at `WdfTimerCreate`; a non-zero Period of 1000 ms used to
/// override registry `TelemetryMs=250` after the first tick.
fn arm_telemetry_timer() {
    let timer = unsafe { TIMER };
    if timer.is_null() {
        return;
    }
    let st = unsafe { state() };
    let period = i64::from(st.telemetry_ms.max(TELEMETRY_MS_MIN));
    unsafe {
        let _ = call_unsafe_wdf_function_binding!(WdfTimerStart, timer, -10_000_i64 * period);
    }
}

/// Короткий код состояния «есть ли вход и режим» для метки `EngageState`.
///
/// 0 — нет входа, 1 — standby, 2 — bypass, 3 — switching,
/// 4 — вход повышен, но 2:1 не проходит по физике (`Vin < 2*Vbat + 250 мВ`),
/// а обход 1:1 при таком входе запрещён.
fn engage_state(input_present: bool, vin_uv: i32, vbat_uv: u32, mode: OpMode) -> u32 {
    if !input_present {
        return ENGAGE_NO_INPUT;
    }
    if vin_uv >= SWITCHING_MIN_VIN_UV && charge_mode(vin_uv, vbat_uv).is_none() {
        return ENGAGE_NO_HEADROOM;
    }
    match mode {
        OpMode::Switching => ENGAGE_SWITCHING,
        OpMode::Bypass => ENGAGE_BYPASS,
        _ => ENGAGE_STANDBY,
    }
}

/// Пишет метки одной попытки включения заряда: `ChargeAttemptN`,
/// `LastEnableErr` (0 = успех) и `LastEnableMs` (монотонные миллисекунды).
fn mark_charge_attempt(
    device: WDFDEVICE,
    attempts: u32,
    now_ms: u64,
    result: &Result<OpMode, PumpError>,
) {
    let err = match result {
        Ok(_) => 0,
        Err(err) => pump_error_code(*err),
    };
    mark_device_value(device, "ChargeAttemptN", attempts);
    mark_device_value(device, "LastEnableErr", err as u32);
    mark_device_value(
        device,
        "LastEnableMs",
        u32::try_from(now_ms).unwrap_or(u32::MAX),
    );
}

/// Пишет метки фактического профиля защиты: `ProfLoops` и `ProfSel`.
///
/// `ProfLoops` — 1, если в итоговом профиле петли регулирования насоса включены
/// (и `V_FLOAT`, и `IIN`); иначе 0: тогда напряжение батареи ничем, кроме
/// аппаратного `VBAT_OV`, не ограничено, и это надо видеть в постмортеме.
/// `ProfSel` — применённое значение `ProtectionProfile` из реестра
/// (`PROFILE_NOT_SET` — параметра не было, остался профиль кода).
fn mark_profile(device: WDFDEVICE, config: &PumpConfig, selected: Option<u32>) {
    let loops = u32::from(!config.vbat_reg_disabled && !config.iin_reg_disabled);
    mark_device_value(device, "ProfLoops", loops);
    mark_device_value(device, "ProfSel", selected.unwrap_or(PROFILE_NOT_SET));
}

/// Итог [`recover_ln_shutdown`].
enum ShutdownRecovery {
    /// Чип не был в shutdown — восстанавливать нечего.
    NotInShutdown,
    /// Чип пересобран (`soft_reset` + `configure`) и сразу получил попытку
    /// `set_charging(true)`; внутри — её результат.
    Recovered(Result<OpMode, PumpError>),
}

/// Пауза POR после `soft_reset`.
///
/// Сброс запускает POR, и до [`regs::SOFT_RESET_DELAY_MS`] любой обмен по I²C
/// вешает чип (на этом уже ловили живое зависание). Пауза передаётся в
/// `set_charging`, чтобы 5-вольтовый путь восстановления (`soft_reset` →
/// `configure`) не трогал шину раньше времени.
fn por_delay() {
    let delay = u32::try_from(regs::SOFT_RESET_DELAY_MS)
        .unwrap_or(10)
        .saturating_mul(2);
    hvdcp::sleep_ms(delay);
}

/// Exit LN8000 hardware SHUTDOWN (`SYS_STS` bit0).
///
/// Soft-reset triggers POR: do not touch I²C until
/// [`regs::SOFT_RESET_DELAY_MS`] (live hang was verify-read during POR).
///
/// `configure()` leaves the chip in standby, so the charge attempt is made right
/// here: otherwise the caller would burn a whole `CHARGE_RETRY_MS` cooldown
/// before the next tick could set a mode.
fn recover_ln_shutdown(pump: &mut Pump<SpbBus>) -> ShutdownRecovery {
    let sys = match pump.read_register(regs::SYS_STS) {
        Ok(v) => v,
        Err(_) => return ShutdownRecovery::NotInShutdown,
    };
    if sys & regs::SYS_STS_SHUTDOWN == 0 {
        return ShutdownRecovery::NotInShutdown;
    }
    println!("ln8000-kmdf: SYS_STS=0x{sys:02X} shutdown — soft_reset + reconfigure");
    let _ = pump.soft_reset();
    por_delay();
    let _ = pump.configure();
    ShutdownRecovery::Recovered(pump.set_charging(true, &mut por_delay))
}

/// Останавливает телеметрию и переводит устройство в безопасное состояние.
///
/// # Safety
///
/// Вызывается WDF при удалении устройства.
unsafe extern "C" fn evt_release_hardware(
    _device: WDFDEVICE,
    _resources_translated: WDFCMRESLIST,
) -> NTSTATUS {
    // Счётчик больше не опрашиваем: `read_gauge_if_due` идёт до мьютекса и не
    // увидел бы, что шина вот-вот уйдёт из-под него. Флаг снимается до остановки
    // таймера, чтобы уже начатое чтение осталось единственным.
    GAUGE_READY.store(false, Ordering::Release);
    // Мьютекс берём до остановки таймера: `WdfTimerStop` с нулём не ждёт
    // текущий вызов, поэтому он мог бы работать с `STATE` одновременно с нами.
    let _state = lock_state();
    // SAFETY: `STATE` и шина защищены мьютексом состояния.
    let timer = unsafe { TIMER };
    if !timer.is_null() {
        unsafe {
            let _ = call_unsafe_wdf_function_binding!(WdfTimerStop, timer, 0);
        }
    }
    // SAFETY: detach BattC before tearing down the FDO path.
    unsafe { battery::unload() };
    // SAFETY: запрос питания наш и ещё не снят; снимаем вместе с устройством.
    unsafe { sysreq::release() };
    // SAFETY: см. инварианты `DriverState`.
    let st = unsafe { state() };
    if let Some(pump) = st.pump.as_mut() {
        if let Err(err) = pump.standby() {
            println!("ln8000-kmdf: не удалось уйти в standby: {err}");
        }
        pump.close();
    }
    st.pump = None;
    st.hvdcp_retry_pending = false;
    st.hvdcp_edge_armed = false;
    st.last_input_present = false;
    // SAFETY: single-device lifetime ends with release.
    unsafe {
        DEVICE = core::ptr::null_mut();
    }
    wdk_sys::STATUS_SUCCESS
}

/// Периодический сбор телеметрии, журналирование и защита.
///
/// # Safety
///
/// Вызывается WDF из таймера на пассивном уровне.
unsafe extern "C" fn evt_telemetry_timer(_timer: WDFTIMER) {
    // Топливный счётчик читается до мьютекса: одна транзакция SUPERUSER идёт
    // около двух секунд, а под мьютексом стоят и управление зарядом, и
    // `IOCTL_STATUS`. Пока опрос шёл под мьютексом каждые 250 мс, сторонний
    // читатель SUPERUSER не мог открыть шину в 79 попытках из 100.
    // SAFETY: пассивный уровень, состояние не трогаем.
    let gauge = unsafe { read_gauge_if_due() };
    // Таймер идёт своим контекстом, очередь WDF его не сериализует: шину и
    // `STATE` защищает мьютекс состояния.
    let _state = lock_state();
    // Pump borrow must end before HVDCP reopen (retry / re-plug).
    let (want_replug, want_retry) = {
    // SAFETY: доступ сериализован WDF (автоматическая сериализация таймера).
    let st = unsafe { state() };
    let Some(pump) = st.pump.as_mut() else {
        // Hardware released — do not re-arm (ReleaseHardware already stopped us).
        return;
    };

    let status = match pump.status() {
        Ok(status) => status,
        Err(err) => {
            st.last_error = -10;
            println!("ln8000-kmdf: статус недоступен: {err}");
            arm_telemetry_timer();
            return;
        }
    };

    // Отказ чтения канала даёт ноль, который неотличим от настоящего нуля,
    // поэтому достоверность едет в отсчёте отдельными признаками: по мусору
    // защита не имеет права ни резать ток, ни возвращать его к профилю.
    // Признак — только результат шинного чтения; правдоподобность значения
    // проверяет сама защита (`guard::die_temp_usable`: нулевой сырой код АЦП
    // декодируется в +160,0 °C и отсекается по `DIE_TEMP_MAX_PLAUSIBLE_DC`).
    let vbat_read = pump.read_adc(AdcChannel::Vbat);
    let vbus_read = pump.read_adc(AdcChannel::Vin);
    let iin_read = pump.read_adc(AdcChannel::Iin);
    let temp_read = pump.read_adc(AdcChannel::DieTemp);
    let vbat = vbat_read.unwrap_or_default();
    let vbus = vbus_read.unwrap_or_default();
    let iin = iin_read.unwrap_or_default();
    let temp = temp_read.unwrap_or_default();

    let sample = TelemetrySample {
        ts_ms: monotonic_ms(),
        vbat_uv: u32::try_from(vbat.max(0)).unwrap_or(0),
        vbus_uv: u32::try_from(vbus.max(0)).unwrap_or(0),
        iin_ua: u32::try_from(iin.max(0)).unwrap_or(0),
        die_temp_dc: temp,
        op_mode: status.op_mode,
        input_present: !status.has_critical_fault() && vbus > 0,
        vbat_valid: vbat_read.is_ok(),
        die_temp_valid: temp_read.is_ok(),
    };
    st.telemetry.push(sample);

    // SAFETY: BattC status notify is DISPATCH-safe; we run at PASSIVE.
    unsafe {
        battery::update_from_telemetry(
            sample.vbat_uv,
            sample.vbus_uv,
            sample.iin_ua,
            st.max_iin_ua,
            sample.ts_ms,
        );
    }
    let device = unsafe { DEVICE };
    if !device.is_null() {
        mark_device_value(device, "BattPwr", battery::last_power_state());
        // Живые Vin/Iin/режим для операторских скриптов (читают `SuVinUv`,
        // `SuIin`, `SuMode`); раньше эти имена никто не писал.
        mark_device_value(device, "SuVinUv", sample.vbus_uv);
        mark_device_value(device, "SuIin", sample.iin_ua);
        mark_device_value(device, "SuMode", u32::from(status.op_mode.code()));
        // Битовая маска достоверности каналов (0 — все прочитаны): в дампе
        // отказ чтения больше не выглядит как настоящий ноль.
        // бит 0 — VBAT, бит 1 — DieTemp, бит 2 — Iin, бит 3 — Vin.
        mark_device_value(
            device,
            "AdcValid",
            u32::from(!sample.vbat_valid)
                | (u32::from(!sample.die_temp_valid) << 1)
                | (u32::from(iin_read.is_err()) << 2)
                | (u32::from(vbus_read.is_err()) << 3),
        );
        // Пик тока за окно наблюдения: пишем раз в IIN_WINDOW_MS и начинаем
        // новое окно, чтобы постфактум было видно, брал ли драйвер ток вообще.
        if sample.iin_ua > st.max_iin_ua {
            st.max_iin_ua = sample.iin_ua;
        }
        if st.iin_window_start_ms == 0
            || sample.ts_ms.saturating_sub(st.iin_window_start_ms) >= IIN_WINDOW_MS
        {
            mark_device_value(device, "MaxIinUa", st.max_iin_ua);
            st.max_iin_ua = sample.iin_ua;
            st.iin_window_start_ms = sample.ts_ms;
        }
    }

    // Топливный счётчик PM8150B: единственный достоверный источник процента.
    // Линейная оценка по VBAT на этой банке врала в обе стороны — на
    // подключённом блоке давала 100 %, а там, где счётчик говорит 6 %, — 25 %.
    // Само чтение сделано выше, до мьютекса (`read_gauge_if_due`); здесь только
    // публикация. Отказ ничего не портит: остаётся прежнее значение (а если
    // счётчик не отвечал ни разу — оценка по банке, `SocSrc=2`). Счётчик отказов
    // живёт в модуле батареи: здесь `pump` держит изменяемую ссылку на
    // состояние, и второй раз брать её нельзя.
    let device = unsafe { DEVICE };
    if !device.is_null() {
        match gauge {
            GaugePoll::Raw(raw) => {
                unsafe { battery::set_gauge_raw(raw) };
                mark_device_value(device, "SocRaw", u32::from(raw));
            }
            GaugePoll::Failed => {
                let fails = unsafe { battery::note_gauge_failure() };
                mark_device_value(device, "SocFail", fails);
            }
            GaugePoll::Skipped => {}
        }
        mark_device_value(device, "SocSrc", battery::last_soc_source());
        mark_device_value(device, "BattPct", battery::last_percent());
        // Живое напряжение банки с чипа, а не отсчёт, снятый один раз при
        // запуске устройства: прежняя запись жила только в prepare-hardware и в
        // IOCTL, поэтому марка замирала на значении первого такта (живой замер
        // 19.09 12:0x: `BattVbat` 4 010 мВ при живых 4 230 мВ) и портила разбор
        // полосы переноса, который считается от `vbat`. Во время 2:1 отсчёт
        // идёт по середине шины преобразователя (≈ Vin/2), а не по банке —
        // заряд берётся из счётчика PM8150B (`BattPct`).
        mark_device_value(device, "BattVbat", sample.vbat_uv / 1000);
        mark_device_value(device, "BattPwr", battery::last_power_state());
        // Возраст последнего удачного чтения: по нему видно, что счётчик не
        // «залип», а опрашивается редко — раз в `GAUGE_POLL_MS`.
        let gauge_last = GAUGE_LAST_MS.load(Ordering::Acquire);
        let age_ms = u32::try_from(sample.ts_ms.saturating_sub(gauge_last)).unwrap_or(u32::MAX);
        mark_device_value(device, "SocAgeMs", age_ms);
        mark_device_value(device, "GaugePollMs", GAUGE_POLL_MS);
        // Сырые байты отказов публикуются как есть: у чипа есть групповой
        // признак «напряженческих» отказов (`FAULT1` биты 6:0), который вендор
        // тестирует целиком, а именованных битов в нём только пять. Живой
        // `FAULT1=0x21` — два безымянных бита этой группы: `has_critical_fault`
        // о них молчит, а вендорский `volt_qual` говорит «вход негоден». Без
        // этих марок разница между «нет отказов» и «вход негоден» не видна.
        mark_device_value(device, "Fault1Sts", u32::from(status.fault1_sts));
        mark_device_value(device, "Fault2Sts", u32::from(status.fault2_sts));
        mark_device_value(device, "SysSts", u32::from(status.sys_sts));
        mark_device_value(device, "SafetySts", u32::from(status.safety_sts));
        // Вторая ступень вендора считается только при разрешённом заряде;
        // «разрешён» — это принятый чипом рабочий режим, а не запрошенный.
        let charging = matches!(status.op_mode, OpMode::Switching | OpMode::Bypass);
        mark_device_value(device, "VoltQual", u32::from(status.volt_qual(charging)));
        // Какая стадия 5-вольтового резерва сработала: `1` — чистая запись
        // (то, что работало 17.09), `2` — POR, `3` — маска отказов. По этой
        // марке видно, лечится ли блокировка режима снятием защёлки или вход
        // действительно негоден.
        mark_device_value(device, "BypassStage", u32::from(pump.bypass_stage()));
        // `1` — POR-бюджет этого входа уже израсходован: драйвер упёрся в отказ
        // и ждёт смены блока. По марке видно, что повторные тики не дёргают чип.
        mark_device_value(device, "BypassPorSpent", u32::from(pump.por_spent()));
    }

    // Эпизод перегрева закончился — счётчик отказов 1:1 обнуляется: иначе
    // второй эпизод ≥ 48 °C остановил бы заряд сразу, без ступени снижения тока.
    if bypass_strikes_expired(&sample, &st.limits) {
        st.bypass_denied_strikes = 0;
    }

    // Защита по температуре и току: решение принимается по последнему отсчёту.
    // Действие разрешается с учётом Vin: 1:1 допустим только в окне обхода.
    // Третий аргумент — уставка, которая **фактически стоит в чипе**: её читает
    // `applied_iin_ua` (тапер у верха заряда пишет 1,2 А мимо профиля, и по
    // профилю защита «снижала» бы ток, поднимая его). Регистр не прочитан —
    // уставка неизвестна, решений о токе в этом такте не принимаем.
    // Четвёртый — намеренная уставка тапера (`Pump::taper_setpoint_ua`): возврат
    // к профилю обязан остановиться на ней и никогда не опускать лимит. Без неё
    // в окне пересечения полос тапера и возврата защита каждые 250 мс отменяла бы
    // намеренное снижение тока до 1,2 А.
    let applied_iin_ua = pump.applied_iin_ua();
    let ov_latched = status.fault1_sts & ln8000::regs::FAULT1_VBAT_OV != 0;
    let deliberate_iin_ua = pump.taper_setpoint_ua(sample.vbat_uv, sample.vbus_uv, ov_latched);
    let action = evaluate(&sample, &st.limits, applied_iin_ua, deliberate_iin_ua);
    if !device.is_null() {
        // Кто снял режим: без этих марок падение 2:1 → standby неотличимо от
        // отказа чипа. `GuardAct`: 0 нет, 1 снижение тока, 2 возврат, 3 переход
        // в 1:1, 4 стоп. `DieTempDc` — температура кристалла, по которой принято
        // решение (`0xFFFFFFFF` = канал недостоверен).
        mark_device_value(
            device,
            "GuardAct",
            match action {
                GuardAction::None => 0,
                GuardAction::ReduceCurrent { .. } => 1,
                GuardAction::RestoreCurrent { .. } => 2,
                GuardAction::FallbackToBypass { .. } => 3,
                GuardAction::Stop { .. } => 4,
                // `GuardAction` помечен `#[non_exhaustive]`: новая ступень
                // защиты должна быть видна в марке, а не молча давать ноль.
                _ => 5,
            },
        );
        mark_device_value(
            device,
            "DieTempDc",
            if sample.die_temp_valid {
                u32::try_from(sample.die_temp_dc).unwrap_or(u32::MAX)
            } else {
                u32::MAX
            },
        );
    }
    if action.is_change() {
        apply_guard(
            pump,
            action,
            vbus,
            sample.vbat_uv,
            &st.limits,
            &mut st.bypass_denied_strikes,
        );
        st.actions = st.actions.saturating_add(1);
        println!(
            "ln8000-kmdf: защита {} ({}) при {temp} dC и {iin} uA",
            action.label(),
            match action {
                GuardAction::ReduceCurrent { to_ua, .. }
                | GuardAction::RestoreCurrent { to_ua, .. } => to_ua,
                _ => 0,
            }
        );
    }

    // Автозапуск / смена режима по Vin (cp_qc30): 2:1 при >=8 V, bypass при ~5 V.
    // Если уже в bypass и Vin поднялся QC — обязательно апгрейд до 2:1 (не ждать
    // 30 с). Soft-reset при elevated Vin не делаем: он роняет QC latch.
    // Watchdog latch (FAULT1 bit7) forces standby — clear and retry without soft_reset.
    // Near-float VBAT_OV (bit6) likewise blocks mode until soft-cleared by set_charging.
    if status.fault1_sts
        & (ln8000::regs::FAULT1_WATCHDOG | ln8000::regs::FAULT1_VBAT_OV)
        != 0
    {
        let _ = pump.clear_latched_faults();
        if status.fault1_sts & ln8000::regs::FAULT1_WATCHDOG != 0 {
            let _ = pump.service_watchdog();
            println!("ln8000-kmdf: сброс защёлки watchdog");
        }
        if status.fault1_sts & ln8000::regs::FAULT1_VBAT_OV != 0 {
            println!("ln8000-kmdf: FAULT1_VBAT_OV latched — retry via set_charging");
        }
    }
    let vbus_uv = u32::try_from(vbus.max(0)).unwrap_or(0);
    let vbat_uv = sample.vbat_uv;
    // Выбор режима — по Vin И Vbat (cp_qc30: 2:1 только при Vin >= 2*Vbat + 250 мВ).
    // Повышенный Vin без запаса даёт None: обход 1:1 там запрещён, а standby-цикл
    // не крутим — состояние видно в `EngageState=4`.
    // Работающий перенос не понижаем по просадке: под нагрузкой шина садится
    // на 100–250 мВ, и живой замер даёт 8,176 В при банке 3,985 В, где
    // `charge_mode` требует `2*Vbat + 250 мВ` = 8,22 В — то есть обход. Понижение
    // по мгновенному отсчёту рвало работающий 2:1 каждые ~5 с (живой замер
    // 19.09 11:28 на MDY-11-EP: mode 1→3→1 при 0,42 А, `ChargeAttemptN` +1 на
    // каждый разрыв). Липкое решение держится на абсолютном поле 2:1 (8,0 В):
    // просадка — это следствие нагрузки, а не потеря способности переносить.
    let pump_alive = status.op_mode == OpMode::Switching
        && sample.iin_ua > hvdcp::IIN_DEAD_FLOOR_UA;
    let desired = if pump_alive && vbus_uv >= u32::try_from(SWITCHING_MIN_VIN_UV).unwrap_or(0) {
        Some(OpMode::Switching)
    } else {
        charge_mode(vbus, vbat_uv)
    };
    let mode_ok = matches!(
        (desired, status.op_mode),
        (Some(OpMode::Switching), OpMode::Switching)
            | (Some(OpMode::Bypass), OpMode::Bypass)
            | (None, OpMode::Standby | OpMode::Unknown)
    );
    if !device.is_null() {
        mark_device_value(
            device,
            "EngageState",
            engage_state(sample.input_present, vbus, vbat_uv, status.op_mode),
        );
    }
    // Полоса переноса 2:1 едет за банкой: пока банка набирает заряд, её верх
    // уходит вверх, и шина, выставленная при согласовании, остаётся НИЖЕ полосы
    // — либо, наоборот, уходит выше неё, если согласование целилось в
    // фиксированные 9,5 В. И то и другое кончается одинаково: режим 3
    // сохраняется (`SYS_STS=0x04`), а перенос падает до 39 мА аддитивного пола.
    //
    // Вход считается повышенным уже с `SWITCHING_MIN_VIN_UV`: обход 1:1 там
    // запрещён, поэтому «повышен, но не в 2:1» — состояние, которое надо
    // исправлять, а не оставлять. Прежняя форма требовала
    // `desired == Switching`, а шина НИЖЕ пола полосы даёт `desired = None`
    // (`charge_mode` требует `2*Vbat + 250 мВ`) — и поднимать её было некому:
    // живой замер 18.09 22:38, Vin 8,88 В при полосе [9,00; 9,20] В, mode 1,
    // 39 мА, счётчик попыток стоял, потому что `(None, standby)` считается
    // согласованным состоянием.
    let elevated = vbus_uv >= u32::try_from(SWITCHING_MIN_VIN_UV).unwrap_or(0);
    // Шина выше 5 В — уже не «пятивольтовый» вход: обход 1:1 там греет разницу
    // напряжений на чипе, а полоса переноса 2:1 всего на шаг QC3 выше. Прежнее
    // условие (`elevated || dead_band`) требовало `desired = None`, но при 6–8 В
    // `charge_mode` возвращает обход, и в полосу переноса шину не вёл никто:
    // до ворот 2:1 не хватало одного шага.
    //
    // Живой замер 19.09 11:19 на MDY-11-EP: шина 7,888 В, `desired` = обход,
    // 1:1 даёт 2,06 А при 6,75 В (банка 3,915 В — разница горит на чипе), и
    // режим флапает 1↔2 каждые ~15 с: обход вне своего окна снимается, шина
    // возвращается к 7,888 В, обход включается снова. 39 мА и `ChargeAttemptN`
    // стояли — коррекция не запускалась ни разу.
    let above_five = vbus_uv >= u32::try_from(hvdcp::FIVE_V_STAY_MAX_UV).unwrap_or(0);
    let dead_band = desired.is_none() && above_five;
    let bypass_below_gate = matches!(desired, Some(OpMode::Bypass)) && above_five;
    // Уход из обхода ради подъёма шины: в этом такте решение о режиме
    // пересчитывать нельзя, оно посчитано по старому Vin (см. ниже).
    let mut left_bypass_for_walk = false;
    if vbat_uv > 0 && (elevated || dead_band || bypass_below_gate) {
        // «Мёртвый» ток — это пол АЦП (39,1 мА), а не «мало»: у верха заряда
        // банка берёт 0,1–0,5 А, и по мгновенному отсчёту такие такты
        // выглядели мёртвыми. Поэтому требуется, чтобы и мгновенный отсчёт, и
        // пик за окно `IIN_WINDOW_MS` лежали на полу — тогда за коррекцией
        // действительно нет переноса.
        let dead = status.op_mode == OpMode::Switching
            && sample.iin_ua <= hvdcp::IIN_DEAD_FLOOR_UA
            && st.max_iin_ua <= hvdcp::IIN_DEAD_FLOOR_UA;
        let outside = !ln8000::encoding::vin_in_switching_window(vbus, vbat_uv);
        if outside || dead {
            let now = monotonic_ms();
            let target = hvdcp::target_vbus_uv(vbat_uv);
            let err = vbus_uv.abs_diff(target);
            // Прогресс сбрасывает выдержку; застой (блок не держит QC3-шаг или
            // это PD-адаптер, для которого импульсы — пустая трата SPMI)
            // переводит петлю на редкий повтор.
            if err.saturating_add(WINDOW_PROGRESS_UV) < st.window_best_err_uv {
                st.window_best_err_uv = err;
                st.window_stall_n = 0;
            }
            let cooldown = if st.window_stall_n < WINDOW_STALL_MAX {
                hvdcp::WINDOW_NUDGE_MS
            } else {
                WINDOW_STALL_MS
            };
            // Первый такт в повышенном обходе не ждёт выдержки: 1:1 на 6–8 В
            // греет разницу напряжений на чипе, и каждый лишний такт там — это
            // 10 с работы впустую (см. `bypass_below_gate`). Дальше — обычный
            // темп: `window_stall_n` растёт на каждом срабатывании, так что
            // шторм импульсов невозможен, даже если вывести из обхода не удалось.
            let urgent = matches!(desired, Some(OpMode::Bypass))
                && status.op_mode == OpMode::Bypass
                && st.window_stall_n == 0;
            if urgent || now.saturating_sub(st.last_window_nudge_ms) >= cooldown {
                st.last_window_nudge_ms = now;
                st.window_stall_n = st.window_stall_n.saturating_add(1);
                // В 1:1 импульс INC поднимает вход прямо на банку: FET обхода
                // замкнут, и шаг QC3 уходит не в шину, а в разницу напряжений
                // на чипе. Поэтому сначала уводим чип в standby — тогда импульс
                // меняет именно напряжение входа, а решение о режиме примет
                // следующий такт по новому Vin.
                if status.op_mode == OpMode::Bypass {
                    let left = pump.standby().is_ok();
                    left_bypass_for_walk = left;
                    if !device.is_null() {
                        mark_device_value(device, "WalkStandby", u32::from(left));
                    }
                }
                // SAFETY: PASSIVE_LEVEL; сессия открыта, доступ сериализован
                // таймером (см. комментарий у `state()`).
                let sent = unsafe {
                    hvdcp::nudge_vin_into_window(
                        device,
                        st.usbin_id,
                        &mut st.hvdcp,
                        vbus,
                        vbat_uv,
                        dead,
                    )
                };
                if !device.is_null() {
                    mark_device_value(device, "WindowOut", u32::from(outside));
                    mark_device_value(device, "WindowDead", u32::from(dead));
                }
                if sent > 0 {
                    // Шина только что сдвинулась — режим перерешаем сразу, не
                    // выжидая `CHARGE_RETRY_MS`: одиночный шаг QC3 меньше
                    // `CHARGE_RETRY_DELTA_UV`, поэтому «изменение входа» его не
                    // разбудит.
                    st.last_charge_attempt_ms = 0;
                }
            }
        } else if st.window_best_err_uv != u32::MAX || st.window_stall_n != 0 {
            // Вернулись в полосу — история застоя сбрасывается.
            st.window_best_err_uv = u32::MAX;
            st.window_stall_n = 0;
        }
    } else if st.window_best_err_uv != u32::MAX || st.window_stall_n != 0 {
        st.window_best_err_uv = u32::MAX;
        st.window_stall_n = 0;
    }
    // Обход снят ради подъёма шины: `desired` посчитан по старому Vin, поэтому
    // в этом такте режим не перерешаем — следующий такт (250 мс) прочтёт шину
    // заново и выберет 2:1, если импульс довёл её до полосы переноса.
    if mode_ok && !left_bypass_for_walk {
        st.last_charge_attempt_ms = monotonic_ms();
        st.last_attempt_vbus_uv = vbus_uv;
    } else if desired.is_some() && !action.is_change() && !left_bypass_for_walk {
        let now = monotonic_ms();
        let cooled = now.saturating_sub(st.last_charge_attempt_ms) >= CHARGE_RETRY_MS;
        let changed = st.last_attempt_vbus_uv.abs_diff(vbus_uv) >= CHARGE_RETRY_DELTA_UV;
        // Апгрейд bypass → 2:1 делаем сразу (QC поднял Vin). standby/unknown —
        // это отказ, и повтор идёт не чаще кулдауна, а не каждый такт.
        let upgrade = matches!(
            (desired, status.op_mode),
            (Some(OpMode::Switching), OpMode::Bypass)
        );
        if cooled || changed || upgrade {
            st.last_charge_attempt_ms = now;
            st.last_attempt_vbus_uv = vbus_uv;
            // SAFETY: сессия открыта, доступ сериализован таймером.
            // Soft-reset + configure уже сами пробуют заряд в том же такте.
            let recovery = recover_ln_shutdown(pump);
            // Второй POR в этом же такте не делаем: диагностика должна совпадать
            // с фактическими действиями (см. ветку отказов ниже).
            let recovered_this_tick = matches!(recovery, ShutdownRecovery::Recovered(_));
            let result = match recovery {
                ShutdownRecovery::Recovered(outcome) => {
                    st.last_charge_attempt_ms = monotonic_ms();
                    outcome
                }
                ShutdownRecovery::NotInShutdown => pump.set_charging(true, &mut por_delay),
            };
            st.charge_attempts = st.charge_attempts.saturating_add(1);
            st.last_enable_ms = monotonic_ms();
            if !device.is_null() {
                mark_charge_attempt(device, st.charge_attempts, st.last_enable_ms, &result);
            }
            match result {
                Ok(mode) => {
                    st.auto_starts = st.auto_starts.saturating_add(1);
                    st.failed_attempts = 0;
                    st.last_error = 0;
                    println!("ln8000-kmdf: заряд включён автоматически, режим {}", mode.code());
                }
                Err(err) => {
                    st.last_error = pump_error_code(err);
                    st.failed_attempts = st.failed_attempts.saturating_add(1);
                    if st.failed_attempts >= CHARGE_FAILS_BEFORE_RESET {
                        st.failed_attempts = 0;
                        if recovered_this_tick {
                            println!(
                                "ln8000-kmdf: повтор после восстановления не удался — второй soft_reset в этом такте пропущен"
                            );
                        } else {
                            match recover_ln_shutdown(pump) {
                                ShutdownRecovery::Recovered(outcome) => {
                                    st.last_charge_attempt_ms = monotonic_ms();
                                    st.charge_attempts = st.charge_attempts.saturating_add(1);
                                    st.last_enable_ms = monotonic_ms();
                                    if !device.is_null() {
                                        mark_charge_attempt(
                                            device,
                                            st.charge_attempts,
                                            st.last_enable_ms,
                                            &outcome,
                                        );
                                    }
                                    match outcome {
                                        Ok(mode) => println!(
                                            "ln8000-kmdf: soft_reset после SHUTDOWN — заряд перезапущен, режим {}",
                                            mode.code()
                                        ),
                                        Err(err) => println!(
                                            "ln8000-kmdf: soft_reset после SHUTDOWN — повтор не удался: {err}"
                                        ),
                                    }
                                }
                                ShutdownRecovery::NotInShutdown if vbus < SWITCHING_MIN_VIN_UV => {
                                    let _ = pump.soft_reset();
                                    por_delay();
                                    let _ = pump.configure();
                                    println!("ln8000-kmdf: soft_reset + reconfigure (5 V path)");
                                    // Правка 4: заряд включаем в том же такте, а не
                                    // через CHARGE_RETRY_MS.
                                    let retry = pump.set_charging(true, &mut por_delay);
                                    st.charge_attempts = st.charge_attempts.saturating_add(1);
                                    st.last_enable_ms = monotonic_ms();
                                    if !device.is_null() {
                                        mark_charge_attempt(
                                            device,
                                            st.charge_attempts,
                                            st.last_enable_ms,
                                            &retry,
                                        );
                                    }
                                    match retry {
                                        Ok(mode) => println!(
                                            "ln8000-kmdf: 5 V recovery — заряд перезапущен, режим {}",
                                            mode.code()
                                        ),
                                        Err(err) => println!(
                                            "ln8000-kmdf: 5 V recovery — повтор не удался: {err}"
                                        ),
                                    }
                                }
                                ShutdownRecovery::NotInShutdown => {
                                    let _ = pump.clear_latched_faults();
                                    println!(
                                        "ln8000-kmdf: сброс защёлки (elevated Vin, без soft_reset)"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    } else if desired.is_none()
        && matches!(status.op_mode, OpMode::Switching | OpMode::Bypass)
        && !action.is_change()
    {
        let _ = pump.set_charging(false, &mut por_delay);
    }

    // Сторож чипа: если включён в профиле, его надо обслуживать чаще периода,
    // иначе чип сам прекратит заряд. Когда сторож выключен, вызов ничего не делает.
    if let Err(err) = pump.service_watchdog() {
        println!("ln8000-kmdf: не удалось обслужить сторожевой таймер: {err}");
    }

    // Cold-plug / re-plug: decide while pump is borrowed, act after drop.
    let input_now = hvdcp::input_present_from_vin(vbus);
    let want_replug = st.hvdcp_edge_armed
        && hvdcp::should_renegotiate_on_input_edge(
            st.hvdcp.phase.code(),
            st.last_input_present,
            input_now,
        );
    let now = monotonic_ms();
    let want_retry = hvdcp::superuser_retry_due(
        st.hvdcp_retry_pending,
        st.hvdcp_retry_attempts,
        now,
        st.hvdcp_retry_next_ms,
    );
    st.last_input_present = input_now;
    (want_replug, want_retry)
    };

    // SAFETY: DEVICE set in device_add; null after release_hardware.
    let device = unsafe { DEVICE };
    if device.is_null() {
        // Released — timer already stopped; do not re-arm.
        return;
    }
    if want_replug {
        mark_device_value(device, "HvdcpReplug", 1);
        // Новый вход — старая защёлка `FORCE_9V` силы не имеет: блок мог быть
        // заменён на QC3, для которого импульсы и есть способ подъёма, а
        // `pulse_cmd_bit` под защёлкой их пропускает. Держать её через
        // переподключение — значит запретить импульсный путь до перезапуска
        // драйвера. Внутри одной сессии защёлку по-прежнему снимает только
        // `safe_force_5v`: иначе первый же импульс уронил бы уровень QC2.
        unsafe { state() }.hvdcp.force9v_latched = false;
        println!("ln8000-kmdf: HVDCP re-plug edge — renegotiate");
        let code = run_hvdcp_and_land(device);
        schedule_or_clear_superuser_retry(device, code);
        arm_hvdcp_input_edge(device);
    } else if want_retry {
        // SAFETY: timer serialized with prepare/IOCTL.
        let st = unsafe { state() };
        st.hvdcp_retry_attempts = st.hvdcp_retry_attempts.saturating_add(1);
        st.hvdcp_retry_next_ms =
            monotonic_ms().saturating_add(hvdcp::HVDCP_SUPERUSER_RETRY_MS);
        mark_device_value(device, "HvdcpRetryN", st.hvdcp_retry_attempts);
        println!(
            "ln8000-kmdf: HVDCP SUPERUSER retry #{}",
            st.hvdcp_retry_attempts
        );
        let code = run_hvdcp_and_land(device);
        schedule_or_clear_superuser_retry(device, code);
        if code == 0 {
            arm_hvdcp_input_edge(device);
        }
    }

    arm_telemetry_timer();
}

/// Применяет решение защиты к устройству.
///
/// Уход в 1:1 разрешён только при `Vin` в окне обхода: `EN_1TO1` подаёт вход
/// напрямую на батарею, поэтому на повышенном Vin (QC/PD 9–12 В) защита вместо
/// обхода снижает ток, а при упорной температуре — останавливает заряд.
/// `denied_strikes` — счётчик тактов, когда 1:1 был нужен и запрещён.
fn apply_guard(
    pump: &mut Pump<SpbBus>,
    action: GuardAction,
    vin_uv: i32,
    vbat_uv: u32,
    limits: &GuardLimits,
    denied_strikes: &mut u32,
) {
    match action {
        GuardAction::None => {
            *denied_strikes = 0;
        }
        GuardAction::ReduceCurrent { to_ua, .. } => {
            *denied_strikes = 0;
            if let Err(err) = pump.set_iin_limit(to_ua) {
                println!("ln8000-kmdf: не удалось снизить ток: {err}");
            }
        }
        GuardAction::RestoreCurrent { to_ua, reason } => {
            *denied_strikes = 0;
            println!(
                "ln8000-kmdf: {reason} — возвращаю профильный лимит тока до {to_ua} мкА"
            );
            if let Err(err) = pump.set_iin_limit(to_ua) {
                println!("ln8000-kmdf: не удалось вернуть лимит тока: {err}");
            }
        }
        GuardAction::FallbackToBypass { reason } => match resolve_bypass(
            vin_uv,
            vbat_uv,
            *denied_strikes,
            limits,
        ) {
            BypassResolution::Allowed => {
                *denied_strikes = 0;
                if let Err(err) = pump.enable_bypass() {
                    println!("ln8000-kmdf: не удалось уйти в bypass: {err}");
                }
            }
            BypassResolution::ReduceCurrent { to_ua, reason } => {
                *denied_strikes = denied_strikes.saturating_add(1);
                println!(
                    "ln8000-kmdf: {reason} при Vin {vin_uv} мкВ — 1:1 запрещён, снижаю ток до {to_ua} мкА"
                );
                if let Err(err) = pump.set_iin_limit(to_ua) {
                    println!("ln8000-kmdf: не удалось снизить ток: {err}");
                }
            }
            BypassResolution::Stop { reason } => {
                println!(
                    "ln8000-kmdf: {reason} при Vin {vin_uv} мкВ — 1:1 запрещён, останавливаю заряд"
                );
                if let Err(err) = pump.standby() {
                    println!("ln8000-kmdf: не удалось остановить заряд: {err}");
                }
            }
            // Перечисление помечено `non_exhaustive`: новых разрешений не ждём,
            // но на всякий случай не включаем 1:1 (безопасная сторона).
            _ => {
                println!("ln8000-kmdf: неизвестный вердикт обхода ({reason}) — 1:1 не включаю");
            }
        },
        GuardAction::Stop { .. } => {
            *denied_strikes = 0;
            if let Err(err) = pump.standby() {
                println!("ln8000-kmdf: не удалось остановить заряд: {err}");
            }
        }
        // Перечисление помечено `non_exhaustive`: новые действия игнорируем.
        _ => {}
    }
}

/// Обрабатывает управляющие запросы пользовательского режима.
///
/// # Safety
///
/// Вызывается WDF; буферы запроса проверяются по размеру.
unsafe extern "C" fn evt_io_device_control(
    _queue: WDFQUEUE,
    request: WDFREQUEST,
    _output_buffer_length: usize,
    input_buffer_length: usize,
    io_control_code: ULONG,
) {
    // Обработчики ниже читают `STATE` и ходят по шине: без мьютекса они
    // столкнулись бы с таймером телеметрии.
    let _state = lock_state();
    match io_control_code {
        ioctl::IOCTL_LN8000_GET_STATUS => unsafe { handle_get_status(request) },
        ioctl::IOCTL_LN8000_READ_REG => unsafe { handle_read_reg(request, input_buffer_length) },
        ioctl::IOCTL_LN8000_WRITE_REG => unsafe { handle_write_reg(request, input_buffer_length) },
        ioctl::IOCTL_LN8000_SET_LIMITS => unsafe { handle_set_limits(request, input_buffer_length) },
        ioctl::IOCTL_LN8000_SET_MODE => unsafe { handle_set_mode(request, input_buffer_length) },
        ioctl::IOCTL_LN8000_SET_CHARGE => unsafe { handle_set_charge(request, input_buffer_length) },
        ioctl::IOCTL_LN8000_RUN_HVDCP => unsafe { handle_run_hvdcp(request, input_buffer_length) },
        ioctl::IOCTL_LN8000_GET_SESSIONS => unsafe { handle_get_sessions(request) },
        ioctl::IOCTL_LN8000_GET_SAMPLES => unsafe {
            handle_get_samples(request, input_buffer_length)
        },
        _ => unsafe {
            complete(request, wdk_sys::STATUS_INVALID_DEVICE_REQUEST, 0);
        },
    }
}

/// Autostart HVDCP negotiate via SUPERUSER (Usbin RH secondary).
fn try_autostart_hvdcp(device: WDFDEVICE) {
    mark_device_value(device, "HvdcpAuto", 1);
    let code = run_hvdcp_and_land(device);
    if code != 0 {
        println!("ln8000-kmdf: HVDCP autostart rc={code}");
    }
    schedule_or_clear_superuser_retry(device, code);
    arm_hvdcp_input_edge(device);
}

/// Schedule SUPERUSER retry when the slot was full; clear when negotiate progressed.
fn schedule_or_clear_superuser_retry(device: WDFDEVICE, negotiate_rc: i32) {
    // SAFETY: WDF serializes prepare / IOCTL / timer.
    let st = unsafe { state() };
    if hvdcp::should_schedule_superuser_retry(negotiate_rc) {
        if st.hvdcp_retry_attempts >= hvdcp::HVDCP_SUPERUSER_RETRY_MAX {
            st.hvdcp_retry_pending = false;
            mark_device_value(device, "HvdcpRetryPend", 2);
            println!("ln8000-kmdf: HVDCP SUPERUSER retry exhausted");
        } else {
            st.hvdcp_retry_pending = true;
            if st.hvdcp_retry_next_ms == 0 {
                st.hvdcp_retry_next_ms =
                    monotonic_ms().saturating_add(hvdcp::HVDCP_SUPERUSER_RETRY_MS);
            }
            mark_device_value(device, "HvdcpRetryPend", 1);
            mark_device_value(device, "HvdcpRetryN", st.hvdcp_retry_attempts);
            println!("ln8000-kmdf: HVDCP SUPERUSER busy — will retry");
        }
    } else {
        st.hvdcp_retry_pending = false;
        st.hvdcp_retry_attempts = 0;
        st.hvdcp_retry_next_ms = 0;
        mark_device_value(device, "HvdcpRetryPend", 0);
    }
}

/// After PrepareHardware autostart, seed cable-present so the first timer tick
/// does not look like a rising edge (would double-negotiate).
fn arm_hvdcp_input_edge(device: WDFDEVICE) {
    // SAFETY: prepare serialized with timer.
    let st = unsafe { state() };
    let vin = st
        .pump
        .as_mut()
        .and_then(|p| p.read_adc(AdcChannel::Vin).ok())
        .unwrap_or(0);
    st.last_input_present = hvdcp::input_present_from_vin(vin);
    st.hvdcp_edge_armed = true;
    mark_device_value(
        device,
        "HvdcpEdgeArm",
        u32::from(st.last_input_present),
    );
}

/// Negotiate + post-path (boost/trim/ICL + set_charging). Shared by autostart,
/// SUPERUSER retry, re-plug edge, and `IOCTL_RUN_HVDCP`.
fn run_hvdcp_and_land(device: WDFDEVICE) -> i32 {
    // SAFETY: called from prepare / IOCTL / timer; WDF serializes them.
    let st = unsafe { state() };
    let usbin_id = st.usbin_id;
    let vbat_uv = st
        .pump
        .as_mut()
        .and_then(|p| p.read_adc(AdcChannel::Vbat).ok())
        .map(|v| u32::try_from(v.max(0)).unwrap_or(0))
        .unwrap_or(4_000_000);
    let (code, _) = {
        let pump = &mut st.pump;
        let hvdcp_st = &mut st.hvdcp;
        let mut read_vin = || {
            pump
                .as_mut()
                .and_then(|p| p.read_adc(AdcChannel::Vin).ok())
                .unwrap_or(0)
        };
        // SAFETY: PASSIVE_LEVEL; SUPERUSER preferred, Usbin id optional.
        unsafe { hvdcp::run_negotiate_report(device, usbin_id, vbat_uv, hvdcp_st, &mut read_vin) }
    };
    // After Vin elevation (or 5 V stay), land in the correct pump path:
    //  Vin >= 2*Vbat + 250 мВ → boost/trim + ICL pump + 2:1
    //  ~5 V → max safe IIN retreat bypass (plain DCP / QC2 brick без elevate)
    //  повышен, но без запаса (8,0–9,15 В) → сначала дотягиваем шину до окна
    //  9,5–9,8 В (Android `cp_qc30.c:848`: UP, пока `vbus <= 9500`), затем
    //  перечитываем ADC и решаем заново. Раньше этот случай ничего не
    //  исправлял: boost стоял внутри `if engage == Switching`, то есть за
    //  условием, которое сам должен создать, и такт кончался `ModeNotReached`.
    if let Some(pump) = st.pump.as_mut() {
        let mut vin = pump.read_adc(AdcChannel::Vin).unwrap_or(0);
        let vbat_now = pump
            .read_adc(AdcChannel::Vbat)
            .ok()
            .map_or(vbat_uv, |v| u32::try_from(v.max(0)).unwrap_or(vbat_uv));
        let mut engage = charge_mode(vin, vbat_now);
        if engage != Some(OpMode::Switching) && vin >= hvdcp::FIVE_V_STAY_MAX_UV {
            // Вход повышен (не 5-вольтовая ветка), но 2:1 ещё не допускается:
            // доводим шину до полосы переноса и перечитываем ADC. Порог —
            // «выше 5 В», а не `SWITCHING_MIN_VIN_UV`: живой QC3-блок в
            // continuous-режиме встаёт чуть НИЖЕ ворот 2:1 (замер 19.09: 7,97 В
            // против пола 8,0 В), и последний шаг делает INC-импульс, а не отказ.
            // SAFETY: PASSIVE_LEVEL; may reopen SUPERUSER for INC pulses.
            let _ = unsafe {
                hvdcp::nudge_vin_into_window(device, usbin_id, &mut st.hvdcp, vin, vbat_now, false)
            };
            vin = pump.read_adc(AdcChannel::Vin).unwrap_or(vin);
            engage = charge_mode(vin, vbat_now);
        }
        if engage == Some(OpMode::Switching) {
            mark_device_value(device, "SuAfc5vPath", 0);
            // Полоса переноса едет за банкой (`[2*Vbat+200, 2*Vbat+400]` мВ), так
            // что её держит живой Vbat, а не фиксированные 9,5–10,5 В: те лежат
            // ВЫШЕ полосы на полной банке, и насос отдаёт ровно 39 мА (живой
            // замер 18.09). `false` — ток здесь ещё не измерен, коррекция только
            // по напряжению; мёртвый ток ловит такт телеметрии.
            // SAFETY: PASSIVE_LEVEL; may reopen SUPERUSER for pulses.
            let _ = unsafe {
                hvdcp::nudge_vin_into_window(device, usbin_id, &mut st.hvdcp, vin, vbat_now, false)
            };
            // Перечитывать ADC здесь не для чего: `set_charging` ниже читает
            // Vin и Vbat сам, а режим уже выбран выше.
            // SAFETY: PASSIVE_LEVEL; SUPERUSER / Usbin RH for ICL.
            let _ = unsafe { hvdcp::raise_icl_for_pump(device, usbin_id, &mut st.hvdcp) };
        } else if engage == Some(OpMode::Bypass) && hvdcp::vin_stayed_near_5v(vin) {
            // Plain DCP / QC2 FORCE no-op / brick без elevate: обход 1:1 @ 2.7 А.
            // SAFETY: PASSIVE_LEVEL; raises USBIN ICL for 5 V high-current.
            let _ = unsafe { hvdcp::raise_icl_for_5v_bypass(device, usbin_id, &mut st.hvdcp) };
            // Align pump IIN limit with NABU class-B bus budget.
            let _ = pump.set_iin_limit(2_700_000);
        }
        let result = match recover_ln_shutdown(pump) {
            ShutdownRecovery::Recovered(outcome) => {
                st.last_charge_attempt_ms = monotonic_ms();
                outcome
            }
            ShutdownRecovery::NotInShutdown => pump.set_charging(true, &mut por_delay),
        };
        st.charge_attempts = st.charge_attempts.saturating_add(1);
        st.last_enable_ms = monotonic_ms();
        mark_charge_attempt(device, st.charge_attempts, st.last_enable_ms, &result);
        match result {
            Ok(mode) => {
                st.auto_starts = st.auto_starts.saturating_add(1);
                st.failed_attempts = 0;
                st.last_error = 0;
                mark_device_value(device, "PostHvdcpMode", u32::from(mode.code()));
                mark_device_value(device, "PostHvdcpErr", 0);
                println!("ln8000-kmdf: post-HVDCP charge mode {}", mode.label());
            }
            Err(err) => {
                let ec = pump_error_code(err);
                st.last_error = ec;
                mark_device_value(device, "PostHvdcpMode", 0);
                mark_device_value(device, "PostHvdcpErr", ec as u32);
                println!("ln8000-kmdf: post-HVDCP set_charging failed rc={ec}");
            }
        }
    }
    code
}

/// IOCTL: run or report HVDCP state (SUPERUSER preferred; Usbin RH secondary).
///
/// # Safety
///
/// `request` is a live WDF request; input may be empty (defaults to command=1).
unsafe fn handle_run_hvdcp(request: WDFREQUEST, input_length: usize) {
    let mut answer = Ln8000HvdcpRequest::default();
    if input_length >= core::mem::size_of::<Ln8000HvdcpRequest>() {
        if let Some(req) = unsafe { read_input::<Ln8000HvdcpRequest>(request) } {
            answer.command = req.command;
        }
    } else if input_length > 0 {
        unsafe { complete(request, wdk_sys::STATUS_BUFFER_TOO_SMALL, 0) };
        return;
    } else {
        answer.command = 1;
    }

    let queue =
        unsafe { call_unsafe_wdf_function_binding!(WdfRequestGetIoQueue, request) };
    let device = unsafe { call_unsafe_wdf_function_binding!(WdfIoQueueGetDevice, queue) };

    // SAFETY: IOCTL queue is sequential with timer/prepare.
    let st = unsafe { state() };
    let vbat_uv = st
        .pump
        .as_mut()
        .and_then(|p| p.read_adc(AdcChannel::Vbat).ok())
        .map(|v| u32::try_from(v.max(0)).unwrap_or(0))
        .unwrap_or(4_000_000);

    if answer.command == 0 {
        // Status-only: never touches the bus. Transport may still be SUPERUSER.
        answer.error_code = st.hvdcp.phase.code() as i32;
        if st.hvdcp.phase == hvdcp::HvdcpPhase::Idle && st.usbin_id.is_none() {
            // Idle + no overlay: report 0 (SUPERUSER may still work on command=1).
            answer.error_code = 0;
        }
    } else {
        // Re-elevate is always allowed: FORCE_5V / FiveVBypass / prior Done must
        // not stick until driver reload. RUN_HVDCP re-opens SUPERUSER and negotiates.
        let code = run_hvdcp_and_land(device);
        answer.error_code = code;
        schedule_or_clear_superuser_retry(device, code);
        // Refresh edge baseline after intentional negotiate.
        arm_hvdcp_input_edge(device);
    }

    answer.apsd_status = st.hvdcp.apsd_status;
    answer.apsd_result = st.hvdcp.apsd_result;
    answer.pulse_cnt = u8::try_from(st.hvdcp.pulse_cnt.min(255)).unwrap_or(255);
    answer.phase = st.hvdcp.phase.code();
    if answer.target_vbus_uv == 0 {
        answer.target_vbus_uv = hvdcp::target_vbus_uv(vbat_uv);
    }
    answer.estimated_vbus_uv = hvdcp::estimated_vbus_uv(st.hvdcp.pulse_cnt);
    unsafe { write_output(request, &answer) };
}

/// Отдаёт состояние драйвера.
///
/// # Safety
///
/// `request` валиден; выходной буфер достаточного размера.
unsafe fn handle_get_status(request: WDFREQUEST) {
    // SAFETY: доступ сериализован WDF.
    let st = unsafe { state() };
    let mut status = Ln8000Status {
        magic: LN8000_STATUS_MAGIC,
        version: LN8000_STATUS_VERSION,
        state: match st.pump.as_ref().map(Pump::state) {
            Some(PumpState::Probed) => 1,
            Some(PumpState::Configured) => 2,
            Some(PumpState::Switching) => 3,
            Some(PumpState::Faulted) => 4,
            _ => 0,
        },
        writes: st.writes,
        reads: st.reads,
        last_error: st.last_error,
        sessions: st.telemetry.session_total(),
        samples: st.telemetry.sample_total(),
        ..Ln8000Status::default()
    };
    if let Some(pump) = st.pump.as_ref() {
        status.op_mode = pump.op_mode().code();
    }
    if let Some(pump) = st.pump.as_mut() {
        if let Ok(live) = pump.status() {
            status.op_mode = live.op_mode.code();
            status.sys_sts = live.sys_sts;
            status.fault1_sts = live.fault1_sts;
            status.fault2_sts = live.fault2_sts;
            status.safety_sts = live.safety_sts;
            status.critical_fault = u8::from(live.has_critical_fault());
        }
    }
    if let Some(sample) = st.telemetry.last_sample() {
        status.iin_ua = sample.iin_ua;
        status.vbat_uv = sample.vbat_uv;
        status.vbus_uv = sample.vbus_uv;
        status.die_temp_dc = sample.die_temp_dc;
        // Prefer live op_mode already filled above; keep sample only if no pump.
        if st.pump.is_none() {
            status.op_mode = sample.op_mode.code();
        }
    } else if let Some(pump) = st.pump.as_mut() {
        // Сохранённого снимка ещё нет (периодический сбор не наполнил его),
        // поэтому читаем показатели на месте: обмен по шине занимает единицы
        // миллисекунд, и обработчик не блокируется.
        status.vbat_uv =
            u32::try_from(pump.read_adc(AdcChannel::Vbat).unwrap_or_default().max(0)).unwrap_or(0);
        status.vbus_uv =
            u32::try_from(pump.read_adc(AdcChannel::Vin).unwrap_or_default().max(0)).unwrap_or(0);
        status.iin_ua =
            u32::try_from(pump.read_adc(AdcChannel::Iin).unwrap_or_default().max(0)).unwrap_or(0);
        status.die_temp_dc = pump.read_adc(AdcChannel::DieTemp).unwrap_or_default();
    }
    // Keep BattC SoC fresh even when the telemetry timer failed to start.
    if status.vbat_uv > 0 || status.vbus_uv > 0 {
        unsafe {
            battery::update_from_telemetry(
                status.vbat_uv,
                status.vbus_uv,
                status.iin_ua,
                st.max_iin_ua,
                monotonic_ms(),
            );
        }
        let device = unsafe { DEVICE };
        if !device.is_null() {
            mark_device_value(device, "BattPct", battery::last_percent());
            mark_device_value(device, "BattVbat", status.vbat_uv / 1000);
            mark_device_value(device, "BattPwr", battery::last_power_state());
        }
    }
    unsafe { write_output(request, &status) };
}

/// Читает регистр LN8000 (диагностика).
///
/// # Safety
///
/// `request` валиден; входной буфер содержит [`Ln8000RegRequest`].
unsafe fn handle_read_reg(request: WDFREQUEST, input_length: usize) {
    if input_length < core::mem::size_of::<Ln8000RegRequest>() {
        unsafe { complete(request, wdk_sys::STATUS_BUFFER_TOO_SMALL, 0) };
        return;
    }
    // SAFETY: буфер проверен по размеру; адрес берётся из запроса клиента.
    let Some(mut answer) = (unsafe { read_input::<Ln8000RegRequest>(request) }) else {
        unsafe { complete(request, wdk_sys::STATUS_INVALID_PARAMETER, 0) };
        return;
    };
    // SAFETY: доступ сериализован WDF.
    let st = unsafe { state() };
    if let Some(pump) = st.pump.as_mut() {
        match pump.read_register(answer.addr) {
            Ok(value) => answer.value = value,
            Err(err) => answer.error_code = pump_error_code(err),
        }
        let (writes, reads) = pump.counters();
        st.writes = writes;
        st.reads = reads;
    } else {
        answer.error_code = -1;
    }
    unsafe { write_output(request, &answer) };
}

/// Записывает регистр LN8000 (диагностика).
///
/// # Safety
///
/// `request` валиден; входной буфер содержит [`Ln8000RegRequest`].
unsafe fn handle_write_reg(request: WDFREQUEST, input_length: usize) {
    if input_length < core::mem::size_of::<Ln8000RegRequest>() {
        unsafe { complete(request, wdk_sys::STATUS_BUFFER_TOO_SMALL, 0) };
        return;
    }
    // SAFETY: буфер проверен по размеру; адрес и значение — из запроса клиента.
    let Some(mut answer) = (unsafe { read_input::<Ln8000RegRequest>(request) }) else {
        unsafe { complete(request, wdk_sys::STATUS_INVALID_PARAMETER, 0) };
        return;
    };
    // SAFETY: доступ сериализован WDF.
    let st = unsafe { state() };
    if let Some(pump) = st.pump.as_mut() {
        if let Err(err) = pump.write_register(answer.addr, answer.value) {
            answer.error_code = pump_error_code(err);
        }
        let (writes, reads) = pump.counters();
        st.writes = writes;
        st.reads = reads;
    } else {
        answer.error_code = -1;
    }
    unsafe { write_output(request, &answer) };
}

/// Задаёт лимиты тока и напряжения.
///
/// # Safety
///
/// `request` валиден; входной буфер содержит [`Ln8000LimitsRequest`].
unsafe fn handle_set_limits(request: WDFREQUEST, input_length: usize) {
    if input_length < core::mem::size_of::<Ln8000LimitsRequest>() {
        unsafe { complete(request, wdk_sys::STATUS_BUFFER_TOO_SMALL, 0) };
        return;
    }
    // SAFETY: буфер проверен по размеру.
    let Some(mut answer) = (unsafe { read_input::<Ln8000LimitsRequest>(request) }) else {
        unsafe { complete(request, wdk_sys::STATUS_INVALID_PARAMETER, 0) };
        return;
    };
    // SAFETY: доступ сериализован WDF.
    let st = unsafe { state() };
    if let Some(pump) = st.pump.as_mut() {
        if answer.iin_ua > 0 {
            match pump.set_iin_limit(answer.iin_ua) {
                Ok(code) => {
                    // Сообщаем не запрошенный ток, а фактически применённый:
                    // кодирование округляет значение до шага 50 мА.
                    let applied = decode_iin_limit(code);
                    answer.applied_iin_ua = applied;
                    st.limits.iin_max_ua = applied;
                    st.limits.iin_target_ua = applied;
                }
                Err(err) => answer.error_code = pump_error_code(err),
            }
        }
        if answer.vbat_uv > 0 {
            if let Err(err) = pump.set_vbat_float(answer.vbat_uv) {
                answer.error_code = pump_error_code(err);
            }
        }
    } else {
        answer.error_code = -1;
    }
    unsafe { write_output(request, &answer) };
}

/// Переключает режим работы.
///
/// # Safety
///
/// `request` валиден; входной буфер содержит [`Ln8000ModeRequest`].
unsafe fn handle_set_mode(request: WDFREQUEST, input_length: usize) {
    if input_length < core::mem::size_of::<Ln8000ModeRequest>() {
        unsafe { complete(request, wdk_sys::STATUS_BUFFER_TOO_SMALL, 0) };
        return;
    }
    // SAFETY: буфер проверен по размеру.
    let Some(mut answer) = (unsafe { read_input::<Ln8000ModeRequest>(request) }) else {
        unsafe { complete(request, wdk_sys::STATUS_INVALID_PARAMETER, 0) };
        return;
    };
    if !(1..=3).contains(&answer.mode) {
        unsafe { complete(request, wdk_sys::STATUS_INVALID_PARAMETER, 0) };
        return;
    }
    // SAFETY: доступ сериализован WDF.
    let st = unsafe { state() };
    if let Some(pump) = st.pump.as_mut() {
        // mode 2 (1:1) — только в окне обхода: `EN_1TO1` подаёт вход напрямую на
        // батарею, поэтому на повышенном Vin отказываем отдельным кодом, а не
        // пропускаем вызов в чип.
        if answer.mode == 2 {
            let vin = pump.read_adc(AdcChannel::Vin).unwrap_or(0);
            let vbat =
                u32::try_from(pump.read_adc(AdcChannel::Vbat).unwrap_or(0).max(0)).unwrap_or(0);
            if !bypass_allowed_by_vin(vin, vbat) {
                println!(
                    "ln8000-kmdf: SET_MODE bypass отклонён: Vin {vin} мкВ вне окна обхода"
                );
                answer.error_code = ioctl::ERR_BYPASS_VIN_OUT_OF_WINDOW;
                unsafe { write_output(request, &answer) };
                return;
            }
        }
        let outcome = match answer.mode {
            1 => pump.standby().map(|()| OpMode::Standby.code()),
            2 => pump.enable_bypass().map(|mode| mode.code()),
            _ => pump.enable_switching().map(|mode| mode.code()),
        };
        match outcome {
            Ok(mode) => answer.applied_mode = mode,
            Err(err) => answer.error_code = pump_error_code(err),
        }
    } else {
        answer.error_code = -1;
    }
    unsafe { write_output(request, &answer) };
}

/// Явный старт/стоп заряда через [`IOCTL_LN8000_SET_CHARGE`].
///
/// # Safety
///
/// `request` валиден; входной/выходной буфер достаточного размера.
unsafe fn handle_set_charge(request: WDFREQUEST, input_length: usize) {
    if input_length < core::mem::size_of::<Ln8000ChargeRequest>() {
        unsafe { complete(request, wdk_sys::STATUS_BUFFER_TOO_SMALL, 0) };
        return;
    }
    // SAFETY: буфер проверен по размеру.
    let Some(mut answer) = (unsafe { read_input::<Ln8000ChargeRequest>(request) }) else {
        unsafe { complete(request, wdk_sys::STATUS_INVALID_PARAMETER, 0) };
        return;
    };
    // SAFETY: доступ к глобальному состоянию WDF.
    let st = unsafe { state() };
    let queue = unsafe { call_unsafe_wdf_function_binding!(WdfRequestGetIoQueue, request) };
    let device = unsafe { call_unsafe_wdf_function_binding!(WdfIoQueueGetDevice, queue) };
    if let Some(pump) = st.pump.as_mut() {
        let result = if answer.on != 0 {
            // Soft-reset + configure уже сами пробуют включить заряд в этом такте.
            let outcome = match recover_ln_shutdown(pump) {
                ShutdownRecovery::Recovered(outcome) => outcome,
                ShutdownRecovery::NotInShutdown => pump.set_charging(true, &mut por_delay),
            };
            st.charge_attempts = st.charge_attempts.saturating_add(1);
            st.last_enable_ms = monotonic_ms();
            mark_charge_attempt(device, st.charge_attempts, st.last_enable_ms, &outcome);
            outcome
        } else {
            pump.set_charging(false, &mut por_delay)
        };
        match result {
            Ok(applied) => {
                answer.applied_mode = applied.code();
                answer.sys_sts = pump.status().map_or(0, |s| s.sys_sts);
                answer.error_code = 0;
                st.last_error = 0;
            }
            Err(err) => {
                let code = pump_error_code(err);
                answer.error_code = code;
                st.last_error = code;
            }
        }
    } else {
        answer.error_code = -1;
    }
    unsafe { write_output(request, &answer) };
}

/// Отдаёт сведения о сеансах заряда.
///
/// # Safety
///
/// `request` валиден; вывод проверяется вызывающим.
unsafe fn handle_get_sessions(request: WDFREQUEST) {
    // SAFETY: доступ сериализован WDF.
    let st = unsafe { state() };
    let mut answer = Ln8000Sessions {
        total: st.telemetry.session_total(),
        ..Ln8000Sessions::default()
    };
    let now = monotonic_ms();
    if let Some(current) = st.telemetry.current() {
        answer.current_ms = current.duration_ms(now);
        answer.current_peak_iin_ua = current.peak_iin_ua;
        answer.current_fast = u8::from(current.had_fast_mode());
    }
    if let Some(last) = st.telemetry.last_completed() {
        answer.last_ms = last.duration_ms(now);
        answer.last_peak_iin_ua = last.peak_iin_ua;
        answer.last_peak_temp_dc = last.peak_die_temp_dc;
        answer.last_fast = u8::from(last.had_fast_mode());
    }
    unsafe { write_output(request, &answer) };
}

/// Выгружает отсчёты телеметрии.
///
/// # Safety
///
/// `request` валиден; выходной буфер заполняется поэлементно.
unsafe fn handle_get_samples(request: WDFREQUEST, input_length: usize) {
    if input_length < core::mem::size_of::<Ln8000SamplesRequest>() {
        unsafe { complete(request, wdk_sys::STATUS_BUFFER_TOO_SMALL, 0) };
        return;
    }
    // SAFETY: буфер проверен по размеру.
    let requested_count = unsafe { read_input::<Ln8000SamplesRequest>(request) };
    // SAFETY: доступ сериализован WDF.
    let st = unsafe { state() };
    let (buffer, length) = match unsafe {
        output_buffer(request, core::mem::size_of::<Ln8000SamplesRequest>())
    } {
        Ok(value) => value,
        Err(status) => {
            unsafe { complete(request, status, 0) };
            return;
        }
    };
    let header_size = core::mem::size_of::<Ln8000SamplesRequest>();
    let sample_size = core::mem::size_of::<Ln8000Sample>();
    if sample_size == 0 {
        unsafe { complete(request, wdk_sys::STATUS_INVALID_PARAMETER, 0) };
        return;
    }
    let capacity = length.saturating_sub(header_size) / sample_size;
    let requested = requested_count
        .map_or(capacity, |info| usize::try_from(info.count).unwrap_or(capacity));
    let limit = capacity.min(requested).min(ln8000::session::SAMPLE_RING);
    let base = buffer.cast::<u8>();
    let mut written = 0_usize;
    st.telemetry.for_each_sample(|sample| {
        if written < limit {
            // SAFETY: запись идёт внутри выходного буфера запроса, а
            // `written < limit <= capacity` не даёт выйти за его границы.
            unsafe {
                let destination = base
                    .add(header_size + written * sample_size)
                    .cast::<Ln8000Sample>();
                core::ptr::write(destination, to_sample(sample));
            }
            written = written.saturating_add(1);
        }
    });
    let mut header = Ln8000SamplesRequest {
        count: u32::try_from(limit).unwrap_or(0),
        available: u32::try_from(written).unwrap_or(0),
        ..Ln8000SamplesRequest::default()
    };
    if written > 0 {
        // SAFETY: первый записанный отсчёт лежит сразу за заголовком.
        unsafe {
            core::ptr::copy_nonoverlapping(
                base.add(header_size),
                core::ptr::from_mut(&mut header.first).cast::<u8>(),
                sample_size,
            );
        }
    }
    // SAFETY: заголовок пишется в начало выходного буфера запроса.
    unsafe {
        core::ptr::copy_nonoverlapping(core::ptr::from_ref(&header).cast::<u8>(), base, header_size);
    }
    let information = header_size + written * sample_size;
    unsafe { complete(request, wdk_sys::STATUS_SUCCESS, information) };
}

/// Преобразует отсчёт ядра в структуру ответа.
fn to_sample(sample: &TelemetrySample) -> Ln8000Sample {
    Ln8000Sample {
        ts_ms: sample.ts_ms,
        vbat_uv: sample.vbat_uv,
        vbus_uv: sample.vbus_uv,
        iin_ua: sample.iin_ua,
        die_temp_dc: sample.die_temp_dc,
        op_mode: sample.op_mode.code(),
        input_present: u8::from(sample.input_present),
        reserved: [0; 2],
    }
}

/// Копирует входной буфер запроса в структуру.
///
/// Возвращает `None`, если буфер меньше `size_of::<T>()`.
///
/// # Safety
///
/// `request` валиден; вызов на уровне, допускающем доступ к буферу запроса.
unsafe fn read_input<T: Copy + Default>(request: WDFREQUEST) -> Option<T> {
    let mut buffer: *mut core::ffi::c_void = core::ptr::null_mut();
    let mut length: usize = 0;
    // SAFETY: входной буфер запроса создаётся фреймворком (METHOD_BUFFERED).
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfRequestRetrieveInputBuffer,
            request,
            core::mem::size_of::<T>(),
            &raw mut buffer,
            &raw mut length,
        )
    };
    if status < 0 || buffer.is_null() || length < core::mem::size_of::<T>() {
        return None;
    }
    let mut value = T::default();
    // SAFETY: буфер проверен по размеру; копируем ровно size_of::<T>().
    unsafe {
        core::ptr::copy_nonoverlapping(
            buffer.cast::<u8>(),
            core::ptr::from_mut(&mut value).cast::<u8>(),
            core::mem::size_of::<T>(),
        );
    }
    Some(value)
}

/// Отдаёт выходной буфер запроса и его размер.
///
/// # Safety
///
/// `request` валиден.
unsafe fn output_buffer(
    request: WDFREQUEST,
    min_length: usize,
) -> Result<(*mut core::ffi::c_void, usize), NTSTATUS> {
    let mut buffer: *mut core::ffi::c_void = core::ptr::null_mut();
    let mut length: usize = 0;
    // SAFETY: выходной буфер запроса создаётся фреймворком (METHOD_BUFFERED).
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfRequestRetrieveOutputBuffer,
            request,
            min_length,
            &raw mut buffer,
            &raw mut length,
        )
    };
    if status < 0 {
        return Err(status);
    }
    Ok((buffer, length))
}

/// Копирует структуру в выходной буфер запроса и завершает его.
///
/// # Safety
///
/// `request` валиден; `value` указывает на живую структуру.
unsafe fn write_output<T: Copy>(request: WDFREQUEST, value: &T) {
    let (buffer, _length) = match unsafe { output_buffer(request, core::mem::size_of::<T>()) } {
        Ok(value) => value,
        Err(status) => {
            unsafe { complete(request, status, 0) };
            return;
        }
    };
    // SAFETY: буфер проверен по размеру; копируем ровно size_of::<T>().
    unsafe {
        core::ptr::copy_nonoverlapping(
            core::ptr::from_ref(value).cast::<u8>(),
            buffer.cast::<u8>(),
            core::mem::size_of::<T>(),
        );
    }
    unsafe { complete(request, wdk_sys::STATUS_SUCCESS, core::mem::size_of::<T>()) };
}

/// Завершает запрос с кодом и объёмом данных.
///
/// # Safety
///
/// `request` — валидный незавершённый запрос.
unsafe fn complete(request: WDFREQUEST, status: NTSTATUS, information: usize) {
    // SAFETY: запрос принадлежит этому вызову и ещё не завершён.
    unsafe {
        call_unsafe_wdf_function_binding!(
            WdfRequestCompleteWithInformation,
            request,
            status,
            u64::try_from(information).unwrap_or(0),
        );
    }
}

/// Монотонные миллисекунды ядра.
fn monotonic_ms() -> u64 {
    let mut stamp: u64 = 0;
    // SAFETY: `KeQueryInterruptTimePrecise` — документированная ядерная функция;
    // единица измерения — 100 нс, поэтому делим на 10 000.
    let ticks = unsafe { wdk_sys::ntddk::KeQueryInterruptTimePrecise(&raw mut stamp) };
    ticks / 10_000
}

fn pump_error_code(err: PumpError) -> i32 {
    match err {
        PumpError::Bus(_) => -1,
        PumpError::NotOpen => -2,
        PumpError::WrongDeviceId { .. } => -3,
        PumpError::ModeNotReached { .. } => -4,
        PumpError::Fault { .. } => -5,
        PumpError::OutOfRange { .. } => -6,
        PumpError::WatchdogExpired => -7,
        // Запрет политики, а не отказ чипа: 1:1 вне окна обхода.
        PumpError::BypassNeedsFiveVoltVin { .. } => ioctl::ERR_BYPASS_VIN_OUT_OF_WINDOW,
        // Перечисление помечено `non_exhaustive`: новые варианты дадут -100.
        _ => -100,
    }
}

fn size_of_ulong<T>() -> ULONG {
    u32::try_from(core::mem::size_of::<T>()).unwrap_or(0)
}
