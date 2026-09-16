//! Транспорт LN8000: I²C-узел PEIC через Resource Hub и SPB.
//!
//! # Как это устроено в Windows
//!
//! Периферийный драйвер I²C-устройства получает в `_CRS` ресурс
//! `I2cSerialBusV2` с идентификатором подключения (`ConnectionId`). Путь к шине
//! строится по документированному правилу Resource Hub: префикс
//! `\Device\RESOURCE_HUB\` плюс **16 шестнадцатеричных цифр** идентификатора
//! (`RESOURCE_HUB_ID_TO_FILE_NAME`, формат `%0*I64x`, `reshub.h`).
//!
//! Обмен идёт через `IOCTL_SPB_EXECUTE_SEQUENCE` со списком передач
//! ([`crate::spb_abi`]): для чтения регистра — сначала запись его адреса, затем
//! чтение байта; для записи — одна передача «адрес + значение».
//!
//! Источники: `08-driver-samples/Windows-driver-samples/spb/SpbTestTool/sys/`
//! (`peripheral.cpp`, `device.cpp`), заголовки WDK `spb.h` и `reshub.h`.

use crate::spb_abi::{
    IOCTL_SPB_EXECUTE_SEQUENCE, SPB_DIRECTION_FROM_DEVICE, SPB_DIRECTION_TO_DEVICE,
    SpbTransferList, SpbTransferListEntry, entry_init,
};
use ln8000::{BusError, RegAddr, RegisterBus};
use wdk_sys::{
    call_unsafe_wdf_function_binding, GENERIC_READ, GENERIC_WRITE, NTSTATUS, ULONG, UNICODE_STRING,
    WDFDEVICE, WDFIOTARGET, WDFMEMORY, WDFREQUEST, WDF_IO_TARGET_OPEN_PARAMS,
    WDF_REQUEST_SEND_OPTIONS, WDF_NO_HANDLE, WDF_NO_OBJECT_ATTRIBUTES,
    _WDF_IO_TARGET_OPEN_TYPE::WdfIoTargetOpenByName,
    _WDF_REQUEST_SEND_OPTIONS_FLAGS::WDF_REQUEST_SEND_OPTION_TIMEOUT,
};

/// Префикс пути Resource Hub (`RESOURCE_HUB_DEVICE_NAME_PREFIX`).
pub const RESOURCE_HUB_PREFIX: &str = "/Device/RESOURCE_HUB/";

/// Максимальное число передач в последовательности.
pub const MAX_TRANSFERS: usize = 2;

/// Таймаут транзакции в единицах 100 нс (одна секунда).
const SPB_TIMEOUT_100NS: i64 = -10_000 * 1_000;

/// Размер области передач с запасом под элементы и данные.
const TRANSFER_AREA: usize = 256;

/// Транспорт LN8000 поверх SPB.
///
/// Объекты WDF создаются один раз в [`SpbBus::open`] и живут до удаления
/// устройства: повторные чтения и записи не создают новых объектов.
#[derive(Debug)]
pub struct SpbBus {
    target: WDFIOTARGET,
    request: WDFREQUEST,
    input: WDFMEMORY,
    output: WDFMEMORY,
    area: *mut u8,
    data: *mut u8,
    peripheral_id: u64,
    name: &'static str,
}

