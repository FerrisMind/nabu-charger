//! Транспорт к регистрам PMIC через шину SPMI: устройство `\Device\RESOURCE_HUB`.
//!
//! Штатные драйверы Qualcomm (`qcpmictcc8150.sys`, `qcpmicEIC8150.sys`,
//! `qcbattmngr8150.sys`) обращаются к периферии PMIC именно так: открывают
//! устройство шины и посылают ему управляющие запросы. Здесь повторяется тот же
//! путь, но из нашего драйвера.
//!
//! # Что уже известно о контракте
//!
//! Реверс `qcspmi8150.sys` (см. `G:\nabu-tools\re-spmi-ioctl.md`) показал, что
//! клиенты общаются с хабом кодом **`0x32C004`**, который раскладывается как
//! `CTL_CODE(0x32, 1, METHOD_BUFFERED, FILE_READ | FILE_WRITE)`:
//!
//! ```text
//! 0x32C004 = 0x32 << 16 | 0x3 << 14 | 0x1 << 2 | 0x0
//!            тип          доступ      функция     метод
//! ```
//!
//! Полная структура входного/выходного буфера для чтения и записи регистра
//! доразбирается отдельной задачей; до этого момента коды и размеры буферов —
//! параметры модуля [`SpmiConfig`], а не зашитые константы. Ничего не выдумано:
//! то, что не подтверждено реверсом, помечено `TODO(RE)`.

use charger_core::{ChargerTransport, RegAddr, TransportError};
use wdk_sys::{
    call_unsafe_wdf_function_binding, NTSTATUS, PVOID, ULONG, WDFDEVICE, WDFIOTARGET, WDFMEMORY,
    WDFOBJECT, WDFREQUEST, WDF_IO_TARGET_OPEN_PARAMS, WDF_NO_HANDLE, WDF_NO_OBJECT_ATTRIBUTES,
    WDF_REQUEST_SEND_OPTION_TIMEOUT, WDF_REQUEST_SEND_OPTIONS, WDF_REQUEST_SEND_OPTIONS_INIT,
    WDF_TRI_STATE,
};

/// Код управления, которым клиенты обращаются к шине SPMI.
///
/// Подтверждён реверсом `qcspmi8150.sys` (см. отчёт `re-spmi-ioctl.md`).
pub const IOCTL_RESOURCE_HUB_TRANSACT: u32 = 0x0032_C004;

/// Тип устройства шины SPMI (старшие 16 бит кода `0x32C004`).
pub const FILE_DEVICE_RESOURCE_HUB: u32 = 0x32;

/// Настройки доступа к шине.
#[derive(Debug, Clone, Copy)]
pub struct SpmiConfig {
    /// Имя устройства шины.
    pub device_name: &'static str,
    /// Код управления для транзакции.
    pub ioctl: u32,
    /// Сколько байт занимает заголовок запроса (адрес регистра и признак операции).
    ///
    /// `TODO(RE)`: уточнить по разбору обработчика `IRP_MJ_DEVICE_CONTROL`
    /// в `qcspmi8150.sys`. Пока используется минимальный вариант «адрес + значение».
    pub request_header_len: u32,
    /// Таймаут одной транзакции в миллисекундах.
    pub timeout_ms: ULONG,
}

impl SpmiConfig {
    /// Конфигурация по умолчанию для планшета `nabu`.
    #[must_use]
    pub const fn nabu() -> Self {
        Self {
            device_name: "\\Device\\RESOURCE_HUB",
            ioctl: IOCTL_RESOURCE_HUB_TRANSACT,
            request_header_len: 3,
            timeout_ms: 1_000,
        }
    }
}

/// Транспорт к регистрам PMIC поверх шины SPMI.
///
/// Каждая операция — синхронный управляющий запрос к открытому устройству шины.
/// Объекты WDF создаются один раз в [`SpmiTransport::open`] и живут до выгрузки
/// драйвера, поэтому повторные чтения и записи не создают новых объектов.
#[derive(Debug)]
pub struct SpmiTransport {
    target: WDFIOTARGET,
    request: WDFREQUEST,
    input: WDFMEMORY,
    output: WDFMEMORY,
    config: SpmiConfig,
}

