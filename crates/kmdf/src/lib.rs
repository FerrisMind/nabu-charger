//! Драйвер режима ядра (KMDF) для зарядки Xiaomi Pad 5 (`nabu`), Windows on ARM64.
//!
//! # Что делает драйвер
//!
//! Ядро логики живёт в крейте `charger-core` (без зависимостей от ОС): оно
//! читает результат аппаратной детекции адаптера (APSD) и выставляет лимит
//! входного тока. Здесь — обвязка WDF: устройство, очередь управляющих
//! запросов, таймер неблокирующей детекции и транспорт к шине SPMI.
//!
//! # Почему ARM64
//!
//! Целевое устройство — планшет под Snapdragon 860. Драйвер собирается
//! **только** под `aarch64-pc-windows-msvc` (см. `.cargo/config.toml` рядом);
//! архитектура сборочной машины к драйверу отношения не имеет.
//!
//! # Состояние проверки
//!
//! Логика ядра покрыта автотестами и собирается на хосте. Эта обвязка
//! собирается `cargo wdk build` под ARM64; проверка на планшете — см.
//! `docs/HANDOVER.md`.

#![no_std]
#![deny(missing_docs)]
#![allow(clippy::missing_safety_doc)]

mod ioctl;
mod spmi;

extern crate wdk_panic;

use core::cell::RefCell;
use ioctl::{
    NabuIclRequest, NabuJournalRequest, NabuRegRequest, NabuState, NabuStatus, NABU_CAPABILITIES,
    NABU_STATUS_MAGIC, NABU_STATUS_VERSION,
};
use charger_core::{Charger, ChargerConfig, Clock, Event, Journal};
use spmi::{SpmiConfig, SpmiTransport};
use wdk::println;
use wdk_sys::{
    _WDF_DRIVER_INIT_FLAGS::WdfDriverInitNonPnpDriver,
    _WDF_IO_QUEUE_DISPATCH_TYPE::WdfIoQueueDispatchSequential,
    _WDF_TRI_STATE::WdfTrue,
    call_unsafe_wdf_function_binding, NTSTATUS, PCUNICODE_STRING, PWDFDEVICE_INIT, ULONG, WDFDEVICE,
    WDFDRIVER, WDFQUEUE, WDFREQUEST, WDF_DRIVER_CONFIG, WDF_IO_QUEUE_CONFIG, WDF_NO_HANDLE,
    WDF_NO_OBJECT_ATTRIBUTES, WDF_OBJECT_ATTRIBUTES,
};

/// Интерфейс устройства: по этому GUID клиент находит драйвер.
///
/// `{7C1A5B3E-9D42-4C6B-A1E7-2F4B8C3D5A90}`
pub const GUID_DEVINTERFACE_NABU_CHARGER: wdk_sys::GUID = wdk_sys::GUID {
    Data1: 0x7C1A_5B3E,
    Data2: 0x9D42,
    Data3: 0x4C6B,
    Data4: [0xA1, 0xE7, 0x2F, 0x4B, 0x8C, 0x3D, 0x5A, 0x90],
};

/// Размер кольцевого журнала операций.
const JOURNAL_CAPACITY: usize = 256;

/// Часы ядра: монотонные миллисекунды от счётчика производительности WDF.
#[derive(Debug, Default)]
pub struct KernelClock;

impl Clock for KernelClock {
    fn now_ms(&self) -> u64 {
        let mut stamp: u64 = 0;
        // SAFETY: `KeQueryInterruptTimePrecise` — документированная ядерная функция;
        // ей передаётся указатель на локальную переменную под метку времени QPC.
        let ticks = unsafe { wdk_sys::ntddk::KeQueryInterruptTimePrecise(&raw mut stamp) };
        // Единица измерения — 100 нс, значит миллисекунда — это 10 000 интервалов.
        ticks / 10_000
    }
}

/// Кольцевой журнал операций: фиксированный буфер без аллокаций.
///
/// # Инварианты
///
/// Запись идёт только из последовательной очереди устройства и её таймера,
/// то есть вызовы сериализованы WDF; одновременного доступа из нескольких
/// потоков не бывает. Поэтому `Sync` реализуется вручную.
#[derive(Debug)]
struct RingJournal {
    entries: RefCell<[Option<Event>; JOURNAL_CAPACITY]>,
    next: RefCell<usize>,
    total: RefCell<u64>,
}

// SAFETY: см. инварианты выше — доступ сериализован WDF.
unsafe impl Sync for RingJournal {}

