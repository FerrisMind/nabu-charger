//! Транспорт к регистрам PMIC через шину SPMI.
//!
//! # Что подтверждено реверсом
//!
//! Реверс штатных драйверов Qualcomm (`qcpmicEIC8150.sys`, `qcspmi8150.sys`)
//! показал два разных механизма, и их важно не путать:
//!
//! 1. **Подключение к SPMI** — служебный код `0x32C004` (это
//!    `CTL_CODE(0x32, 1, METHOD_BUFFERED, FILE_READ|FILE_WRITE)`). Штатный клиент
//!    посылает запрос из 8 байт (младшие 4 — сигнатура `0x42696541`, старшие 4 —
//!    константа из `.rdata`) и получает буфер, который разбирает в свой контекст, а
//!    затем ставит таблицу функций доступа к регистрам, выбирая её по версии
//!    контроллера. Раскладка этого запроса и ответа подтверждена **частично**.
//!
//! 2. **Доступ к регистрам** — публичный код `0x41808`:
//!
//!    ```text
//!    0x41808 = CTL_CODE(FILE_DEVICE_CONTROLLER, 0x602, METHOD_BUFFERED, FILE_ANY_ACCESS)
//!            = IOCTL_SPB_EXECUTE_SEQUENCE
//!    ```
//!
//!    То есть регистры читаются и пишутся обычным списком передач SPB
//!    (`SPB_TRANSFER_LIST`) — тем же интерфейсом, что использует драйвер LN8000 на
//!    шине I²C. Приватного протокола «регистр-в-ответе» нет, и именно поэтому
//!    поиск такой раскладки раньше не давал результата.
//!
//! Разбор с адресами и выдержками — `docs/SPMI-PATH.md`.
//!
//! # Чего не хватает
//!
//! Чтобы отправить список передач, нужна **цель ввода-вывода шины SPMI**. Штатный
//! клиент получает её первым шагом (`0x32C004`) через хаб ресурсов. Наш драйвер
//! такой шаг пока не выполняет: он открывает устройство хаба по имени и посылает
//! последовательность ему. На железе это может оказаться недостаточно — тогда
//! запрос честно вернёт ошибку транспорта, а не «тихо ничего не сделает».
//! Правильное решение — получить подключение SPMI из `_CRS` своего узла, поэтому
//! драйверу нужен колбэк подготовки ресурсов (его сейчас нет).
//!
//! # Уровень IRQL
//!
//! Все операции синхронные, пассивного уровня: вызываются из
//! `EvtIoDeviceControl` последовательной очереди.

use charger_core::{ChargerTransport, RegAddr, TransportError};
use spb::{
    entry_init, SpbTransferList, SpbTransferListEntry, IOCTL_SPB_EXECUTE_SEQUENCE,
    SPB_DIRECTION_FROM_DEVICE, SPB_DIRECTION_TO_DEVICE,
};
use wdk_sys::{
    _POOL_TYPE::NonPagedPool, _WDF_IO_TARGET_OPEN_TYPE::WdfIoTargetOpenByName,
    _WDF_REQUEST_SEND_OPTIONS_FLAGS::WDF_REQUEST_SEND_OPTION_TIMEOUT,
    call_unsafe_wdf_function_binding, NTSTATUS, ULONG, UNICODE_STRING, WDFDEVICE, WDFIOTARGET,
    WDFMEMORY, WDFREQUEST, WDF_IO_TARGET_OPEN_PARAMS, WDF_REQUEST_REUSE_PARAMS,
    WDF_REQUEST_SEND_OPTIONS, WDF_NO_HANDLE, WDF_NO_OBJECT_ATTRIBUTES,
};

/// Служебный код подключения к SPMI (подтверждён реверсом; шаг не выполняется).
#[allow(dead_code)]
pub const IOCTL_RESOURCE_HUB_TRANSACT: u32 = 0x0032_C004;

/// Тип устройства хаба (старшие 16 бит кода `0x32C004`).
#[allow(dead_code)]
pub const FILE_DEVICE_RESOURCE_HUB: u32 = 0x32;

/// Область под список передач, байт.
const TRANSFER_AREA: usize = 256;

/// Буфер данных (адрес и значение регистра).
const DATA_LEN: usize = 8;

/// Наибольшее число передач в одной последовательности.
const MAX_TRANSFERS: usize = 2;

/// Смещение полезной нагрузки: сразу за списком из двух передач.
///
/// `sizeof(SPB_TRANSFER_LIST) + sizeof(SPB_TRANSFER_LIST_ENTRY)` = 48 + 32 = 80.
const PAYLOAD_OFFSET: usize = 80;

/// Таймаут одной транзакции: 1 с в единицах по 100 нс (значение отрицательное —
/// отсчёт относительный, как требует `WDF_REQUEST_SEND_OPTIONS.Timeout`).
const SPB_TIMEOUT_100NS: i64 = -10_000_000;