impl SpmiTransport {
    /// Открывает устройство шины и готовит буферы обмена.
    ///
    /// # Errors
    ///
    /// [`TransportError`] категории `Io`, если устройство шины недоступно, и
    /// `Unsupported`, если WDF не дал создать объекты запроса.
    ///
    /// # Safety
    ///
    /// Вызывается из `EvtDeviceAdd` при пассивном уровне IRQL: WDF требует
    /// пассивного уровня для создания объектов и открытия цели ввода-вывода.
    pub unsafe fn open(device: WDFDEVICE, config: SpmiConfig) -> Result<Self, TransportError> {
        let mut target: WDFIOTARGET = WDF_NO_HANDLE.cast();
        // SAFETY: `device` — валидный дескриптор WDFDEVICE, полученный от WDF;
        // `target` — локальная переменная под выходной дескриптор.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfIoTargetCreate,
                device,
                WDF_NO_OBJECT_ATTRIBUTES,
                &raw mut target,
            )
        };
        if !nt_ok(status) {
            return Err(TransportError::io("не удалось создать цель ввода-вывода"));
        }

        let mut params = WDF_IO_TARGET_OPEN_PARAMS::default();
        // SAFETY: структура параметров инициализируется макросом WDF и затем
        // дополняется именем устройства; буферы строк живут в статической памяти.
        unsafe {
            wdf_sys::WDF_IO_TARGET_OPEN_PARAMS_INIT_OPEN_BY_NAME(
                &raw mut params,
                wdf_sys::WDF_NO_HANDLE.cast(),
                FILE_DEVICE_RESOURCE_HUB,
            );
        }
        params.TargetDeviceName = wdf_object_name(config.device_name);

        // SAFETY: `target` создан выше, параметры заполнены; вызов идёт с пассивного уровня.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(WdfIoTargetOpen, target, &raw mut params)
        };
        if !nt_ok(status) {
            return Err(TransportError::io("устройство \\Device\\RESOURCE_HUB недоступно"));
        }

        let mut request: WDFREQUEST = WDF_NO_HANDLE.cast();
        let mut input: WDFMEMORY = WDF_NO_HANDLE.cast();
        let mut output: WDFMEMORY = WDF_NO_HANDLE.cast();
        // SAFETY: дескрипторы — локальные переменные под выходные значения.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfRequestCreate,
                WDF_NO_OBJECT_ATTRIBUTES,
                target,
                &raw mut request,
            )
        };
        if !nt_ok(status) {
            return Err(TransportError::unsupported("не удалось создать WDFREQUEST"));
        }
        // SAFETY: `request` создан выше; память выделяется на время жизни запроса.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfMemoryCreate,
                WDF_NO_OBJECT_ATTRIBUTES,
                wdk_sys::POOL_TYPE::NonPagedPool,
                0,
                ulong(64),
                &raw mut input,
                core::ptr::null_mut(),
            )
        };
        if !nt_ok(status) {
            return Err(TransportError::unsupported("не удалось выделить входной буфер"));
        }
        // SAFETY: аналогично входному буферу.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfMemoryCreate,
                WDF_NO_OBJECT_ATTRIBUTES,
                wdk_sys::POOL_TYPE::NonPagedPool,
                0,
                ulong(64),
                &raw mut output,
                core::ptr::null_mut(),
            )
        };
        if !nt_ok(status) {
            return Err(TransportError::unsupported("не удалось выделить выходной буфер"));
        }

        Ok(Self {
            target,
            request,
            input,
            output,
            config,
        })
    }

    /// Выполняет одну транзакцию по шине.
    ///
    /// `write` = `true` означает запись регистра, `false` — чтение.
    fn transact(&mut self, addr: RegAddr, value: u8, write: bool) -> Result<u8, TransportError> {
        let mut payload = [0_u8; 8];
        payload[0] = u8::from(write);
        payload[1] = (addr & 0x00FF) as u8;
        payload[2] = (addr >> 8) as u8;
        payload[3] = value;

        // SAFETY: буферы созданы в `open` с достаточным размером (64 байта),
        // копируем 4 байта; дескрипторы запроса и памяти валидны.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfMemoryCopyFromBuffer,
                self.input,
                0,
                payload.as_ptr().cast::<core::ffi::c_void>(),
                ULONG::from(self.config.request_header_len + 1),
            )
        };
        if !nt_ok(status) {
            return Err(TransportError::io("не удалось заполнить запрос шины"));
        }

        // SAFETY: запрос, память и цель валидны; формат запроса IOCTL.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfIoTargetFormatRequestForIoctl,
                self.target,
                self.request,
                self.config.ioctl,
                self.input,
                core::ptr::null_mut(),
                self.output,
                core::ptr::null_mut(),
            )
        };
        if !nt_ok(status) {
            return Err(TransportError::protocol("шина отклонила формат запроса"));
        }

        let mut options = WDF_REQUEST_SEND_OPTIONS::default();
        // SAFETY: инициализация структуры параметров отправки макросом WDF.
        unsafe {
            WDF_REQUEST_SEND_OPTIONS_INIT(
                &raw mut options,
                WDF_REQUEST_SEND_OPTION_TIMEOUT as ULONG,
            );
            options.Timeout = self.config.timeout_ms;
        }
        // SAFETY: запрос отправляется синхронно (в ожидании результата);
        // уровень IRQL пассивный, повторный вход исключён последовательной очередью.
        let sent = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfRequestSend,
                self.request,
                self.target,
                &raw mut options,
            )
        };
        if sent == 0 {
            return Err(TransportError::timeout("шина не ответила за отведённое время"));
        }

        // SAFETY: запрос завершён синхронно; читаем его состояние.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(WdfRequestGetStatus, self.request)
        };
        if !nt_ok(status) {
            return Err(TransportError::io("шина вернула отказ на транзакцию"));
        }

        if write {
            return Ok(());
        }
        Err(TransportError::unsupported(
            "чтение ещё не подтверждено реверсом (TODO(RE))",
        ))
    }
}

