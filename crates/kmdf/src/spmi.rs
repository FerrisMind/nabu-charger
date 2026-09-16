//! Транспорт к регистрам PMIC через шину SPMI: устройство `\Device\RESOURCE_HUB`.
//!
//! Штатные драйверы Qualcomm (`qcpmictcc8150.sys`, `qcpmicEIC8150.sys`) обращаются
//! к периферии PMIC именно так: открывают устройство шины и посылают ему
//! управляющие запросы. Здесь повторяется тот же путь из нашего драйвера.
//!
//! # Состояние контракта
//!
//! Реверс `qcspmi8150.sys` показал, что клиенты общаются с хабом кодом
//! **`0x32C004`** — это `CTL_CODE(0x32, 1, METHOD_BUFFERED, FILE_READ | FILE_WRITE)`:
//!
//! ```text
//! 0x32C004 = 0x32 << 16 | 0x3 << 14 | 0x1 << 2 | 0x0
//!             тип          доступ     функция    метод
//! ```
//!
//! Раскладка буферов (как передаётся адрес регистра и как возвращается значение)
//! ещё доводится: всё неподтверждённое помечено `TODO(RE)` и вынесено в
//! [`SpmiConfig`], а не зашито в код как факт.
//!
//! # Уровень IRQL
//!
//! Все операции синхронные, пассивного уровня: вызываются из
//! `EvtIoDeviceControl` последовательной очереди и из таймера детекции.

use charger_core::{ChargerTransport, RegAddr, TransportError};
use wdk_sys::{
    _POOL_TYPE::NonPagedPool, _WDF_IO_TARGET_OPEN_TYPE::WdfIoTargetOpenByName,
    _WDF_REQUEST_SEND_OPTIONS_FLAGS::WDF_REQUEST_SEND_OPTION_TIMEOUT,
    call_unsafe_wdf_function_binding, NTSTATUS, ULONG, UNICODE_STRING, WDFDEVICE, WDFIOTARGET,
    WDFMEMORY, WDFREQUEST, WDF_IO_TARGET_OPEN_PARAMS, WDF_REQUEST_REUSE_PARAMS,
    WDF_REQUEST_SEND_OPTIONS, WDF_NO_HANDLE, WDF_NO_OBJECT_ATTRIBUTES,
};

/// Код управления, которым клиенты обращаются к шине SPMI (подтверждён реверсом).
pub const IOCTL_RESOURCE_HUB_TRANSACT: u32 = 0x0032_C004;

/// Тип устройства шины SPMI (старшие 16 бит кода).
#[allow(dead_code)] // Используется при прямом открытии устройства без WDF.
pub const FILE_DEVICE_RESOURCE_HUB: u32 = 0x32;

/// Размер буфера обмена с шиной.
const BUFFER_LEN: usize = 64;

/// Настройки доступа к шине.
#[derive(Debug, Clone, Copy)]
pub struct SpmiConfig {
    /// Код управления для транзакции.
    pub ioctl: u32,
    /// Длина заголовка запроса: признак операции и адрес (2 байта).
    ///
    /// `TODO(RE)`: уточнить по разбору обработчика `IRP_MJ_DEVICE_CONTROL`.
    pub request_header_len: u32,
    /// Таймаут одной транзакции в миллисекундах.
    pub timeout_ms: i64,
}

impl SpmiConfig {
    /// Конфигурация по умолчанию для планшета `nabu`.
    #[must_use]
    pub const fn nabu() -> Self {
        Self {
            ioctl: IOCTL_RESOURCE_HUB_TRANSACT,
            request_header_len: 3,
            timeout_ms: 1_000,
        }
    }
}

/// Имя устройства шины в UTF-16, собранное на этапе компиляции.
const DEVICE_NAME_UTF16: [u16; 20] = utf16_lit("/Device/RESOURCE_HUB");

/// Собирает UTF-16 без завершающего нуля: символы `'/'` заменяются на `'\\'`.
const fn utf16_lit(ascii: &str) -> [u16; 20] {
    let bytes = ascii.as_bytes();
    let mut out = [0_u16; 20];
    let mut index = 0;
    while index < bytes.len() && index < 20 {
        let byte = bytes[index];
        out[index] = if byte == b'/' { b'\\' as u16 } else { byte as u16 };
        index += 1;
    }
    out
}

