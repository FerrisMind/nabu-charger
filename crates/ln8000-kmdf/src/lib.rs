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

mod ioctl;
mod spb;
mod spb_abi;

extern crate wdk_panic;

use ioctl::{
    Ln8000LimitsRequest, Ln8000ModeRequest, Ln8000RegRequest, Ln8000Sample, Ln8000SamplesRequest, Ln8000Sessions,
    Ln8000Status, LN8000_STATUS_MAGIC, LN8000_STATUS_VERSION,
};
use ln8000::encoding::decode_iin_limit;
use ln8000::{
    AdcChannel, GuardAction, GuardLimits, OpMode, Pump, PumpConfig, PumpError, PumpState, Telemetry,
    TelemetrySample, evaluate,
};
use spb::SpbBus;
use wdk::println;
use wdk_sys::{
    _WDF_EXECUTION_LEVEL::WdfExecutionLevelPassive,
    _WDF_IO_QUEUE_DISPATCH_TYPE::WdfIoQueueDispatchSequential,
    _WDF_SYNCHRONIZATION_SCOPE::WdfSynchronizationScopeNone, _WDF_TRI_STATE::WdfTrue,
    call_unsafe_wdf_function_binding, CmResourceTypeConnection, NTSTATUS, PCUNICODE_STRING,
    PLUGPLAY_REGKEY_DEVICE, PWDFDEVICE_INIT, ULONG, UNICODE_STRING, WDFCMRESLIST, WDFDEVICE, WDFDRIVER,
    WDFKEY, WDFQUEUE, WDFREQUEST, WDFTIMER, WDF_DRIVER_CONFIG, WDF_IO_QUEUE_CONFIG, WDF_NO_HANDLE,
    WDF_NO_OBJECT_ATTRIBUTES, WDF_OBJECT_ATTRIBUTES, WDF_PNPPOWER_EVENT_CALLBACKS, WDF_TIMER_CONFIG,
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

/// Период телеметрии по умолчанию, мс.
const TELEMETRY_PERIOD_MS: u32 = 1_000;

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
            telemetry_ms: 1000,
        }
    }
}

/// Единственный экземпляр состояния драйвера.
static mut STATE: DriverState = DriverState::new();

/// Доступ к состоянию драйвера.
///
/// # Safety
///
/// Вызывается только из очереди и таймера, которые WDF сериализует.
unsafe fn state() -> &'static mut DriverState {
    // SAFETY: см. инварианты `DriverState`.
    unsafe { &mut *core::ptr::addr_of_mut!(STATE) }
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
        let _ = call_unsafe_wdf_function_binding!(WdfRegistryClose, key);
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
        let _ = call_unsafe_wdf_function_binding!(WdfRegistryClose, key);
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
        let _ = call_unsafe_wdf_function_binding!(WdfRegistryClose, key);
    }
}

/// Записывает одно значение в ключ устройства — для разовой диагностики.
fn mark_device_value(device: WDFDEVICE, name: &str, value: u32) {
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
        let _ = call_unsafe_wdf_function_binding!(WdfRegistryClose, key);
    }
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
    let mut telemetry_ms = 1000_u32;

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
            let _ = call_unsafe_wdf_function_binding!(WdfRegistryClose, device_key);
        }
        return DriverParams {
            config,
            limits,
            telemetry_ms,
        };
    }

    for name in [
        "IinLimitUa",
        "VbatFloatUv",
        "VacOvpUv",
        "NtcAlarmCfg",
        "WatchdogEnabled",
        "ProtectionProfile",
    ] {
        // SAFETY: ключ Parameters открыт на чтение.
        if let Some(value) = unsafe { query_ulong(params_key, name) } {
            if !config.apply_parameter(name, value) {
                println!("ln8000-kmdf: параметр {name} = {value} отклонён, остаётся значение по умолчанию");
            }
        }
    }
    // SAFETY: ключ Parameters открыт на чтение.
    if let Some(ms) = unsafe { query_ulong(params_key, "TelemetryMs") } {
        if (100..=60_000).contains(&ms) {
            telemetry_ms = ms;
        } else {
            println!("ln8000-kmdf: период телеметрии {ms} мс вне границ 100..60000, беру 1000");
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
        let _ = call_unsafe_wdf_function_binding!(WdfRegistryClose, params_key);
        let _ = call_unsafe_wdf_function_binding!(WdfRegistryClose, device_key);
    }

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

    // Периодический таймер телеметрии живёт на устройстве.
    let mut timer_config = WDF_TIMER_CONFIG {
        Size: size_of_ulong::<WDF_TIMER_CONFIG>(),
        Period: TELEMETRY_PERIOD_MS,
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
    }

    println!("ln8000-kmdf: устройство готово");
    mark_driver(driver, STAGE_DONE, 0);
    mark_stage(device, STAGE_DONE, 0);
    wdk_sys::STATUS_SUCCESS
}