/// Настройки доступа к шине.
#[derive(Debug, Clone, Copy)]
pub struct SpmiConfig {
    /// Таймаут одной транзакции в единицах по 100 нс (отрицательное значение).
    pub timeout_100ns: i64,
    /// Порядок байт адреса регистра: `true` — старший байт первым.
    ///
    /// Принято по спецификации SPMI: командный кадр передаёт адрес, начиная со
    /// старшего байта. **На железе не подтверждено** — поэтому это параметр, а не
    /// зашитая константа: при первом же прогоне на планшете его можно перевернуть
    /// без правки логики.
    pub address_big_endian: bool,
}

impl SpmiConfig {
    /// Конфигурация по умолчанию для планшета `nabu`.
    #[must_use]
    pub const fn nabu() -> Self {
        Self {
            timeout_100ns: SPB_TIMEOUT_100NS,
            address_big_endian: true,
        }
    }

    /// Байты адреса регистра в порядке, заданном конфигурацией.
    #[must_use]
    fn address_bytes(&self, addr: RegAddr) -> [u8; 2] {
        if self.address_big_endian {
            addr.to_be_bytes()
        } else {
            addr.to_le_bytes()
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
/// Объекты WDF создаются один раз при добавлении устройства и живут до его
/// удаления, поэтому повторные чтения и записи не создают новых объектов.
#[derive(Debug)]
pub struct SpmiTransport {
    target: WDFIOTARGET,
    request: WDFREQUEST,
    input: WDFMEMORY,
    output: WDFMEMORY,
    area: *mut u8,
    data: *mut u8,
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
    /// Вызывается на пассивном уровне IRQL (из `EvtDeviceAdd`): создание объектов
    /// WDF и открытие цели по имени на повышенном уровне запрещено.
    pub unsafe fn open(device: WDFDEVICE, config: SpmiConfig) -> Result<Self, TransportError> {
        let mut target: WDFIOTARGET = WDF_NO_HANDLE.cast();
        // SAFETY: `device` — валидный WDFDEVICE; `target` — локальная переменная.
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
        let status =
            unsafe { call_unsafe_wdf_function_binding!(WdfIoTargetOpen, target, &raw mut params) };
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

        let mut area: *mut core::ffi::c_void = core::ptr::null_mut();
        // SAFETY: память выделяется в невыгружаемом пуле и живёт до удаления
        // устройства; выравнивание пула достаточно для `SPB_TRANSFER_LIST`.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfMemoryCreate,
                WDF_NO_OBJECT_ATTRIBUTES,
                NonPagedPool,
                0,
                TRANSFER_AREA,
                &raw mut input,
                &raw mut area,
            )
        };
        if !nt_ok(status) {
            return Err(TransportError::unsupported("не удалось выделить область передач"));
        }

        let mut data: *mut core::ffi::c_void = core::ptr::null_mut();
        // SAFETY: аналогично области передач.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfMemoryCreate,
                WDF_NO_OBJECT_ATTRIBUTES,
                NonPagedPool,
                0,
                DATA_LEN,
                &raw mut output,
                &raw mut data,
            )
        };
        if !nt_ok(status) {
            return Err(TransportError::unsupported("не удалось выделить буфер данных"));
        }