fn device_name() -> UNICODE_STRING {
    let length = u16::try_from(DEVICE_NAME_UTF16.len().saturating_mul(2)).unwrap_or(0);
    UNICODE_STRING {
        Length: length,
        MaximumLength: length,
        Buffer: DEVICE_NAME_UTF16.as_ptr().cast_mut(),
    }
}

/// Транспорт к регистрам PMIC поверх шины SPMI.
///
/// Объекты WDF (цель ввода-вывода, запрос, буферы) создаются один раз при
/// добавлении устройства и живут до его удаления, поэтому повторные чтения и
/// записи не создают новых объектов и не накапливают ресурсы.
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
    /// * `Io` — устройство шины недоступно или объекты WDF не создались.
    /// * `Unsupported` — WDF не поддержал запрошенный режим.
    ///
    /// # Safety
    ///
    /// Вызывается на пассивном уровне IRQL (из `EvtDeviceAdd`): создание
    /// объектов WDF и открытие цели по имени на повышенном уровне запрещено.
    pub unsafe fn open(device: WDFDEVICE, config: SpmiConfig) -> Result<Self, TransportError> {
        let mut target: WDFIOTARGET = WDF_NO_HANDLE.cast();
        // SAFETY: `device` — валидный WDFDEVICE из EvtDeviceAdd; `target` — локальная переменная.
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

        let mut params: WDF_IO_TARGET_OPEN_PARAMS = unsafe { core::mem::zeroed() };
        params.Size = size_of_ulong::<WDF_IO_TARGET_OPEN_PARAMS>();
        params.Type = WdfIoTargetOpenByName;
        params.TargetDeviceName = device_name();

        // SAFETY: `target` создан выше, параметры заполнены; уровень пассивный.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(WdfIoTargetOpen, target, &raw mut params)
        };
        if !nt_ok(status) {
            return Err(TransportError::io(
                "устройство \\Device\\RESOURCE_HUB недоступно",
            ));
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

        let mut input_buffer: *mut core::ffi::c_void = core::ptr::null_mut();
        // SAFETY: память выделяется в невыгружаемом пуле и живёт до удаления устройства.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfMemoryCreate,
                WDF_NO_OBJECT_ATTRIBUTES,
                NonPagedPool,
                0,
                BUFFER_LEN,
                &raw mut input,
                &raw mut input_buffer,
            )
        };
        if !nt_ok(status) {
            return Err(TransportError::unsupported("не удалось выделить входной буфер"));
        }

        let mut output_buffer: *mut core::ffi::c_void = core::ptr::null_mut();
        // SAFETY: аналогично входному буферу.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfMemoryCreate,
                WDF_NO_OBJECT_ATTRIBUTES,
                NonPagedPool,
                0,
                BUFFER_LEN,
                &raw mut output,
                &raw mut output_buffer,
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

    /// Записывает байт регистра через шину.
    ///
    /// # Errors
    ///
    /// * `Io` — шина отказала.
    /// * `Timeout` — шина не ответила за [`SpmiConfig::timeout_ms`].
    /// * `Protocol` — шина отклонила формат запроса.
    pub fn write_reg(&mut self, addr: RegAddr, value: u8) -> Result<(), TransportError> {
        let mut payload = [0_u8; BUFFER_LEN];
        payload[0] = 1; // признак операции «запись»
        payload[1] = (addr & 0x00FF) as u8;
        payload[2] = (addr >> 8) as u8;
        payload[3] = value;
        let length = usize::try_from(self.config.request_header_len)
            .unwrap_or(4)
            .saturating_add(1)
            .min(4);

        // SAFETY: входной буфер создан размером BUFFER_LEN, копируем не более 4 байт;
        // дескриптор памяти валиден до удаления устройства.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfMemoryCopyFromBuffer,
                self.input,
                0,
                payload.as_ptr().cast_mut().cast::<core::ffi::c_void>(),
                length,
            )
        };
        if !nt_ok(status) {
            return Err(TransportError::io("не удалось заполнить запрос шины"));
        }

        // SAFETY: запрос, память и цель валидны; формат — управляющий запрос IOCTL.
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

        let mut options = WDF_REQUEST_SEND_OPTIONS {
            Size: size_of_ulong::<WDF_REQUEST_SEND_OPTIONS>(),
            Flags: WDF_REQUEST_SEND_OPTION_TIMEOUT as ULONG,
            Timeout: self.config.timeout_ms,
        };
        options.Size = size_of_ulong::<WDF_REQUEST_SEND_OPTIONS>();

        // SAFETY: отправка синхронная, уровень пассивный, повторный вход исключён
        // последовательной очередью устройства.
        let sent = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfRequestSend,
                self.request,
                self.target,
                &raw mut options,
            )
        };
        if sent == 0 {
            // При отказе отправки запрос нужно вернуть в исходное состояние.
            unsafe { reuse_request(self.request) };
            return Err(TransportError::timeout("шина не ответила за отведённое время"));
        }

        // SAFETY: запрос завершён; читаем статус и переиспользуем запрос.
        let status = unsafe { call_unsafe_wdf_function_binding!(WdfRequestGetStatus, self.request) };
        unsafe { reuse_request(self.request) };
        if !nt_ok(status) {
            return Err(TransportError::io("шина вернула отказ на запись"));
        }
        Ok(())
    }

    /// Читает байт регистра через шину.
    ///
    /// # Errors
    ///
    /// * `Unsupported` — раскладка ответа ещё не подтверждена реверсом (`TODO(RE)`).
    pub fn read_reg(&mut self, _addr: RegAddr) -> Result<u8, TransportError> {
        // Раскладка ответа на `IOCTL_RESOURCE_HUB_TRANSACT` (какие байты несут
        // значение регистра) не подтверждена: см. `docs/REGISTERS.md`.
        // Выдавать догадку за факт нельзя — драйвер честно возвращает отказ,
        // а верхний уровень обрабатывает его как ошибку транспорта.
        Err(TransportError::unsupported(
            "чтение регистра: раскладка ответа не подтверждена реверсом",
        ))
    }

    /// Освобождает объекты WDF. Вызывается при удалении устройства.
    ///
    /// # Safety
    ///
    /// Дескрипторы должны быть валидны; вызов на пассивном уровне.
    #[allow(dead_code)] // Вызывается из обработчика удаления устройства (bring-up).
    pub unsafe fn close(self) {
        // SAFETY: цель создана в `open`; закрываем корректно.
        unsafe {
            let _ = call_unsafe_wdf_function_binding!(WdfIoTargetClose, self.target);
        }
    }
}

