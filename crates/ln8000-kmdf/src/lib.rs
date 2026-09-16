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
    Ln8000LimitsRequest, Ln8000ModeRequest, Ln8000RegRequest, Ln8000SamplesRequest, Ln8000Sessions,
    Ln8000Status, LN8000_STATUS_MAGIC, LN8000_STATUS_VERSION,
};
use ln8000::{
    AdcChannel, GuardAction, GuardLimits, Pump, PumpConfig, PumpError, PumpState, Telemetry,
    TelemetrySample, evaluate,
};
use spb::SpbBus;
use wdk::println;
use wdk_sys::{
    _WDF_EXECUTION_LEVEL::WdfExecutionLevelPassive,
    _WDF_IO_QUEUE_DISPATCH_TYPE::WdfIoQueueDispatchSequential,
    _WDF_SYNCHRONIZATION_SCOPE::WdfSynchronizationScopeNone, _WDF_TRI_STATE::WdfTrue,
    call_unsafe_wdf_function_binding, CmResourceTypeConnection, NTSTATUS, PCUNICODE_STRING,
    PWDFDEVICE_INIT, ULONG, UNICODE_STRING, WDFCMRESLIST, WDFDEVICE, WDFDRIVER, WDFQUEUE, WDFREQUEST,
    WDFTIMER, WDF_DRIVER_CONFIG, WDF_IO_QUEUE_CONFIG, WDF_NO_HANDLE, WDF_NO_OBJECT_ATTRIBUTES,
    WDF_OBJECT_ATTRIBUTES, WDF_PNPPOWER_EVENT_CALLBACKS, WDF_TIMER_CONFIG,
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
const SYMLINK_CHARS: usize = 20;
const SYMLINK: [u16; SYMLINK_CHARS] = utf16_lit("/DosDevices/nabu_ln800");

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
    }
    status
}