impl RingJournal {
    const fn new() -> Self {
        Self {
            entries: RefCell::new([None; JOURNAL_CAPACITY]),
            next: RefCell::new(0),
            total: RefCell::new(0),
        }
    }

    /// Сколько записей всего прошло через журнал.
    fn total(&self) -> u64 {
        *self.total.borrow()
    }
}

impl Journal for RingJournal {
    fn event(&self, event: &Event) {
        let index = *self.next.borrow();
        if let Some(slot) = self.entries.borrow_mut().get_mut(index) {
            *slot = Some(*event);
        }
        *self.next.borrow_mut() = (index.saturating_add(1)) % JOURNAL_CAPACITY;
        *self.total.borrow_mut() = self.total.borrow().saturating_add(1);
    }
}

/// Глобальный журнал и часы: живут всё время работы драйвера.
static JOURNAL: RingJournal = RingJournal::new();
static CLOCK: KernelClock = KernelClock;

fn static_clock() -> &'static KernelClock {
    &CLOCK
}

fn static_journal() -> &'static RingJournal {
    &JOURNAL
}

/// Контекст устройства: сессия драйвера зарядника.
struct DeviceContext {
    charger: Charger<'static, SpmiTransport, KernelClock, RingJournal>,
}

/// Точка входа драйвера.
///
/// # Safety
///
/// Вызывается ядром; `driver` и `registry_path` валидны по контракту WDF.
#[unsafe(no_mangle)]
#[unsafe(link_section = "INIT")]
pub unsafe extern "system" fn DriverEntry(
    driver: &mut wdk_sys::DRIVER_OBJECT,
    registry_path: PCUNICODE_STRING,
) -> NTSTATUS {
    println!("nabu-charger: DriverEntry");

    let mut config = WDF_DRIVER_CONFIG {
        Size: size_of_ulong::<WDF_DRIVER_CONFIG>(),
        EvtDriverDeviceAdd: Some(evt_device_add),
        EvtDriverUnload: None,
        DriverInitFlags: WdfDriverInitNonPnpDriver as ULONG,
        DriverPoolTag: 0x5542_414E, // 'NABU'
    };

    let mut driver_handle: WDFDRIVER = WDF_NO_HANDLE.cast();
    // SAFETY: конфигурация заполнена полностью, дескриптор — локальная переменная.
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
        println!("nabu-charger: WdfDriverCreate не удался: {status:#010X}");
    }
    status
}

/// Создаёт устройство, очередь запросов и открывает шину SPMI.
///
/// # Safety
///
/// Вызывается WDF на пассивном уровне; дескрипторы валидны.
unsafe extern "C" fn evt_device_add(
    _driver: WDFDRIVER,
    mut device_init: PWDFDEVICE_INIT,
) -> NTSTATUS {
    let mut attributes = WDF_OBJECT_ATTRIBUTES {
        Size: size_of_ulong::<WDF_OBJECT_ATTRIBUTES>(),
        ..unsafe { core::mem::zeroed() }
    };
    attributes.Size = size_of_ulong::<WDF_OBJECT_ATTRIBUTES>();

    let mut device: WDFDEVICE = WDF_NO_HANDLE.cast();
    // SAFETY: `device_init` предоставлен WDF; атрибуты заполнены.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceCreate,
            &raw mut device_init,
            &raw mut attributes,
            &raw mut device,
        )
    };
    if status < 0 {
        println!("nabu-charger: WdfDeviceCreate не удался: {status:#010X}");
        return status;
    }

    // SAFETY: пассивный уровень, устройство создано выше.
    let transport = match unsafe { SpmiTransport::open(device, SpmiConfig::nabu()) } {
        Ok(transport) => transport,
        Err(err) => {
            println!("nabu-charger: шина SPMI недоступна: {err}");
            return wdk_sys::STATUS_DEVICE_NOT_READY;
        }
    };

    if Charger::open(
        transport,
        static_clock(),
        static_journal(),
        ChargerConfig::for_nabu(),
    )
    .is_err()
    {
        println!("nabu-charger: сессия драйвера не открылась");
        return wdk_sys::STATUS_DEVICE_NOT_READY;
    }

    // Очередь управляющих запросов: последовательная, управляется питанием.
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
        println!("nabu-charger: WdfIoQueueCreate не удался: {status:#010X}");
        return status;
    }

    println!("nabu-charger: устройство готово, журнал на {} записей", JOURNAL.total());
    wdk_sys::STATUS_SUCCESS
}