impl SpbBus {
    /// Открывает шину для периферии с указанным идентификатором подключения.
    ///
    /// # Errors
    ///
    /// * `Io` — цель не создалась или узел Resource Hub недоступен.
    /// * `Unsupported` — WDF не дал создать запрос или буферы.
    ///
    /// # Safety
    ///
    /// Вызывается на пассивном уровне IRQL (из `EvtDevicePrepareHardware`).
    pub unsafe fn open(device: WDFDEVICE, peripheral_id: u64) -> Result<Self, BusError> {
        let mut target: WDFIOTARGET = WDF_NO_HANDLE.cast();
        // SAFETY: `device` — валидный WDFDEVICE; дескриптор под выход.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfIoTargetCreate,
                device,
                WDF_NO_OBJECT_ATTRIBUTES,
                &raw mut target,
            )
        };
        if !nt_ok(status) {
            return Err(BusError::io("не удалось создать цель ввода-вывода"));
        }

        let path = resource_hub_path(peripheral_id);
        let mut params: WDF_IO_TARGET_OPEN_PARAMS = unsafe { core::mem::zeroed() };
        params.Size = size_of_ulong::<WDF_IO_TARGET_OPEN_PARAMS>();
        params.Type = WdfIoTargetOpenByName;
        params.TargetDeviceName = path.as_unicode_string();
        params.DesiredAccess = GENERIC_READ | GENERIC_WRITE;
        params.ShareAccess = 0;
        params.CreateDisposition = wdk_sys::FILE_OPEN;
        params.FileAttributes = wdk_sys::FILE_ATTRIBUTE_NORMAL;

        // SAFETY: цель создана, параметры заполнены, уровень пассивный.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(WdfIoTargetOpen, target, &raw mut params)
        };
        if !nt_ok(status) {
            return Err(BusError::io(
                "узел PEIC недоступен через Resource Hub (проверьте _CRS)",
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
            return Err(BusError::unsupported("не удалось создать WDFREQUEST"));
        }

        let mut area: *mut core::ffi::c_void = core::ptr::null_mut();
        // SAFETY: память выделяется в невыгружаемом пуле под список передач.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfMemoryCreate,
                WDF_NO_OBJECT_ATTRIBUTES,
                wdk_sys::_POOL_TYPE::NonPagedPool,
                0,
                TRANSFER_AREA,
                &raw mut input,
                &raw mut area,
            )
        };
        if !nt_ok(status) {
            return Err(BusError::unsupported("не удалось выделить буфер передач"));
        }

        let mut data: *mut core::ffi::c_void = core::ptr::null_mut();
        // SAFETY: аналогично буферу передач.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfMemoryCreate,
                WDF_NO_OBJECT_ATTRIBUTES,
                wdk_sys::_POOL_TYPE::NonPagedPool,
                0,
                8,
                &raw mut output,
                &raw mut data,
            )
        };
        if !nt_ok(status) {
            return Err(BusError::unsupported("не удалось выделить буфер данных"));
        }

        Ok(Self {
            target,
            request,
            input,
            output,
            area: area.cast::<u8>(),
            data: data.cast::<u8>(),
            peripheral_id,
            name: "spb-i2c",
        })
    }

    /// Идентификатор подключения, полученный из `_CRS`.
    ///
    /// Оставлен для журнала и диагностики на этапе bring-up.
    #[allow(dead_code)]
    #[must_use]
    pub const fn peripheral_id(&self) -> u64 {
        self.peripheral_id
    }

    /// Человекочитаемый путь шины (для журнала).
    #[must_use]
    pub fn path_string(&self) -> [u8; HUB_PATH_CHARS] {
        resource_hub_path(self.peripheral_id).as_ascii()
    }

    /// Выполняет одну транзакцию: чтение или запись регистра.
    ///
    /// # Errors
    ///
    /// * `Protocol` — не удалось подготовить список передач.
    /// * `Timeout` — шина не ответила за секунду.
    /// * `Io` — шина вернула отказ.
    pub fn transact(&mut self, addr: RegAddr, value: Option<u8>) -> Result<u8, BusError> {
        // SAFETY: область передач выделена размером TRANSFER_AREA и живёт до
        // удаления устройства; указатели внутри списка ссылаются на неё же.
        unsafe { self.prepare(addr, value)? };

        // SAFETY: запрос, память и цель валидны; формат — управляющий запрос IOCTL.
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
            return Err(BusError::protocol("SPB отклонил последовательность"));
        }

        let mut options: WDF_REQUEST_SEND_OPTIONS = unsafe { core::mem::zeroed() };
        options.Size = size_of_ulong::<WDF_REQUEST_SEND_OPTIONS>();
        options.Flags = WDF_REQUEST_SEND_OPTION_TIMEOUT as ULONG;
        options.Timeout = SPB_TIMEOUT_100NS;

        // SAFETY: отправка синхронная, уровень пассивный, повторный вход исключён
        // последовательной очередью устройства и таймером с автосериализацией.
        let sent = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfRequestSend,
                self.request,
                self.target,
                &raw mut options,
            )
        };
        if sent == 0 {
            unsafe { reuse_request(self.request) };
            return Err(BusError::timeout("шина I²C не ответила"));
        }

        // SAFETY: запрос завершён; читаем статус и значение.
        let status = unsafe { call_unsafe_wdf_function_binding!(WdfRequestGetStatus, self.request) };
        // SAFETY: буфер данных создан размером 8 байт.
        let byte = unsafe { core::ptr::read_volatile(self.data) };
        unsafe { reuse_request(self.request) };
        if !nt_ok(status) {
            return Err(BusError::io("SPB вернул отказ на транзакцию"));
        }
        Ok(if value.is_none() { byte } else { 0 })
    }

    /// Заполняет область передач под чтение или запись регистра.
    ///
    /// # Safety
    ///
    /// Область передач выделена размером [`TRANSFER_AREA`]; вызов идёт с
    /// пассивного уровня, сериализация обеспечена вызывающей стороной.
    unsafe fn prepare(&mut self, addr: RegAddr, value: Option<u8>) -> Result<(), BusError> {
        if self.area.is_null() || self.data.is_null() {
            return Err(BusError::unsupported("буферы SPB не созданы"));
        }
        // SAFETY: выравнивание области обеспечено аллокатором невыгружаемого пула.
        let list = self.area.cast::<SpbTransferList>();
        // SAFETY: запись заголовка списка.
        unsafe {
            (*list).size = u32::try_from(SpbTransferList::header_size()).unwrap_or(0);
            (*list).reserved = 0;
            (*list).transfer_count = match value {
                Some(_) => 1,
                None => 2,
            };
        }

        // Данные лежат сразу за элементами списка.
        let data_offset = SpbTransferList::area_size(MAX_TRANSFERS);
        // SAFETY: смещение с запасом входит в TRANSFER_AREA.
        let payload = unsafe { self.area.add(data_offset) };
        match value {
            Some(byte) => {
                // Запись: одна передача «адрес + значение».
                let mut frame = [addr, byte];
                // SAFETY: копируем два байта в выделенную область.
                unsafe {
                    core::ptr::copy_nonoverlapping(frame.as_mut_ptr(), payload, 2);
                    let entry = core::ptr::addr_of_mut!((*list).transfers[0]);
                    *entry = entry_init(
                        SPB_DIRECTION_TO_DEVICE,
                        payload.cast::<core::ffi::c_void>(),
                        2,
                    );
                }
                for index in 1..MAX_TRANSFERS {
                    // SAFETY: элементы лежат подряд за первым.
                    unsafe {
                        let slot = self
                            .area
                            .add(SpbTransferList::header_size())
                            .cast::<SpbTransferListEntry>()
                            .add(index);
                        *slot = entry_init(SPB_DIRECTION_TO_DEVICE, core::ptr::null_mut(), 0);
                    }
                }
            }
            None => {
                // Чтение: запись адреса, затем чтение байта.
                // SAFETY: пишем адрес в область данных.
                unsafe {
                    core::ptr::write_volatile(payload, addr);
                    let first = core::ptr::addr_of_mut!((*list).transfers[0]);
                    *first = entry_init(
                        SPB_DIRECTION_TO_DEVICE,
                        payload.cast::<core::ffi::c_void>(),
                        1,
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
            }
        }
        Ok(())
    }

    /// Освобождает объекты WDF.
    ///
    /// Оставлено для явного закрытия на этапе bring-up: цель ввода-вывода
    /// принадлежит устройству и освобождается WDF автоматически.
    ///
    /// # Safety
    ///
    /// Дескрипторы должны быть валидны; вызов на пассивном уровне.
    #[allow(dead_code)]
    pub unsafe fn close(self) {
        // SAFETY: цель создана в `open`.
        unsafe {
            let _ = call_unsafe_wdf_function_binding!(WdfIoTargetClose, self.target);
        }
    }
}

impl RegisterBus for SpbBus {
    fn read(&mut self, addr: RegAddr) -> Result<u8, BusError> {
        self.transact(addr, None)
    }

    fn write(&mut self, addr: RegAddr, value: u8) -> Result<(), BusError> {
        self.transact(addr, Some(value)).map(|_| ())
    }

    /// Переоткрытие шины выполняется на уровне устройства: цель живёт до
    /// удаления устройства, поэтому здесь достаточно подтвердить готовность.
    fn reset(&mut self) -> Result<(), BusError> {
        Ok(())
    }

    fn name(&self) -> &'static str {
        self.name
    }
}