/// Создаёт устройство, интерфейс и очередь управляющих запросов.
///
/// # Safety
///
/// Вызывается WDF на пассивном уровне.
unsafe extern "C" fn evt_device_add(
    _driver: WDFDRIVER,
    mut device_init: PWDFDEVICE_INIT,
) -> NTSTATUS {
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

    let mut attributes = WDF_OBJECT_ATTRIBUTES {
        Size: size_of_ulong::<WDF_OBJECT_ATTRIBUTES>(),
        ..unsafe { core::mem::zeroed() }
    };
    attributes.Size = size_of_ulong::<WDF_OBJECT_ATTRIBUTES>();

    let mut device: WDFDEVICE = WDF_NO_HANDLE.cast();
    // SAFETY: атрибуты заполнены, `device_init` валиден.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceCreate,
            &raw mut device_init,
            &raw mut attributes,
            &raw mut device,
        )
    };
    if status < 0 {
        println!("ln8000-kmdf: WdfDeviceCreate не удался: {status:#010X}");
        return status;
    }

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
        return status;
    }

    // Символическая ссылка: диагностической утилите достаточно открыть
    // \\.\nabu_ln8000 — без перечисления интерфейсов через SetupAPI.
    let mut link = UNICODE_STRING {
        Length: u16::try_from(SYMLINK_CHARS.saturating_mul(2)).unwrap_or(0),
        MaximumLength: u16::try_from(SYMLINK_CHARS.saturating_mul(2)).unwrap_or(0),
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
        println!("ln8000-kmdf: символическая ссылка не создана: {status:#010X}");
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
        ..unsafe { core::mem::zeroed() }
    };
    timer_attributes.Size = size_of_ulong::<WDF_OBJECT_ATTRIBUTES>();
    timer_attributes.ExecutionLevel = WdfExecutionLevelPassive;
    timer_attributes.SynchronizationScope = WdfSynchronizationScopeNone;

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
        println!("ln8000-kmdf: WdfTimerCreate не удался: {status:#010X}");
        return status;
    }

    // Запоминаем таймер в статике: его запускает prepare_hardware, когда шина готова.
    // SAFETY: единственный экземпляр устройства, доступ сериализован.
    unsafe {
        TIMER = timer;
    }

    println!("ln8000-kmdf: устройство готово");
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
    let peripheral_id = match unsafe { find_peripheral_id(resources_translated) } {
        Some(id) => id,
        None => {
            println!("ln8000-kmdf: в _CRS нет I²C-подключения (узел PEIC не найден)");
            return wdk_sys::STATUS_DEVICE_NOT_READY;
        }
    };
    println!("ln8000-kmdf: подключение {peripheral_id:#018X}");
    // SAFETY: пассивный уровень, устройство создано.
    let bus = match unsafe { SpbBus::open(device, peripheral_id) } {
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
            return wdk_sys::STATUS_DEVICE_NOT_READY;
        }
    };

    // 3. Опознаём чип и настраиваем его.
    let config = PumpConfig::for_qc35_class_b();
    let mut pump = match Pump::open(bus, config) {
        Ok(pump) => pump,
        Err(err) => {
            println!("ln8000-kmdf: LN8000 не опознан: {err}");
            let st = unsafe { state() };
            st.last_error = -2;
            return wdk_sys::STATUS_DEVICE_NOT_READY;
        }
    };
    if let Err(err) = pump.configure() {
        println!("ln8000-kmdf: конфигурация не удалась: {err}");
        let st = unsafe { state() };
        st.last_error = -3;
        return wdk_sys::STATUS_DEVICE_NOT_READY;
    }

    // 4. Пробуем включить ускоренный режим. Если чип его не подтвердил —
    //    работаем дальше в bypass: зарядка должна быть безопасной и рабочей.
    match pump.enable_switching() {
        Ok(mode) => println!("ln8000-kmdf: режим {}", mode.label()),
        Err(err) => {
            println!("ln8000-kmdf: режим 2:1 не включился ({err}); остаёмся в bypass");
            if let Err(fallback) = pump.enable_bypass() {
                println!("ln8000-kmdf: bypass тоже не включился: {fallback}");
            }
        }
    }

    let st = unsafe { state() };
    st.pump = Some(pump);

    // 5. Запускаем телеметрию.
    // SAFETY: таймер создан в `evt_device_add`.
    let timer = unsafe { TIMER };
    if !timer.is_null() {
        unsafe {
            let _ = call_unsafe_wdf_function_binding!(WdfTimerStart, timer, -10_000_i64 * 1_000);
        }
    }

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
    // SAFETY: доступ сериализован WDF; адрес берётся из входного буфера клиента.
    let st = unsafe { state() };
    let mut answer = Ln8000RegRequest::default();
    match st.pump.as_mut() {
        Some(pump) => match pump.read_register(0) {
            Ok(value) => answer.value = value,
            Err(err) => answer.error_code = pump_error_code(err),
        },
        None => answer.error_code = -1,
    }
    if let Some(pump) = st.pump.as_ref() {
        let _ = pump;
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
    // TODO(bring-up): прочитать адрес и значение из входного буфера и записать
    // через `Pump::write_register`. Пока отвечаем честным отказом.
    unsafe { complete(request, wdk_sys::STATUS_NOT_IMPLEMENTED, 0) };
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
    // TODO(bring-up): применить лимиты из запроса через ядро LN8000.
    unsafe { complete(request, wdk_sys::STATUS_NOT_IMPLEMENTED, 0) };
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
    unsafe { complete(request, wdk_sys::STATUS_NOT_IMPLEMENTED, 0) };
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
    // TODO(bring-up): скопировать кольцо отсчётов в выходной буфер клиента.
    unsafe { complete(request, wdk_sys::STATUS_NOT_IMPLEMENTED, 0) };
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

/// Копирует структуру в выходной буфер запроса и завершает его.
///
/// # Safety
///
/// `request` валиден; `value` указывает на живую структуру.
unsafe fn write_output<T: Copy>(request: WDFREQUEST, value: &T) {
    let mut buffer: *mut core::ffi::c_void = core::ptr::null_mut();
    let mut length: usize = 0;
    // SAFETY: выходной буфер запроса создаётся фреймворком (METHOD_BUFFERED).
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfRequestRetrieveOutputBuffer,
            request,
            core::mem::size_of::<T>(),
            &raw mut buffer,
            &raw mut length,
        )
    };
    if status < 0 {
        unsafe { complete(request, status, 0) };
        return;
    }
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