/// Таймер телеметрии: единственный экземпляр на драйвер.
static mut TIMER: WDFTIMER = core::ptr::null_mut();

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
    // 1. Ищем ресурс подключения (I²C) и забираем идентификатор.
    mark_stage(device, STAGE_PREPARE, 0);
    let peripheral_id = match unsafe { find_peripheral_id(resources_translated) } {
        Some(id) => id,
        None => {
            println!("ln8000-kmdf: в _CRS нет I²C-подключения (узел PEIC не найден)");
            mark_stage(device, STAGE_PREPARE_BUS, wdk_sys::STATUS_DEVICE_NOT_READY);
            return wdk_sys::STATUS_DEVICE_NOT_READY;
        }
    };
    println!("ln8000-kmdf: подключение {peripheral_id:#018X}");
    // SAFETY: пассивный уровень, устройство создано.
    let mut bus = match unsafe { SpbBus::open(device, peripheral_id) } {
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
    // Проверка цели: принимает ли узел управляющий запрос SPB вообще.
    let lock = bus.lock_connection();
    mark_device_value(device, "LockStatus", lock as u32);
    mark_device_value(device, "LockOk", if lock >= 0 { 1 } else { 0 });
    // Подключение к периферии: этот шаг делает эталонный драйвер до доступа
    // к регистрам. Записываем статус и первые слова ответа.
    let attach = bus.attach();
    mark_device_value(device, "AttachStatus", attach as u32);
    for (index, name) in [(0_usize, "Att0"), (1, "Att1"), (2, "Att2"), (3, "Att3")] {
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

    let mut pump = match Pump::open(bus, config) {
        Ok(pump) => pump,
        Err(err) => {
            println!("ln8000-kmdf: LN8000 не опознан: {err}");
            let st = unsafe { state() };
            st.last_error = -2;
            mark_stage(device, STAGE_PREPARE_CHIP, 0);
            return wdk_sys::STATUS_DEVICE_NOT_READY;
        }
    };
    if let Err(err) = pump.configure() {
        println!("ln8000-kmdf: конфигурация не удалась: {err}");
        let st = unsafe { state() };
        st.last_error = -3;
        mark_stage(device, STAGE_PREPARE_CONFIG, 0);
        return wdk_sys::STATUS_DEVICE_NOT_READY;
    }

    // 4. Пробуем включить ускоренный режим. Если чип его не подтвердил —
    //    работаем дальше в bypass: зарядка должна быть безопасной и рабочей.
    //    Если не подтверждается и bypass — уводим чип в standby: без рабочего
    //    режима он не должен оставаться в неопределённом состоянии.
    match pump.enable_switching_or_bypass() {
        Ok(mode) => println!("ln8000-kmdf: режим {}", mode.label()),
        Err(err) => {
            println!("ln8000-kmdf: ни 2:1, ни bypass не подтверждены ({err}); уходим в standby");
            if let Err(standby_error) = pump.standby() {
                println!("ln8000-kmdf: standby тоже не подтверждён: {standby_error}");
            }
        }
    }

    let st = unsafe { state() };
    st.pump = Some(pump);
    st.telemetry_ms = params.telemetry_ms;
    st.limits = guard_limits;

    // 5. Запускаем телеметрию с периодом из реестра.
    // SAFETY: таймер создан в `evt_device_add`.
    let timer = unsafe { TIMER };
    if !timer.is_null() {
        let period = i64::from(st.telemetry_ms.max(100));
        unsafe {
            let _ = call_unsafe_wdf_function_binding!(WdfTimerStart, timer, -10_000_i64 * period);
        }
    }

    mark_stage(device, STAGE_READY, 0);
    wdk_sys::STATUS_SUCCESS
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
    // SAFETY: доступ сериализован WDF.
    let timer = unsafe { TIMER };
    if !timer.is_null() {
        unsafe {
            let _ = call_unsafe_wdf_function_binding!(WdfTimerStop, timer, 0);
        }
    }
    // SAFETY: см. инварианты `DriverState`.
    let st = unsafe { state() };
    if let Some(pump) = st.pump.as_mut() {
        if let Err(err) = pump.standby() {
            println!("ln8000-kmdf: не удалось уйти в standby: {err}");
        }
        pump.close();
    }
    st.pump = None;
    wdk_sys::STATUS_SUCCESS
}

/// Периодический сбор телеметрии, журналирование и защита.
///
/// # Safety
///
/// Вызывается WDF из таймера на пассивном уровне.
unsafe extern "C" fn evt_telemetry_timer(_timer: WDFTIMER) {
    // SAFETY: доступ сериализован WDF (автоматическая сериализация таймера).
    let st = unsafe { state() };
    let Some(pump) = st.pump.as_mut() else {
        return;
    };

    let status = match pump.status() {
        Ok(status) => status,
        Err(err) => {
            st.last_error = -10;
            println!("ln8000-kmdf: статус недоступен: {err}");
            return;
        }
    };

    let vbat = pump.read_adc(AdcChannel::Vbat).unwrap_or_default();
    let vbus = pump.read_adc(AdcChannel::Vin).unwrap_or_default();
    let iin = pump.read_adc(AdcChannel::Iin).unwrap_or_default();
    let temp = pump.read_adc(AdcChannel::DieTemp).unwrap_or_default();

    let sample = TelemetrySample {
        ts_ms: monotonic_ms(),
        vbat_uv: u32::try_from(vbat.max(0)).unwrap_or(0),
        vbus_uv: u32::try_from(vbus.max(0)).unwrap_or(0),
        iin_ua: u32::try_from(iin.max(0)).unwrap_or(0),
        die_temp_dc: temp,
        op_mode: status.op_mode,
        input_present: !status.has_critical_fault() && vbus > 0,
    };
    st.telemetry.push(sample);

    // Защита по температуре и току: решение принимается по последнему отсчёту.
    let action = evaluate(&sample, &st.limits);
    if action.is_change() {
        apply_guard(pump, action);
        st.actions = st.actions.saturating_add(1);
        println!(
            "ln8000-kmdf: защита {} ({}) при {temp} dC и {iin} uA",
            action.label(),
            match action {
                GuardAction::ReduceCurrent { to_ua, .. } => to_ua,
                _ => 0,
            }
        );
    }

    // Сторож чипа: если включён в профиле, его надо обслуживать чаще периода,
    // иначе чип сам прекратит заряд. Когда сторож выключен, вызов ничего не делает.
    if let Err(err) = pump.service_watchdog() {
        println!("ln8000-kmdf: не удалось обслужить сторожевой таймер: {err}");
    }
}

/// Применяет решение защиты к устройству.
fn apply_guard(pump: &mut Pump<SpbBus>, action: GuardAction) {
    match action {
        GuardAction::None => {}
        GuardAction::ReduceCurrent { to_ua, .. } => {
            if let Err(err) = pump.set_iin_limit(to_ua) {
                println!("ln8000-kmdf: не удалось снизить ток: {err}");
            }
        }
        GuardAction::FallbackToBypass { .. } => {
            if let Err(err) = pump.enable_bypass() {
                println!("ln8000-kmdf: не удалось уйти в bypass: {err}");
            }
        }
        GuardAction::Stop { .. } => {
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
    match io_control_code {
        ioctl::IOCTL_LN8000_GET_STATUS => unsafe { handle_get_status(request) },
        ioctl::IOCTL_LN8000_READ_REG => unsafe { handle_read_reg(request, input_buffer_length) },
        ioctl::IOCTL_LN8000_WRITE_REG => unsafe { handle_write_reg(request, input_buffer_length) },
        ioctl::IOCTL_LN8000_SET_LIMITS => unsafe { handle_set_limits(request, input_buffer_length) },
        ioctl::IOCTL_LN8000_SET_MODE => unsafe { handle_set_mode(request, input_buffer_length) },
        ioctl::IOCTL_LN8000_GET_SESSIONS => unsafe { handle_get_sessions(request) },
        ioctl::IOCTL_LN8000_GET_SAMPLES => unsafe {
            handle_get_samples(request, input_buffer_length)
        },
        _ => unsafe {
            complete(request, wdk_sys::STATUS_INVALID_DEVICE_REQUEST, 0);
        },
    }
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
    if let Some(sample) = st.telemetry.last_sample() {
        status.iin_ua = sample.iin_ua;
        status.vbat_uv = sample.vbat_uv;
        status.vbus_uv = sample.vbus_uv;
        status.die_temp_dc = sample.die_temp_dc;
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

/// Отдаёт сведения о сеансах заряда.
///
/// # Safety
///
/// `request` валиден; выходной буфер достаточного размера.
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

/// Ищет ресурс подключения I²C в переведённом списке `_CRS`.
///
/// Возвращает идентификатор подключения (Resource Hub).
///
/// # Safety
///
/// `resources` — валидный список ресурсов WDF.
unsafe fn find_peripheral_id(resources: WDFCMRESLIST) -> Option<u64> {
    let mut index: ULONG = 0;
    loop {
        // SAFETY: индекс увеличивается до момента, когда дескриптора нет.
        let descriptor = unsafe {
            call_unsafe_wdf_function_binding!(WdfCmResourceListGetDescriptor, resources, index)
        };
        if descriptor.is_null() {
            return None;
        }
        // SAFETY: дескриптор валиден до следующего вызова.
        let kind = unsafe { (*descriptor).Type };
        if u32::from(kind) == CmResourceTypeConnection {
            // SAFETY: для ресурса подключения поле Connection заполнено.
            let low = unsafe { (*descriptor).u.Connection.IdLowPart };
            let high = unsafe { (*descriptor).u.Connection.IdHighPart };
            return Some((u64::from(high) << 32) | u64::from(low));
        }
        index = index.saturating_add(1);
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
        let _ = call_unsafe_wdf_function_binding!(
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
        // Перечисление помечено `non_exhaustive`: новые варианты дадут -100.
        _ => -100,
    }
}

fn size_of_ulong<T>() -> ULONG {
    u32::try_from(core::mem::size_of::<T>()).unwrap_or(0)
}