        Ok(Self {
            target,
            request,
            input,
            output,
            area: area.cast::<u8>(),
            data: data.cast::<u8>(),
            config,
        })
    }

    /// Записывает байт регистра: одна передача «адрес (16 бит) + значение».
    ///
    /// # Errors
    ///
    /// * `Io` — шина отказала.
    /// * `Timeout` — шина не ответила за [`SpmiConfig::timeout_100ns`].
    /// * `Protocol` — шина отклонила последовательность.
    pub fn write_reg(&mut self, addr: RegAddr, value: u8) -> Result<(), TransportError> {
        if self.area.is_null() || self.data.is_null() {
            return Err(TransportError::unsupported("буферы SPB не созданы"));
        }
        let bytes = self.config.address_bytes(addr);

        // SAFETY: полезная нагрузка лежит внутри выделенной области с запасом;
        // запись идёт с пассивного уровня, сериализация — на вызывающей стороне.
        let (payload, list) = unsafe { self.begin(1)? };
        // SAFETY: пишем «адрес, значение» и одну передачу на три байта.
        unsafe {
            core::ptr::write_volatile(payload, bytes[0]);
            core::ptr::write_volatile(payload.add(1), bytes[1]);
            core::ptr::write_volatile(payload.add(2), value);
            *core::ptr::addr_of_mut!((*list).transfers[0]) = entry_init(
                SPB_DIRECTION_TO_DEVICE,
                payload.cast::<core::ffi::c_void>(),
                3,
            );
        }
        self.send()
    }

    /// Читает байт регистра: передача адреса (16 бит), затем чтение байта.
    ///
    /// # Errors
    ///
    /// * `Io` — шина отказала.
    /// * `Timeout` — шина не ответила за [`SpmiConfig::timeout_100ns`].
    /// * `Protocol` — шина отклонила последовательность.
    pub fn read_reg(&mut self, addr: RegAddr) -> Result<u8, TransportError> {
        if self.area.is_null() || self.data.is_null() {
            return Err(TransportError::unsupported("буферы SPB не созданы"));
        }
        let bytes = self.config.address_bytes(addr);

        // SAFETY: см. `write_reg`; вторая передача ссылается на буфер данных.
        let (payload, list) = unsafe { self.begin(MAX_TRANSFERS)? };
        // SAFETY: адрес — два байта; первая передача пишет их, вторая читает байт.
        unsafe {
            core::ptr::write_volatile(payload, bytes[0]);
            core::ptr::write_volatile(payload.add(1), bytes[1]);
            *core::ptr::addr_of_mut!((*list).transfers[0]) = entry_init(
                SPB_DIRECTION_TO_DEVICE,
                payload.cast::<core::ffi::c_void>(),
                2,
            );
            let second = self
                .area
                .add(SpbTransferList::header_size())
                .cast::<SpbTransferListEntry>()
                .add(1);
            *second = entry_init(
                SPB_DIRECTION_FROM_DEVICE,
                self.data.cast::<core::ffi::c_void>(),
                1,
            );
        }
        self.send()?;
        // SAFETY: буфер данных создан размером `DATA_LEN`, читаем первый байт.
        Ok(unsafe { core::ptr::read_volatile(self.data) })
    }

    /// Готовит список передач под `count` передач и возвращает нагрузку и список.
    ///
    /// # Errors
    ///
    /// * `Unsupported` — буферы не созданы или передач больше `MAX_TRANSFERS`.
    ///
    /// # Safety
    ///
    /// Вызывается с пассивного уровня; буферы принадлежат транспорту.
    unsafe fn begin(&mut self, count: usize) -> Result<(*mut u8, *mut SpbTransferList), TransportError> {
        if count == 0 || count > MAX_TRANSFERS {
            return Err(TransportError::unsupported("недопустимое число передач"));
        }
        // SAFETY: область передач выделена размером `TRANSFER_AREA`, выравнивание
        // невыгружаемого пула не меньше выравнивания `SPB_TRANSFER_LIST`.
        let list = self.area.cast::<SpbTransferList>();
        // SAFETY: заголовок списка заполняется целиком.
        unsafe {
            (*list).size = u32::try_from(SpbTransferList::header_size()).unwrap_or(0);
            (*list).reserved = 0;
            (*list).transfer_count =
                u32::try_from(count).map_err(|_| TransportError::unsupported("слишком много передач"))?;
        }
        // SAFETY: смещение входит в `TRANSFER_AREA`.
        let payload = unsafe { self.area.add(PAYLOAD_OFFSET) };
        Ok((payload, list))
    }

    /// Отправляет подготовленную последовательность на шину.
    ///
    /// # Errors
    ///
    /// * `Protocol` — шина отклонила формат запроса.
    /// * `Timeout` — шина не ответила.
    /// * `Io` — шина вернула отказ.
    fn send(&mut self) -> Result<(), TransportError> {
        // SAFETY: запрос, память и цель валидны; формат — управляющий запрос IOCTL,
        // входом идёт область со списком передач, выход не используется.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfIoTargetFormatRequestForIoctl,
                self.target,
                self.request,
                IOCTL_SPB_EXECUTE_SEQUENCE,
                self.input,
                core::ptr::null_mut(),
                self.output,
                core::ptr::null_mut(),
            )
        };
        if !nt_ok(status) {
            return Err(TransportError::protocol("шина отклонила последовательность SPB"));
        }

        let mut options: WDF_REQUEST_SEND_OPTIONS = unsafe { core::mem::zeroed() };
        options.Size = size_of_ulong::<WDF_REQUEST_SEND_OPTIONS>();
        options.Flags = WDF_REQUEST_SEND_OPTION_TIMEOUT as ULONG;
        options.Timeout = self.config.timeout_100ns;

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
            // SAFETY: при отказе отправки запрос нужно вернуть в исходное состояние.
            unsafe { reuse_request(self.request) };
            return Err(TransportError::timeout("шина SPMI не ответила за отведённое время"));
        }

        // SAFETY: запрос завершён; читаем статус и переиспользуем запрос.
        let status = unsafe { call_unsafe_wdf_function_binding!(WdfRequestGetStatus, self.request) };
        unsafe { reuse_request(self.request) };
        if !nt_ok(status) {
            return Err(TransportError::io("шина SPMI вернула отказ"));
        }
        Ok(())
    }

    /// Освобождает объекты WDF. Вызывается при удалении устройства.
    ///
    /// # Safety
    ///
    /// Дескрипторы должны быть валидны; вызов на пассивном уровне.
    #[allow(dead_code)]
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