impl ChargerTransport for SpmiTransport {
    fn read(&mut self, addr: RegAddr) -> Result<u8, TransportError> {
        self.transact(addr, 0, false)
    }

    fn write(&mut self, addr: RegAddr, value: u8) -> Result<(), TransportError> {
        self.transact(addr, value, true).map(|_| ())
    }

    fn reset(&mut self) -> Result<(), TransportError> {
        // Переоткрытие цели: закрываем и открываем заново то же устройство.
        // SAFETY: дескриптор цели валиден до выгрузки драйвера.
        let status = unsafe { call_unsafe_wdf_function_binding!(WdfIoTargetClose, self.target) };
        if !nt_ok(status) {
            return Err(TransportError::disconnected("не удалось закрыть шину"));
        }
        Err(TransportError::unsupported(
            "переоткрытие шины выполняется в EvtDeviceAdd (TODO)",
        ))
    }

    fn name(&self) -> &'static str {
        "spmi"
    }
}

fn nt_ok(status: NTSTATUS) -> bool {
    status >= 0
}

fn ulong(value: usize) -> ULONG {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// Формирует `UNICODE_STRING` из статической строки в кодировке ASCII.
///
/// # Safety
///
/// Возвращаемая структура указывает на статические данные с временем жизни
/// программы; вызывающая сторона не должна изменять буфер.
unsafe fn wdf_object_name(name: &'static str) -> wdk_sys::UNICODE_STRING {
    // WDF ожидает длину в байтах без завершающего нуля.
    let bytes = name.as_bytes();
    let len = u16::try_from(bytes.len().saturating_mul(2)).unwrap_or(0);
    wdk_sys::UNICODE_STRING {
        Length: len,
        MaximumLength: len,
        Buffer: name.as_ptr().cast_mut().cast(),
    }
}

/// Заглушка, чтобы модуль компилировался до реверса структур обмена.
///
/// # Safety
///
/// Указатель используется только при вызове `WdfRequestRetrieveOutputBuffer`.
pub unsafe fn unused(_: PVOID, _: WDFOBJECT, _: PVOID) {}

#[allow(dead_code)]
const _WDF_TRI_STATE_MARKER: WDF_TRI_STATE = WDF_TRI_STATE::WdfUseDefault;