fn nt_ok(status: NTSTATUS) -> bool {
    status >= 0
}

fn size_of_ulong<T>() -> ULONG {
    u32::try_from(core::mem::size_of::<T>()).unwrap_or(0)
}

/// Возвращает завершённый запрос в исходное состояние.
///
/// # Safety
///
/// `request` должен быть завершён и не использоваться параллельно.
unsafe fn reuse_request(request: WDFREQUEST) {
    let mut params = wdk_sys::WDF_REQUEST_REUSE_PARAMS {
        Size: size_of_ulong::<wdk_sys::WDF_REQUEST_REUSE_PARAMS>(),
        Flags: 0,
        Status: wdk_sys::STATUS_SUCCESS,
        NewIrp: core::ptr::null_mut(),
    };
    params.Size = size_of_ulong::<wdk_sys::WDF_REQUEST_REUSE_PARAMS>();
    // SAFETY: запрос завершён, параметры заполнены.
    unsafe {
        let _ = call_unsafe_wdf_function_binding!(WdfRequestReuse, request, &raw mut params);
    }
}

/// Путь к узлу Resource Hub: префикс плюс 16 шестнадцатеричных цифр.
///
/// Формат совпадает с `RESOURCE_HUB_ID_TO_FILE_NAME` (`%0*I64x`, ширина 16) из
/// `reshub.h` WDK.
#[must_use]
pub fn resource_hub_path(peripheral_id: u64) -> HubPath {
    let mut chars = [0_u16; HUB_PATH_CHARS];
    let prefix = RESOURCE_HUB_PREFIX.as_bytes();
    for (index, byte) in prefix.iter().enumerate() {
        if let Some(slot) = chars.get_mut(index) {
            *slot = if *byte == b'/' {
                b'\\' as u16
            } else {
                u16::from(*byte)
            };
        }
    }
    for position in 0..16_u32 {
        let shift = 60_u32.saturating_sub(position.saturating_mul(4));
        let nibble = ((peripheral_id >> shift) & 0xF) as u8;
        let symbol = match nibble {
            0..=9 => b'0' + nibble,
            _ => b'a' + (nibble - 10),
        };
        let index = RESOURCE_HUB_PREFIX.len().saturating_add(usize::try_from(position).unwrap_or(0));
        if let Some(slot) = chars.get_mut(index) {
            *slot = u16::from(symbol);
        }
    }
    HubPath { chars }
}

/// Число символов в пути (префикс + 16 цифр).
pub const HUB_PATH_CHARS: usize = RESOURCE_HUB_PREFIX.len() + 16;

/// Путь к узлу Resource Hub в UTF-16.
#[derive(Debug, Clone, Copy)]
pub struct HubPath {
    chars: [u16; HUB_PATH_CHARS],
}

impl HubPath {
    /// Представление пути как `UNICODE_STRING` (без завершающего нуля).
    #[must_use]
    pub fn as_unicode_string(&self) -> UNICODE_STRING {
        let length = u16::try_from(HUB_PATH_CHARS.saturating_mul(2)).unwrap_or(0);
        UNICODE_STRING {
            Length: length,
            MaximumLength: length,
            Buffer: self.chars.as_ptr().cast_mut(),
        }
    }

    /// Путь в виде ASCII-байтов (для журнала и тестов).
    #[must_use]
    pub fn as_ascii(&self) -> [u8; HUB_PATH_CHARS] {
        let mut out = [0_u8; HUB_PATH_CHARS];
        for (index, symbol) in self.chars.iter().enumerate() {
            if let Some(slot) = out.get_mut(index) {
                *slot = u8::try_from(*symbol).unwrap_or(b'?');
            }
        }
        out
    }
}