/// Обрабатывает управляющие запросы пользовательского режима.
///
/// # Safety
///
/// Вызывается WDF; `request` валиден, размеры буферов проверяются.
unsafe extern "C" fn evt_io_device_control(
    _queue: WDFQUEUE,
    request: WDFREQUEST,
    _output_buffer_length: usize,
    input_buffer_length: usize,
    io_control_code: ULONG,
) {
    match io_control_code {
        ioctl::IOCTL_NABU_GET_STATUS => unsafe { handle_get_status(request) },
        ioctl::IOCTL_NABU_READ_REG => unsafe { handle_read_reg(request, input_buffer_length) },
        ioctl::IOCTL_NABU_WRITE_REG => unsafe { handle_write_reg(request, input_buffer_length) },
        ioctl::IOCTL_NABU_SET_ICL => unsafe { handle_set_icl(request, input_buffer_length) },
        ioctl::IOCTL_NABU_GET_JOURNAL => unsafe {
            handle_get_journal(request, input_buffer_length)
        },
        // Детекция и применение политики выполняются таймером (bring-up).
        ioctl::IOCTL_NABU_DETECT_START | ioctl::IOCTL_NABU_APPLY_POLICY => unsafe {
            complete(request, wdk_sys::STATUS_NOT_IMPLEMENTED, 0);
        },
        _ => unsafe {
            complete(request, wdk_sys::STATUS_INVALID_DEVICE_REQUEST, 0);
        },
    }
}

/// Отдаёт состояние драйвера клиенту.
///
/// # Safety
///
/// `request` валиден; выходной буфер достаточного размера.
unsafe fn handle_get_status(request: WDFREQUEST) {
    let status = NabuStatus {
        magic: NABU_STATUS_MAGIC,
        version: NABU_STATUS_VERSION,
        capabilities: NABU_CAPABILITIES,
        state: NabuState::Closed as u8,
        adapter_code: 0xFF,
        ..NabuStatus::default()
    };
    unsafe { write_output(request, &status) };
}

/// Читает регистр периферии (диагностика).
///
/// # Safety
///
/// `request` валиден; входной буфер содержит [`NabuRegRequest`].
unsafe fn handle_read_reg(request: WDFREQUEST, input_length: usize) {
    if input_length < core::mem::size_of::<NabuRegRequest>() {
        unsafe { complete(request, wdk_sys::STATUS_BUFFER_TOO_SMALL, 0) };
        return;
    }
    let answer = NabuRegRequest {
        error_code: wdk_sys::STATUS_NOT_IMPLEMENTED,
        ..NabuRegRequest::default()
    };
    unsafe { write_output(request, &answer) };
}

/// Записывает регистр периферии (диагностика).
///
/// # Safety
///
/// `request` валиден; входной буфер содержит [`NabuRegRequest`].
unsafe fn handle_write_reg(request: WDFREQUEST, input_length: usize) {
    if input_length < core::mem::size_of::<NabuRegRequest>() {
        unsafe { complete(request, wdk_sys::STATUS_BUFFER_TOO_SMALL, 0) };
        return;
    }
    unsafe { complete(request, wdk_sys::STATUS_NOT_IMPLEMENTED, 0) };
}

/// Принудительно задаёт лимит входного тока.
///
/// # Safety
///
/// `request` валиден; входной буфер содержит [`NabuIclRequest`].
unsafe fn handle_set_icl(request: WDFREQUEST, input_length: usize) {
    if input_length < core::mem::size_of::<NabuIclRequest>() {
        unsafe { complete(request, wdk_sys::STATUS_BUFFER_TOO_SMALL, 0) };
        return;
    }
    unsafe { complete(request, wdk_sys::STATUS_NOT_IMPLEMENTED, 0) };
}

/// Отдаёт снимок журнала операций.
///
/// # Safety
///
/// `request` валиден; выходной буфер заполняется поэлементно.
unsafe fn handle_get_journal(request: WDFREQUEST, input_length: usize) {
    if input_length < core::mem::size_of::<NabuJournalRequest>() {
        unsafe { complete(request, wdk_sys::STATUS_BUFFER_TOO_SMALL, 0) };
        return;
    }
    unsafe { complete(request, wdk_sys::STATUS_NOT_IMPLEMENTED, 0) };
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

/// Завершает запрос с кодом и объёмом переданных данных.
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

/// Размер структуры в виде `ULONG` для поля `Size`.
fn size_of_ulong<T>() -> ULONG {
    u32::try_from(core::mem::size_of::<T>()).unwrap_or(0)
}