impl ChargerTransport for SpmiTransport {
    fn read(&mut self, addr: RegAddr) -> Result<u8, TransportError> {
        self.read_reg(addr)
    }

    fn write(&mut self, addr: RegAddr, value: u8) -> Result<(), TransportError> {
        self.write_reg(addr, value)
    }

    /// Сброса не требуется: соединение с шиной постоянно, состояние держит хаб.
    fn reset(&mut self) -> Result<(), TransportError> {
        Ok(())
    }

    fn name(&self) -> &'static str {
        "spmi"
    }
}

fn nt_ok(status: NTSTATUS) -> bool {
    status >= 0
}

/// Возвращает завершённый запрос в исходное состояние для следующей транзакции.
///
/// # Safety
///
/// `request` должен быть завершён и не использоваться параллельно.
unsafe fn reuse_request(request: WDFREQUEST) {
    let mut params = WDF_REQUEST_REUSE_PARAMS {
        Size: size_of_ulong::<WDF_REQUEST_REUSE_PARAMS>(),
        Flags: 0,
        Status: wdk_sys::STATUS_SUCCESS,
        NewIrp: core::ptr::null_mut(),
    };
    params.Size = size_of_ulong::<WDF_REQUEST_REUSE_PARAMS>();
    // SAFETY: запрос завершён; параметры заполнены.
    unsafe {
        let _ = call_unsafe_wdf_function_binding!(WdfRequestReuse, request, &raw mut params);
    }
}

/// Размер структуры в виде `ULONG` для поля `Size`.
fn size_of_ulong<T>() -> ULONG {
    u32::try_from(core::mem::size_of::<T>()).unwrap_or(0)
}
