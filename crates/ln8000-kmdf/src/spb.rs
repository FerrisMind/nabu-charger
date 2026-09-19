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
    ATTACH_MAGIC, ATTACH_REPLY_LEN, IOCTL_ATTACH, IOCTL_SPB_EXECUTE_SEQUENCE, IOCTL_SPB_LOCK_CONNECTION,
    IOCTL_SPMI_SUPERUSER_GRANT, IOCTL_SPMI_SUPERUSER_READ, IOCTL_SPMI_SUPERUSER_WRITE,
    SPMI_SUPERUSER_HEADER_LEN, SPB_DIRECTION_FROM_DEVICE, SPB_DIRECTION_NONE,
    SPB_DIRECTION_TO_DEVICE, SPB_FORMAT_SIMPLE, SpbTransferList, SpbTransferListEntry, entry_init,
};
use ln8000::{BusError, RegAddr, RegisterBus};
use wdk_sys::{
    call_unsafe_wdf_function_binding, NTSTATUS, ULONG, UNICODE_STRING,
    WDFDEVICE, WDFIOTARGET, WDFMEMORY, WDFREQUEST, WDF_IO_TARGET_OPEN_PARAMS,
    WDF_REQUEST_SEND_OPTIONS, WDF_NO_HANDLE, WDF_NO_OBJECT_ATTRIBUTES,
    _WDF_IO_TARGET_OPEN_TYPE::WdfIoTargetOpenByName,
    _WDF_REQUEST_SEND_OPTIONS_FLAGS::{WDF_REQUEST_SEND_OPTION_SYNCHRONOUS, WDF_REQUEST_SEND_OPTION_TIMEOUT},
};

/// Префикс пути Resource Hub (`RESOURCE_HUB_DEVICE_NAME_PREFIX`).
pub const RESOURCE_HUB_PREFIX: &str = "/Device/RESOURCE_HUB/";

/// Максимальное число передач в последовательности.
pub const MAX_TRANSFERS: usize = 3;

/// Таймаут транзакции в единицах 100 нс (одна секунда).
const SPB_TIMEOUT_100NS: i64 = -10_000 * 1_000;

/// Маска доступа при открытии узла Resource Hub: как у штатного драйвера
/// (`FILE_GENERIC_READ|FILE_GENERIC_WRITE|SYNCHRONIZE` = 0x1F01FF).
const HUB_DESIRED_ACCESS: u32 = 0x001F_01FF;

/// Формат буфера `SimpleNonPaged`: разрешает буфер вне буферов запроса.
const SPB_FORMAT_SIMPLE_NON_PAGED: u32 = 3;

/// Вариант 1: данные в буфере данных запроса, формат `Simple`.
const VARIANT_OUTPUT_SIMPLE: u8 = 0;
/// Вариант 2: те же данные, но формат `SimpleNonPaged`.
const VARIANT_OUTPUT_NON_PAGED: u8 = 1;
/// Вариант 5: список завершается элементом с направлением `None`.
const VARIANT_TERMINATED: u8 = 3;
/// Вариант 6: данные внутри выходного буфера, который передан в запрос.
const VARIANT_OUTPUT_MEMORY: u8 = 4;
/// Вариант 7: то же, но формат `SimpleNonPaged`.
const VARIANT_OUTPUT_MEMORY_NON_PAGED: u8 = 5;

/// Размер области передач с запасом под элементы и данные.
/// Размер области передач: ровно под максимальное число передач.
const TRANSFER_AREA: usize = SpbTransferList::area_size(MAX_TRANSFERS);

/// Транспорт LN8000 поверх SPB.
///
/// Объекты WDF создаются один раз в [`SpbBus::open`] и живут до удаления
/// устройства: повторные чтения и записи не создают новых объектов.
#[derive(Debug)]
pub struct SpbBus {
    target: WDFIOTARGET,
    request: WDFREQUEST,
    /// Владелец области передач: сам буфер не читается, но держит выделение.
    #[allow(dead_code)]
    input: WDFMEMORY,
    /// Буфер данных: в запрос не передаётся (так делает эталонный пример),
    /// но нужен как область для байтов обмена.
    #[allow(dead_code)]
    output: WDFMEMORY,
    area: *mut u8,
    data: *mut u8,
    peripheral_id: u64,
    name: &'static str,
    /// Статус последнего обмена: нужен при разборе отказов на железе,
    /// где отладочный вывод драйвера недоступен.
    last_status: i32,
    /// Отправка не завершилась, и WDF всё ещё владеет кэшированным запросом.
    /// Повторная отправка такого запроса — фатальная ошибка WDF, поэтому до
    /// перезапуска устройства шина отвечает отказом вместо обмена.
    request_lost: bool,
    /// Как оформлять запрос: см. `VARIANT_*`. Переключается пробой.
    variant: u8,
    /// Вид буфера передач на одну передачу: точная длина 48 байт.
    input_one: WDFMEMORY,
    /// Вид буфера передач на две передачи: точная длина 80 байт.
    input_two: WDFMEMORY,
    /// Вид буфера передач на три передачи (с завершающим элементом).
    input_three: WDFMEMORY,
    /// Буфер входа запроса подключения (8 байт).
    attach_in: WDFMEMORY,
    /// Буфер ответа на запрос подключения (1024 байта).
    attach_out: WDFMEMORY,
    /// Указатель на вход подключения.
    attach_in_ptr: *mut u8,
    /// Указатель на ответ подключения.
    attach_out_ptr: *mut u8,
    /// Первые слова ответа узла: доказательство для реестра.
    attach_words: [u32; 4],
    /// Сколько передач объявил последний собранный список.
    last_count: u32,
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
    pub unsafe fn open(device: WDFDEVICE, peripheral_id: u64, use_hub: bool) -> Result<Self, BusError> {
        // Цель — либо родитель устройства (стек контроллера шины), либо узел
        // Resource Hub по идентификатору подключения.
        //
        // Проверено на планшете: последовательность в родителя проходит, но
        // адреса устройства там нет; в узел адрес есть, но запрос отвергается.
        // Единственное отличие нашего открытия узла от штатного драйвера —
        // маска доступа, поэтому она вынесена отдельно.
        let target: WDFIOTARGET = if use_hub {
            let mut hub: WDFIOTARGET = WDF_NO_HANDLE.cast();
            // SAFETY: устройство создано; дескриптор — локальная переменная.
            let status = unsafe {
                call_unsafe_wdf_function_binding!(
                    WdfIoTargetCreate,
                    device,
                    WDF_NO_OBJECT_ATTRIBUTES,
                    &raw mut hub,
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
            // Маска как у штатного драйвера узла: с ней узел выдаёт объект,
            // который принимает последовательности.
            params.DesiredAccess = HUB_DESIRED_ACCESS;
            params.ShareAccess = 0;
            params.CreateDisposition = wdk_sys::FILE_OPEN;
            params.FileAttributes = wdk_sys::FILE_ATTRIBUTE_NORMAL;
            // SAFETY: цель создана, параметры заполнены, уровень пассивный.
            let status = unsafe {
                call_unsafe_wdf_function_binding!(WdfIoTargetOpen, hub, &raw mut params)
            };
            if !nt_ok(status) {
                return Err(BusError::io("узел Resource Hub недоступен"));
            }
            hub
        } else {
            // SAFETY: устройство создано; цель принадлежит WDF.
            let parent: WDFIOTARGET =
                unsafe { call_unsafe_wdf_function_binding!(WdfDeviceGetIoTarget, device) };
            if parent.is_null() {
                return Err(BusError::io("нет цели родителя устройства"));
            }
            parent
        };

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

        // Виды того же буфера с точной длиной: узел проверяет длину списка
        // передач, поэтому "с запасом" он не принимает.
        let one_len = SpbTransferList::area_size(1);
        let two_len = SpbTransferList::area_size(2);
        let mut input_one: WDFMEMORY = WDF_NO_HANDLE.cast();
        // SAFETY: буфер выделен выше и живёт до удаления устройства.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfMemoryCreatePreallocated,
                WDF_NO_OBJECT_ATTRIBUTES,
                area,
                one_len,
                &raw mut input_one,
            )
        };
        if !nt_ok(status) {
            return Err(BusError::unsupported("не удалось создать вид буфера на одну передачу"));
        }
        let mut input_two: WDFMEMORY = WDF_NO_HANDLE.cast();
        // SAFETY: то же самое, длина на две передачи.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfMemoryCreatePreallocated,
                WDF_NO_OBJECT_ATTRIBUTES,
                area,
                two_len,
                &raw mut input_two,
            )
        };
        if !nt_ok(status) {
            return Err(BusError::unsupported("не удалось создать вид буфера на две передачи"));
        }
        let three_len = SpbTransferList::area_size(3);
        let mut input_three: WDFMEMORY = WDF_NO_HANDLE.cast();
        // SAFETY: то же самое, длина на три передачи.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfMemoryCreatePreallocated,
                WDF_NO_OBJECT_ATTRIBUTES,
                area,
                three_len,
                &raw mut input_three,
            )
        };
        if !nt_ok(status) {
            return Err(BusError::unsupported("не удалось создать вид буфера на три передачи"));
        }

        let mut attach_in: *mut core::ffi::c_void = core::ptr::null_mut();
        let mut attach_in_mem: WDFMEMORY = WDF_NO_HANDLE.cast();
        // SAFETY: вход подключения — восемь байт в невыгружаемом пуле.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfMemoryCreate,
                WDF_NO_OBJECT_ATTRIBUTES,
                wdk_sys::_POOL_TYPE::NonPagedPool,
                0,
                8,
                &raw mut attach_in_mem,
                &raw mut attach_in,
            )
        };
        if !nt_ok(status) {
            return Err(BusError::unsupported("не удалось выделить вход подключения"));
        }
        let mut attach_out: *mut core::ffi::c_void = core::ptr::null_mut();
        let mut attach_out_mem: WDFMEMORY = WDF_NO_HANDLE.cast();
        // SAFETY: ответ подключения — 1024 байта в невыгружаемом пуле.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfMemoryCreate,
                WDF_NO_OBJECT_ATTRIBUTES,
                wdk_sys::_POOL_TYPE::NonPagedPool,
                0,
                ATTACH_REPLY_LEN,
                &raw mut attach_out_mem,
                &raw mut attach_out,
            )
        };
        if !nt_ok(status) {
            return Err(BusError::unsupported("не удалось выделить ответ подключения"));
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
            last_status: 0,
            request_lost: false,
            variant: VARIANT_OUTPUT_SIMPLE,
            input_one,
            input_two,
            input_three,
            attach_in: attach_in_mem,
            attach_out: attach_out_mem,
            attach_in_ptr: attach_in.cast::<u8>(),
            attach_out_ptr: attach_out.cast::<u8>(),
            attach_words: [0; 4],
            last_count: 0,
        })
    }

    /// Переключает оформление запроса к узлу шины.
    pub fn set_variant(&mut self, variant: u8) {
        self.variant = variant;
    }

    /// Сырой статус последнего обмена по шине (`NTSTATUS`).
    ///
    /// Ноль — успех; отрицательное значение — код отказа от WDF или от узла шины.
    #[must_use]
    pub fn last_status(&self) -> i32 {
        self.last_status
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

    /// Проверяет, что кэшированный запрос можно отправлять снова.
    ///
    /// # Errors
    ///
    /// `Timeout` — запрос потерян (см. [`Self::send_failed`]).
    fn cached_request(&self) -> Result<(), BusError> {
        if self.request_lost {
            return Err(BusError::timeout(
                "кэшированный запрос потерян: нужен перезапуск устройства",
            ));
        }
        Ok(())
    }

    /// Помечает кэшированный запрос потерянным после неудачной отправки.
    ///
    /// `WdfRequestSend` вернул `false` при выставленной опции `TIMEOUT` — значит,
    /// запрос остался у I/O-таргета: он завершится позже либо будет отменён.
    /// Вызывать на нём `WdfRequestReuse`, а затем отправлять повторно нельзя —
    /// WDF на это отвечает `WDF_VIOLATION (0x10D)` с `Arg2 = 3` («запрос уже
    /// отправлен I/O-таргету»). Именно так ядро падало три раза 18.09.
    /// Запрос освободит сам WDF при удалении устройства, поэтому дальше драйвер
    /// только отказывает до перезапуска.
    fn send_failed(&mut self, reason: &'static str) -> BusError {
        self.request_lost = true;
        self.last_status = -1;
        BusError::timeout(reason)
    }

    /// Выполняет одну транзакцию: чтение или запись регистра.
    ///
    /// # Errors
    ///
    /// * `Protocol` — не удалось подготовить список передач.
    /// * `Timeout` — шина не ответила за секунду.
    /// * `Io` — шина вернула отказ.
    pub fn transact(&mut self, addr: RegAddr, value: Option<u8>) -> Result<u8, BusError> {
        self.cached_request()?;
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
                match self.last_count {
                    1 => self.input_one,
                    2 => self.input_two,
                    _ => self.input_three,
                },
                core::ptr::null_mut(),
                // Выходной буфер передаём только в вариантах, которые это
                // проверяют: данные лежат внутри него.
                if self.variant >= VARIANT_OUTPUT_MEMORY {
                    self.output
                } else {
                    core::ptr::null_mut()
                },
                core::ptr::null_mut(),
            )
        };
        if !nt_ok(status) {
            return Err(BusError::protocol("SPB отклонил последовательность"));
        }

        let mut options: WDF_REQUEST_SEND_OPTIONS = unsafe { core::mem::zeroed() };
        options.Size = size_of_ulong::<WDF_REQUEST_SEND_OPTIONS>();
        options.Flags = (WDF_REQUEST_SEND_OPTION_SYNCHRONOUS | WDF_REQUEST_SEND_OPTION_TIMEOUT) as ULONG;
        options.Timeout = SPB_TIMEOUT_100NS;

        // SAFETY: отправка синхронная, уровень пассивный; от повторного входа
        // шину защищает мьютекс состояния в `lib.rs` — очередь WDF сериализует
        // только IOCTL, а таймер телеметрии идёт своим контекстом.
        let sent = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfRequestSend,
                self.request,
                self.target,
                &raw mut options,
            )
        };
        if sent == 0 {
            return Err(self.send_failed("шина I²C не ответила"));
        }

        // SAFETY: запрос завершён; читаем статус и значение.
        let status = unsafe { call_unsafe_wdf_function_binding!(WdfRequestGetStatus, self.request) };
        self.last_status = status;
        // SAFETY: буфер данных не меньше двух байт; байт ответа идёт вторым —
        // первым записан адрес регистра.
        let byte = unsafe { core::ptr::read_volatile(self.data.add(1)) };
        unsafe { reuse_request(self.request) };
        if !nt_ok(status) {
            return Err(BusError::io("SPB вернул отказ на транзакцию"));
        }
        Ok(if value.is_none() { byte } else { 0 })
    }

    /// SPMI-транзакция с 16-битным адресом регистра (USBIN / PM8150B).
    ///
    /// В отличие от I²C LN8000 (1 байт адреса), SPMI передаёт два байта адреса,
    /// затем значение. Порядок байт задаётся `big_endian` (по умолчанию BE —
    /// как в спецификации SPMI; на железе может потребоваться LE).
    ///
    /// # Errors
    ///
    /// Те же, что у [`Self::transact`].
    pub fn transact_spmi16(
        &mut self,
        addr: u16,
        value: Option<u8>,
        big_endian: bool,
    ) -> Result<u8, BusError> {
        self.cached_request()?;
        // SAFETY: буферы созданы в `open`; пассивный уровень.
        unsafe { self.prepare_spmi16(addr, value, big_endian)? };
        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfIoTargetFormatRequestForIoctl,
                self.target,
                self.request,
                IOCTL_SPB_EXECUTE_SEQUENCE,
                match self.last_count {
                    1 => self.input_one,
                    2 => self.input_two,
                    _ => self.input_three,
                },
                core::ptr::null_mut(),
                if self.variant >= VARIANT_OUTPUT_MEMORY {
                    self.output
                } else {
                    core::ptr::null_mut()
                },
                core::ptr::null_mut(),
            )
        };
        if !nt_ok(status) {
            return Err(BusError::protocol("SPB отклонил SPMI-последовательность"));
        }

        let mut options: WDF_REQUEST_SEND_OPTIONS = unsafe { core::mem::zeroed() };
        options.Size = size_of_ulong::<WDF_REQUEST_SEND_OPTIONS>();
        options.Flags =
            (WDF_REQUEST_SEND_OPTION_SYNCHRONOUS | WDF_REQUEST_SEND_OPTION_TIMEOUT) as ULONG;
        options.Timeout = SPB_TIMEOUT_100NS;

        let sent = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfRequestSend,
                self.request,
                self.target,
                &raw mut options,
            )
        };
        if sent == 0 {
            return Err(self.send_failed("шина SPMI не ответила"));
        }

        let status = unsafe { call_unsafe_wdf_function_binding!(WdfRequestGetStatus, self.request) };
        self.last_status = status;
        // Адрес — два байта; ответ — третий байт буфера данных.
        let byte = unsafe { core::ptr::read_volatile(self.data.add(2)) };
        unsafe { reuse_request(self.request) };
        if !nt_ok(status) {
            return Err(BusError::io("SPB вернул отказ на SPMI-транзакцию"));
        }
        Ok(if value.is_none() { byte } else { 0 })
    }

    /// Пробует выполнить подключение к периферии.
    ///
    /// Эталонный драйвер делает этот шаг до доступа к регистрам: шлёт во входе
    /// восемь байт (магия `0x42696541` плюс четыре байта) и получает 1024 байта
    /// ответа. Возвращает статус запроса; первые слова ответа сохраняются.
    pub fn attach(&mut self) -> i32 {
        if self.request_lost {
            return -1;
        }
        if self.attach_in_ptr.is_null() || self.attach_out_ptr.is_null() {
            return -1;
        }
        // SAFETY: буферы созданы в `open`; входа ровно восемь байт.
        unsafe {
            core::ptr::write_volatile(self.attach_in_ptr.cast::<u32>(), ATTACH_MAGIC);
            core::ptr::write_volatile(self.attach_in_ptr.add(4).cast::<u32>(), 1);
        }
        // SAFETY: цель и запрос валидны; буферы созданы в `open`.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfIoTargetFormatRequestForIoctl,
                self.target,
                self.request,
                IOCTL_ATTACH,
                self.attach_in,
                core::ptr::null_mut(),
                self.attach_out,
                core::ptr::null_mut(),
            )
        };
        if !nt_ok(status) {
            self.last_status = status;
            return status;
        }
        let mut options: WDF_REQUEST_SEND_OPTIONS = unsafe { core::mem::zeroed() };
        options.Size = size_of_ulong::<WDF_REQUEST_SEND_OPTIONS>();
        options.Flags = (WDF_REQUEST_SEND_OPTION_SYNCHRONOUS | WDF_REQUEST_SEND_OPTION_TIMEOUT) as ULONG;
        options.Timeout = SPB_TIMEOUT_100NS;
        // SAFETY: синхронная отправка на пассивном уровне.
        let sent = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfRequestSend,
                self.request,
                self.target,
                &raw mut options,
            )
        };
        if sent == 0 {
            let _ = self.send_failed("I/O-таргет не завершил запрос");
            return -1;
        }
        // SAFETY: запрос завершён; читаем статус и первые слова ответа.
        let status = unsafe { call_unsafe_wdf_function_binding!(WdfRequestGetStatus, self.request) };
        self.last_status = status;
        if nt_ok(status) {
            for index in 0..4 {
                // SAFETY: ответ 1024 байта, четыре слова в его пределах.
                self.attach_words[index] = unsafe {
                    core::ptr::read_volatile(self.attach_out_ptr.add(index * 4).cast::<u32>())
                };
            }
        }
        unsafe { reuse_request(self.request) };
        status
    }

    /// Слово из ответа узла на запрос подключения.
    #[must_use]
    pub fn attach_word(&self, index: usize) -> u32 {
        self.attach_words.get(index).copied().unwrap_or(0)
    }

    /// Собирает элемент списка передач с указанным форматом буфера.
    fn entry_with_format(
        format: u32,
        direction: u32,
        buffer: *mut core::ffi::c_void,
        buffer_cb: u32,
    ) -> SpbTransferListEntry {
        let mut entry = entry_init(direction, buffer, buffer_cb);
        entry.buffer.format = format;
        entry
    }

    /// Отправляет управляющий запрос SPB без буферов и возвращает статус.
    ///
    /// Нужно, чтобы понять, какие запросы узел вообще поддерживает: по одному
    /// коду запроса на вызов. Статус пишется вызывающим в реестр.
    pub fn probe_ioctl(&mut self, code: u32) -> i32 {
        if self.request_lost {
            return -1;
        }
        // SAFETY: цель и запрос валидны; запрос без буферов.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfIoTargetFormatRequestForIoctl,
                self.target,
                self.request,
                code,
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                core::ptr::null_mut(),
            )
        };
        if !nt_ok(status) {
            self.last_status = status;
            return status;
        }
        let mut options: WDF_REQUEST_SEND_OPTIONS = unsafe { core::mem::zeroed() };
        options.Size = size_of_ulong::<WDF_REQUEST_SEND_OPTIONS>();
        options.Flags =
            (WDF_REQUEST_SEND_OPTION_SYNCHRONOUS | WDF_REQUEST_SEND_OPTION_TIMEOUT) as ULONG;
        options.Timeout = SPB_TIMEOUT_100NS;
        // SAFETY: синхронная отправка на пассивном уровне.
        let sent = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfRequestSend,
                self.request,
                self.target,
                &raw mut options,
            )
        };
        if sent == 0 {
            let _ = self.send_failed("I/O-таргет не завершил запрос");
            return -1;
        }
        // SAFETY: запрос завершён, читаем его статус.
        let status = unsafe { call_unsafe_wdf_function_binding!(WdfRequestGetStatus, self.request) };
        self.last_status = status;
        unsafe { reuse_request(self.request) };
        status
    }

    /// Пытается занять соединение (`IOCTL_SPB_LOCK_CONNECTION`).
    ///
    /// Это проверка цели: если узел отвечает успехом, значит перед нами
    /// настоящее SPB-соединение и дело в оформлении последовательности;
    /// если отказом — цель выбрана неверно.
    pub fn lock_connection(&mut self) -> i32 {
        if self.request_lost {
            return -1;
        }
        // SAFETY: цель и запрос валидны; запрос без буферов.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfIoTargetFormatRequestForIoctl,
                self.target,
                self.request,
                IOCTL_SPB_LOCK_CONNECTION,
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                core::ptr::null_mut(),
            )
        };
        if !nt_ok(status) {
            self.last_status = status;
            return status;
        }
        let mut options: WDF_REQUEST_SEND_OPTIONS = unsafe { core::mem::zeroed() };
        options.Size = size_of_ulong::<WDF_REQUEST_SEND_OPTIONS>();
        options.Flags = (WDF_REQUEST_SEND_OPTION_SYNCHRONOUS | WDF_REQUEST_SEND_OPTION_TIMEOUT) as ULONG;
        options.Timeout = SPB_TIMEOUT_100NS;
        // SAFETY: синхронная отправка на пассивном уровне.
        let sent = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfRequestSend,
                self.request,
                self.target,
                &raw mut options,
            )
        };
        if sent == 0 {
            let _ = self.send_failed("I/O-таргет не завершил запрос");
            return -1;
        }
        // SAFETY: запрос завершён, читаем его статус.
        let status = unsafe { call_unsafe_wdf_function_binding!(WdfRequestGetStatus, self.request) };
        self.last_status = status;
        unsafe { reuse_request(self.request) };
        status
    }

    /// Заполняет область передач под SPMI-регистр с 16-битным адресом.
    ///
    /// # Safety
    ///
    /// Область передач выделена размером [`TRANSFER_AREA`]; пассивный уровень.
    unsafe fn prepare_spmi16(
        &mut self,
        addr: u16,
        value: Option<u8>,
        big_endian: bool,
    ) -> Result<(), BusError> {
        if self.area.is_null() || self.data.is_null() {
            return Err(BusError::unsupported("буферы SPB не созданы"));
        }
        let list = self.area.cast::<SpbTransferList>();
        let write = value.is_some();
        let count: u32 = if write { 1 } else { 2 };
        self.last_count = count;
        let size_field = SpbTransferList::header_size();
        // SAFETY: запись заголовка списка в выделенную область.
        unsafe {
            (*list).size = u32::try_from(size_field).unwrap_or(0);
            (*list).reserved = 0;
            (*list).transfer_count = count;
        }
        let format = if self.variant == VARIANT_OUTPUT_NON_PAGED
            || self.variant == VARIANT_OUTPUT_MEMORY_NON_PAGED
        {
            SPB_FORMAT_SIMPLE_NON_PAGED
        } else {
            SPB_FORMAT_SIMPLE
        };
        let addr_bytes = if big_endian {
            addr.to_be_bytes()
        } else {
            addr.to_le_bytes()
        };
        let payload = self.data;
        let read_target = unsafe { self.data.add(2) };
        match value {
            Some(byte) => {
                // SAFETY: буфер данных ≥ 3 байт; одна передача «адрес + значение».
                unsafe {
                    core::ptr::write_volatile(payload, addr_bytes[0]);
                    core::ptr::write_volatile(payload.add(1), addr_bytes[1]);
                    core::ptr::write_volatile(payload.add(2), byte);
                    let entry = core::ptr::addr_of_mut!((*list).transfers[0]);
                    *entry = Self::entry_with_format(
                        format,
                        SPB_DIRECTION_TO_DEVICE,
                        payload.cast::<core::ffi::c_void>(),
                        3,
                    );
                }
            }
            None => {
                // SAFETY: адрес — 2 байта; вторая передача читает 1 байт.
                unsafe {
                    core::ptr::write_volatile(payload, addr_bytes[0]);
                    core::ptr::write_volatile(payload.add(1), addr_bytes[1]);
                    let first = core::ptr::addr_of_mut!((*list).transfers[0]);
                    *first = Self::entry_with_format(
                        format,
                        SPB_DIRECTION_TO_DEVICE,
                        payload.cast::<core::ffi::c_void>(),
                        2,
                    );
                    let second = self
                        .area
                        .add(SpbTransferList::header_size())
                        .cast::<SpbTransferListEntry>();
                    *second = Self::entry_with_format(
                        format,
                        SPB_DIRECTION_FROM_DEVICE,
                        read_target.cast::<core::ffi::c_void>(),
                        1,
                    );
                }
            }
        }
        Ok(())
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
        // Сколько передач объявляем и что пишем в поле `Size`.
        let write = value.is_some();
        let terminated = self.variant == VARIANT_TERMINATED;
        let count: u32 = match (write, terminated) {
            (true, false) => 1,
            (false, false) => 2,
            (true, true) => 2,
            (false, true) => 3,
        };
        self.last_count = count;
        // Поле `Size` ВСЕГДА равно `sizeof(SPB_TRANSFER_LIST)` — заголовок
        // вместе с одной записью, независимо от числа передач (см. `spb.h`:
        // «List size - must be set to sizeof(SPB_TRANSFER_LIST)»).
        let size_field = SpbTransferList::header_size();
        // SAFETY: запись заголовка списка.
        unsafe {
            (*list).size = u32::try_from(size_field).unwrap_or(0);
            (*list).reserved = 0;
            (*list).transfer_count = count;
        }

        // Байты лежат в буфере данных запроса: список передач занимает
        // входной буфер целиком, и данные внутри него уже не помещаются.
        let format =
            if self.variant == VARIANT_OUTPUT_NON_PAGED || self.variant == VARIANT_OUTPUT_MEMORY_NON_PAGED {
                SPB_FORMAT_SIMPLE_NON_PAGED
            } else {
                SPB_FORMAT_SIMPLE
            };
        let payload = self.data;
        // Байт ответа идёт сразу за адресом в том же буфере.
        // SAFETY: выходной буфер 8 байт, второй байт в его пределах.
        let read_target = unsafe { self.data.add(1) };
        match value {
            Some(byte) => {
                // Запись: одна передача «адрес + значение».
                let mut frame = [addr, byte];
                // SAFETY: копируем два байта в выделенную область.
                unsafe {
                    core::ptr::copy_nonoverlapping(frame.as_mut_ptr(), payload, 2);
                    let entry = core::ptr::addr_of_mut!((*list).transfers[0]);
                    *entry = Self::entry_with_format(
                        format,
                        SPB_DIRECTION_TO_DEVICE,
                        payload.cast::<core::ffi::c_void>(),
                        2,
                    );
                }
                // Вторая передача не нужна: список объявляет одну передачу,
                // и его буфер имеет ровно эту длину.
            }
            None => {
                // Чтение: запись адреса, затем чтение байта.
                // SAFETY: пишем адрес в область данных.
                unsafe {
                    core::ptr::write_volatile(payload, addr);
                    let first = core::ptr::addr_of_mut!((*list).transfers[0]);
                    *first = Self::entry_with_format(
                        format,
                        SPB_DIRECTION_TO_DEVICE,
                        payload.cast::<core::ffi::c_void>(),
                        1,
                    );
                    let second = self
                        .area
                        .add(SpbTransferList::header_size())
                        .cast::<SpbTransferListEntry>();
                    *second = Self::entry_with_format(
                        format,
                        SPB_DIRECTION_FROM_DEVICE,
                        read_target.cast::<core::ffi::c_void>(),
                        1,
                    );
                }
            }
        }
        if terminated {
            // Завершающий элемент с направлением `None`: это единственный
            // элемент ABI, который мы раньше не использовали вообще.
            // SAFETY: элемент лежит в пределах выделенной области.
            unsafe {
                let offset = SpbTransferList::header_size()
                    + usize::try_from(count).unwrap_or(2).saturating_sub(2)
                        * SpbTransferList::entry_size();
                let tail = self.area.add(offset).cast::<SpbTransferListEntry>();
                *tail =
                    Self::entry_with_format(format, SPB_DIRECTION_NONE, core::ptr::null_mut(), 0);
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
            call_unsafe_wdf_function_binding!(WdfIoTargetClose, self.target);
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

/// Максимальная длина имени объекта в пробе (в символах).
pub const PROBE_NAME_CHARS: usize = 64;

/// Совместный доступ: чтение + запись + удаление (`FILE_SHARE_READ|WRITE|DELETE`).
pub const PROBE_SHARE_ALL: u32 = 0x0000_0007;

/// Проба: открывается ли объект ядра с указанным именем.
///
/// Нужна для диагностики: проверяем, есть ли в пространстве имён ядра объект
/// шины SPMI (например `\Device\Spmi\SUPERUSER`) или символьная ссылка
/// штатного PMIC/ADC (`\DosDevices\Global\QCOMPMIC`, `\??\QCOM_ADC`).
/// Из пользовательского режима часть из них не видна, а драйвер открывает их
/// тем же способом, что и узел ресурсов — по имени через `WdfIoTargetOpenByName`.
///
/// Цель после пробы закрывается и удаляется: иначе успешное открытие держало бы
/// исключающую ссылку на чужой стек (SUPERUSER / ADC).
///
/// Имена — только ASCII: буфер заполняется побайтово.
///
/// # Safety
///
/// Пассивный уровень IRQL, устройство создано и не удаляется.
pub unsafe fn probe_named_target(device: WDFDEVICE, name: &str, desired_access: u32) -> i32 {
    // SAFETY: пассивный уровень; делегируем общей пробе с нулевым ShareAccess.
    unsafe { probe_named_target_ex(device, name, desired_access, 0) }
}

/// Проба открытия объекта ядра с явной маской совместного доступа.
///
/// # Safety
///
/// Пассивный уровень IRQL, устройство создано и не удаляется.
pub unsafe fn probe_named_target_ex(
    device: WDFDEVICE,
    name: &str,
    desired_access: u32,
    share_access: u32,
) -> i32 {
    let mut chars = [0_u16; PROBE_NAME_CHARS];
    let mut length = 0_usize;
    for byte in name.bytes() {
        if let Some(slot) = chars.get_mut(length) {
            *slot = u16::from(byte);
            length = length.saturating_add(1);
        }
    }
    let mut io_target: WDFIOTARGET = WDF_NO_HANDLE.cast();
    // SAFETY: устройство создано; дескриптор — локальная переменная.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfIoTargetCreate,
            device,
            WDF_NO_OBJECT_ATTRIBUTES,
            &raw mut io_target,
        )
    };
    if !nt_ok(status) {
        return status;
    }
    let bytes = u16::try_from(length.saturating_mul(2)).unwrap_or(0);
    let mut params: WDF_IO_TARGET_OPEN_PARAMS = unsafe { core::mem::zeroed() };
    params.Size = size_of_ulong::<WDF_IO_TARGET_OPEN_PARAMS>();
    params.Type = WdfIoTargetOpenByName;
    params.TargetDeviceName = UNICODE_STRING {
        Length: bytes,
        MaximumLength: bytes,
        Buffer: chars.as_mut_ptr(),
    };
    params.DesiredAccess = desired_access;
    params.ShareAccess = share_access;
    params.CreateDisposition = wdk_sys::FILE_OPEN;
    params.FileAttributes = wdk_sys::FILE_ATTRIBUTE_NORMAL;
    // Как у qcpmic8150: FILE_NON_DIRECTORY_FILE.
    params.CreateOptions = 0x0000_0040;
    // SAFETY: цель создана, параметры заполнены, уровень пассивный.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(WdfIoTargetOpen, io_target, &raw mut params)
    };
    // Всегда закрываем цель: и при успехе (не держим чужой стек), и при отказе
    // (иначе WdfIoTargetCreate оставляет незакрытый объект).
    // SAFETY: цель создана выше; пассивный уровень.
    unsafe {
        if nt_ok(status) {
            call_unsafe_wdf_function_binding!(WdfIoTargetClose, io_target);
        }
        call_unsafe_wdf_function_binding!(WdfObjectDelete, io_target.cast());
    }
    status
}

/// Результат пробы SUPERUSER: статус открытия и байт APSD (если чтение удалось).
#[derive(Debug, Clone, Copy)]
pub struct SuperuserApsdProbe {
    /// `NTSTATUS` открытия `\Device\Spmi\SUPERUSER`.
    pub open_status: i32,
    /// `NTSTATUS` peri-grant `0x13` (или `0xFFFFFFFF`, если open не удался).
    pub grant_status: i32,
    /// `NTSTATUS` чтения `0x1307` (или `0xFFFFFFFF`, если open/grant отсёк путь).
    pub read_status: i32,
    /// Значение `APSD_STATUS`, если `read_status == 0`.
    pub value: u8,
}

/// Открывает `\Device\Spmi\SUPERUSER`, выдаёт peri `0x13`, читает `APSD_STATUS` (`0x1307`).
///
/// Слот SUPERUSER ограничен тремя одновременными открытиями (`qcpmic` /
/// `qcpmicext` / `qcpmgpio`). Если все три заняты, open вернёт `0xC0000001`.
/// При свободном слоте (после reboot/disable одного клиента) путь работает.
///
/// # Safety
///
/// Пассивный уровень IRQL, устройство создано и не удаляется.
pub unsafe fn probe_superuser_apsd(device: WDFDEVICE) -> SuperuserApsdProbe {
    let mut out = SuperuserApsdProbe {
        open_status: -1,
        grant_status: -1_i32,
        read_status: -1_i32,
        value: 0,
    };
    let mut chars = [0_u16; PROBE_NAME_CHARS];
    let name = "\\Device\\Spmi\\SUPERUSER";
    let mut length = 0_usize;
    for byte in name.bytes() {
        if let Some(slot) = chars.get_mut(length) {
            *slot = u16::from(byte);
            length = length.saturating_add(1);
        }
    }
    let mut io_target: WDFIOTARGET = WDF_NO_HANDLE.cast();
    // SAFETY: устройство создано; дескриптор локальный.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfIoTargetCreate,
            device,
            WDF_NO_OBJECT_ATTRIBUTES,
            &raw mut io_target,
        )
    };
    if !nt_ok(status) {
        out.open_status = status;
        return out;
    }
    let bytes = u16::try_from(length.saturating_mul(2)).unwrap_or(0);
    let mut params: WDF_IO_TARGET_OPEN_PARAMS = unsafe { core::mem::zeroed() };
    params.Size = size_of_ulong::<WDF_IO_TARGET_OPEN_PARAMS>();
    params.Type = WdfIoTargetOpenByName;
    params.TargetDeviceName = UNICODE_STRING {
        Length: bytes,
        MaximumLength: bytes,
        Buffer: chars.as_mut_ptr(),
    };
    // Как qcpmic8150: GENERIC_READ|GENERIC_WRITE, share all, FILE_NON_DIRECTORY_FILE.
    params.DesiredAccess = 0xC000_0000;
    params.ShareAccess = PROBE_SHARE_ALL;
    params.CreateDisposition = wdk_sys::FILE_OPEN;
    params.FileAttributes = wdk_sys::FILE_ATTRIBUTE_NORMAL;
    params.CreateOptions = 0x0000_0040;
    // SAFETY: цель создана, пассивный уровень.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(WdfIoTargetOpen, io_target, &raw mut params)
    };
    out.open_status = status;
    if !nt_ok(status) {
        unsafe {
            call_unsafe_wdf_function_binding!(WdfObjectDelete, io_target.cast());
        }
        return out;
    }

    // peri-grant: u16 count=1, u16 peri=0x0013
    let grant = [1_u8, 0, 0x13, 0];
    let mut request: WDFREQUEST = WDF_NO_HANDLE.cast();
    let mut in_mem: WDFMEMORY = WDF_NO_HANDLE.cast();
    let mut in_ptr: *mut core::ffi::c_void = core::ptr::null_mut();
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfRequestCreate,
            WDF_NO_OBJECT_ATTRIBUTES,
            io_target,
            &raw mut request,
        )
    };
    if !nt_ok(status) {
        out.grant_status = status;
        unsafe {
            call_unsafe_wdf_function_binding!(WdfIoTargetClose, io_target);
            call_unsafe_wdf_function_binding!(WdfObjectDelete, io_target.cast());
        }
        return out;
    }
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfMemoryCreate,
            WDF_NO_OBJECT_ATTRIBUTES,
            wdk_sys::_POOL_TYPE::NonPagedPool,
            0,
            grant.len(),
            &raw mut in_mem,
            &raw mut in_ptr,
        )
    };
    if !nt_ok(status) {
        out.grant_status = status;
        unsafe {
            call_unsafe_wdf_function_binding!(WdfObjectDelete, request.cast());
            call_unsafe_wdf_function_binding!(WdfIoTargetClose, io_target);
            call_unsafe_wdf_function_binding!(WdfObjectDelete, io_target.cast());
        }
        return out;
    }
    unsafe {
        core::ptr::copy_nonoverlapping(grant.as_ptr(), in_ptr.cast::<u8>(), grant.len());
    }
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfIoTargetFormatRequestForIoctl,
            io_target,
            request,
            IOCTL_SPMI_SUPERUSER_GRANT,
            in_mem,
            core::ptr::null_mut(),
            WDF_NO_HANDLE.cast(),
            core::ptr::null_mut(),
        )
    };
    if nt_ok(status) {
        let mut options: WDF_REQUEST_SEND_OPTIONS = unsafe { core::mem::zeroed() };
        options.Size = size_of_ulong::<WDF_REQUEST_SEND_OPTIONS>();
        options.Flags =
            (WDF_REQUEST_SEND_OPTION_SYNCHRONOUS | WDF_REQUEST_SEND_OPTION_TIMEOUT) as ULONG;
        options.Timeout = SPB_TIMEOUT_100NS;
        let sent = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfRequestSend,
                request,
                io_target,
                &raw mut options,
            )
        };
        out.grant_status = if sent == 0 {
            -1
        } else {
            unsafe { call_unsafe_wdf_function_binding!(WdfRequestGetStatus, request) }
        };
    } else {
        out.grant_status = status;
    }
    unsafe {
        call_unsafe_wdf_function_binding!(WdfObjectDelete, in_mem.cast());
        call_unsafe_wdf_function_binding!(WdfObjectDelete, request.cast());
    }

    // READ APSD_STATUS: header + 1-byte output. addr_enc = (sid2 << 16) | 0x1307
    let mut header = [0_u8; SPMI_SUPERUSER_HEADER_LEN];
    header[4] = 0x07;
    header[5] = 0x13;
    header[6] = 0x02;
    header[7] = 0x00;
    header[8] = 0x01;
    let mut request: WDFREQUEST = WDF_NO_HANDLE.cast();
    let mut in_mem: WDFMEMORY = WDF_NO_HANDLE.cast();
    let mut out_mem: WDFMEMORY = WDF_NO_HANDLE.cast();
    let mut in_ptr: *mut core::ffi::c_void = core::ptr::null_mut();
    let mut out_ptr: *mut core::ffi::c_void = core::ptr::null_mut();
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfRequestCreate,
            WDF_NO_OBJECT_ATTRIBUTES,
            io_target,
            &raw mut request,
        )
    };
    if !nt_ok(status) {
        out.read_status = status;
        unsafe {
            call_unsafe_wdf_function_binding!(WdfIoTargetClose, io_target);
            call_unsafe_wdf_function_binding!(WdfObjectDelete, io_target.cast());
        }
        return out;
    }
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfMemoryCreate,
            WDF_NO_OBJECT_ATTRIBUTES,
            wdk_sys::_POOL_TYPE::NonPagedPool,
            0,
            header.len(),
            &raw mut in_mem,
            &raw mut in_ptr,
        )
    };
    if !nt_ok(status) {
        out.read_status = status;
        unsafe {
            call_unsafe_wdf_function_binding!(WdfObjectDelete, request.cast());
            call_unsafe_wdf_function_binding!(WdfIoTargetClose, io_target);
            call_unsafe_wdf_function_binding!(WdfObjectDelete, io_target.cast());
        }
        return out;
    }
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfMemoryCreate,
            WDF_NO_OBJECT_ATTRIBUTES,
            wdk_sys::_POOL_TYPE::NonPagedPool,
            0,
            1,
            &raw mut out_mem,
            &raw mut out_ptr,
        )
    };
    if !nt_ok(status) {
        out.read_status = status;
        unsafe {
            call_unsafe_wdf_function_binding!(WdfObjectDelete, in_mem.cast());
            call_unsafe_wdf_function_binding!(WdfObjectDelete, request.cast());
            call_unsafe_wdf_function_binding!(WdfIoTargetClose, io_target);
            call_unsafe_wdf_function_binding!(WdfObjectDelete, io_target.cast());
        }
        return out;
    }
    unsafe {
        core::ptr::copy_nonoverlapping(header.as_ptr(), in_ptr.cast::<u8>(), header.len());
        core::ptr::write_volatile(out_ptr.cast::<u8>(), 0xFF);
    }
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfIoTargetFormatRequestForIoctl,
            io_target,
            request,
            IOCTL_SPMI_SUPERUSER_READ,
            in_mem,
            core::ptr::null_mut(),
            out_mem,
            core::ptr::null_mut(),
        )
    };
    if nt_ok(status) {
        let mut options: WDF_REQUEST_SEND_OPTIONS = unsafe { core::mem::zeroed() };
        options.Size = size_of_ulong::<WDF_REQUEST_SEND_OPTIONS>();
        options.Flags =
            (WDF_REQUEST_SEND_OPTION_SYNCHRONOUS | WDF_REQUEST_SEND_OPTION_TIMEOUT) as ULONG;
        options.Timeout = SPB_TIMEOUT_100NS;
        let sent = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfRequestSend,
                request,
                io_target,
                &raw mut options,
            )
        };
        out.read_status = if sent == 0 {
            -1
        } else {
            unsafe { call_unsafe_wdf_function_binding!(WdfRequestGetStatus, request) }
        };
        if nt_ok(out.read_status) {
            out.value = unsafe { core::ptr::read_volatile(out_ptr.cast::<u8>()) };
        }
    } else {
        out.read_status = status;
    }
    unsafe {
        call_unsafe_wdf_function_binding!(WdfObjectDelete, out_mem.cast());
        call_unsafe_wdf_function_binding!(WdfObjectDelete, in_mem.cast());
        call_unsafe_wdf_function_binding!(WdfObjectDelete, request.cast());
        call_unsafe_wdf_function_binding!(WdfIoTargetClose, io_target);
        call_unsafe_wdf_function_binding!(WdfObjectDelete, io_target.cast());
    }
    out
}

/// SID PM8150B USBIN (charger peripheral on SPMI).
pub const SPMI_SID_USBIN: u8 = 2;
/// Peri-grant id for USBIN (`0x13` — matches register bank `0x13xx`).
pub const SPMI_PERI_USBIN: u16 = 0x0013;
/// Peri-grant id периферии `batt_soc` PM8150B (топливный счётчик, банк `0x40xx`).
pub const SPMI_PERI_BATT_SOC: u16 = 0x0040;
/// `BATT_SOC_SUBTYPE`: у PM8150B равен `0x10` (`FG_BATT_SOC_PM8150B`).
pub const SPMI_REG_BATT_SOC_SUBTYPE: u16 = 0x4005;
/// `FG_MONOTONIC_SOC` — сырой заряд, 8 бит, `0…255` (255 = 100 %).
///
/// Теневая копия значения лежит следом (`+0x0A`); читаются они одним запросом
/// двух байт, как в `fg_get_msoc_raw`, и обязаны совпадать.
pub const SPMI_REG_BATT_SOC: u16 = 0x4009;
/// Ожидаемый subtype периферии `batt_soc` на PM8150B.
pub const SPMI_BATT_SOC_SUBTYPE_PM8150B: u8 = 0x10;

/// Читает сырой заряд из топливного счётчика PM8150B.
///
/// Android берёт то же значение в `fg_get_msoc_raw` (`drivers_power_supply_qcom_fg-util.c`):
/// читает **два** байта с `FG_MONOTONIC_SOC` и требует их равенства (до пяти
/// попыток), потому что это теневые регистры одного числа, и расхождение
/// означает, что счётчик обновляется прямо сейчас. Здесь та же проверка: при
/// расхождении чтение считается неудачным, а не поводом угадывать — значение
/// придёт на следующем такте телеметрии.
///
/// Subtype читается в том же открытии: по адресу `0x4000` на другой платформе
/// может оказаться иная периферия, и тогда `0x4009` — не заряд.
///
/// # Safety
///
/// `PASSIVE_LEVEL`; `device` жив.
pub unsafe fn read_batt_soc_raw(device: WDFDEVICE) -> Option<u8> {
    // SAFETY: пассивный уровень, устройство создано.
    let mut su = unsafe { SuperuserBus::open(device) }.ok()?;
    su.grant(SPMI_PERI_BATT_SOC).ok()?;
    let subtype = su.read_u8(SPMI_SID_USBIN, SPMI_REG_BATT_SOC_SUBTYPE).ok()?;
    if subtype != SPMI_BATT_SOC_SUBTYPE_PM8150B {
        return None;
    }
    let mut cap = [0_u8; 2];
    su.read_bytes(SPMI_SID_USBIN, SPMI_REG_BATT_SOC, &mut cap)
        .ok()?;
    // Вторая ячейка — копия; она читается по `+0x0A`, но `read_bytes` вернул
    // оба байта одним запросом, потому что регистры идут подряд.
    if cap[0] != cap[1] {
        return None;
    }
    Some(cap[0])
}

/// Peri-grant id периферии CHGR (`0x10` — банк `0x10xx`).
///
/// Значение не выведено из адреса, а взято из вендорного DT nabu
/// (`android_kernel_xiaomi_nabu/arch/arm64/boot/dts/qcom/pm8150b.dtsi:190-191`):
/// узел `qcom,chgr@1000`, `reg = <0x1000 0x100>`. Тот же базовый адрес в шапке
/// карты регистров SMB5 — `drivers_power_supply_qcom_smb5-reg.h:18`
/// (`CHGR_BASE 0x1000`).
pub const SPMI_PERI_CHGR: u16 = 0x0010;

/// Peri-grant id периферии `batt_info` (`0x41` — банк `0x41xx`).
///
/// Ток банки лежит не в `batt_soc` (`0x4000`, откуда драйвер берёт процент), а
/// в соседней периферии `batt_info`: в том же DT `qcom,fg-batt-info@4100` с
/// `reg = <0x4100 0x100>` (там же, строки 413-415), а вендорный разбор
/// присваивает базовый адрес по subtype периферии —
/// `android_kernel_xiaomi_nabu/drivers/power/supply/qcom/qpnp-fg-gen4.c:6704-6705`
/// (`case FG_BATT_INFO_PM8150B: fg->batt_info_base = base;`).
pub const SPMI_PERI_BATT_INFO: u16 = 0x0041;

/// `CHGR_FAST_CHARGE_CURRENT_CFG_REG` — FCC бака (buck) SMB5, 50 мА/разряд.
///
/// Адрес: `drivers_power_supply_qcom_smb5-reg.h:79` (`CHGR_BASE + 0x61`). Шаг и
/// потолок — параметры PM8150B из `drivers_power_supply_qcom_qpnp-smb5.c:128-134`
/// (`.min_u = 0`, `.max_u = 8 000 000`, `.step_u = 50 000`), то есть сырое
/// `0x14` = 1,00 А.
pub const SPMI_REG_CHGR_FCC: u16 = 0x1061;

/// `CHARGING_ENABLE_CMD_REG`, бит 0 — команда «заряд разрешён»
/// (`drivers_power_supply_qcom_smb5-reg.h:67-68`).
pub const SPMI_REG_CHGR_CHARGING_ENABLE: u16 = 0x1042;

/// `CHGR_CFG2_REG`, бит 0 `CHARGER_INHIBIT_BIT` — аппаратный запрет заряда
/// (`drivers_power_supply_qcom_smb5-reg.h:73-77`).
pub const SPMI_REG_CHGR_CFG2: u16 = 0x1051;

/// `BATTERY_CHARGER_STATUS_1_REG`, биты `[2:0]` — фаза заряда
/// (`drivers_power_supply_qcom_smb5-reg.h:36-45`).
pub const SPMI_REG_CHGR_STATUS_1: u16 = 0x1006;

/// `CHGR_FLOAT_VOLTAGE_CFG_REG` — напряжение окончания заряда, 10 мВ/разряд от
/// 3,6 В (`drivers_power_supply_qcom_smb5-reg.h:95`; шаг —
/// `drivers_power_supply_qcom_qpnp-smb5.c:136-141`).
pub const SPMI_REG_CHGR_FLOAT_VOLTAGE: u16 = 0x1070;

/// `USBIN_CURRENT_LIMIT_CFG_REG` — предел входного тока USBIN, 50 мА/разряд
/// (`drivers_power_supply_qcom_smb5-reg.h:322`; шаг —
/// `drivers_power_supply_qcom_qpnp-smb5.c:143-148`).
pub const SPMI_REG_USBIN_ICL: u16 = 0x1370;

/// `USBIN_ADAPTER_ALLOW_CFG_REG` — какие напряжения разрешены адаптеру
/// (`drivers_power_supply_qcom_smb5-reg.h:285`).
pub const SPMI_REG_USBIN_ADAPTER_ALLOW: u16 = 0x1360;

/// `BATT_INFO_IBATT_LSB` — младший байт тока банки (16 бит, LE, знак — бит 15).
///
/// Смещение задано в `drivers_power_supply_qcom_fg-reg.h:250-251`
/// (`batt_info_base + 0xA2`/`+0xA3`); базовый адрес этой периферии на nabu —
/// `0x4100` (см. [`SPMI_PERI_BATT_INFO`]).
pub const SPMI_REG_FG_IBATT_LSB: u16 = 0x41A2;

/// Теневая копия того же тока (`BATT_INFO_IBATT_LSB_CP`,
/// `drivers_power_supply_qcom_fg-reg.h:261`).
///
/// Вендор читает обе пары и требует их равенства
/// (`drivers_power_supply_qcom_fg-util.c:1005-1027`, сравнение на `:1020`):
/// пара обновляется счётчиком целиком, и расхождение означает, что чтение
/// попало внутрь обновления.
pub const SPMI_REG_FG_IBATT_LSB_CP: u16 = 0x41A8;

/// `BATT_INFO_VBATT_LSB` — младший байт напряжения банки (16 бит, LE,
/// беззнаковое).
///
/// Смещение задано в `drivers_power_supply_qcom_fg-reg.h:246-247`
/// (`batt_info_base + 0xA0`/`+0xA1`), та же периферия `0x4100`, что и у тока.
pub const SPMI_REG_FG_VBATT_LSB: u16 = 0x41A0;

/// Теневая копия того же напряжения (`BATT_INFO_VBATT_LSB_CP`,
/// `drivers_power_supply_qcom_fg-reg.h:259-260`, `batt_info_base + 0xA6`).
///
/// Проверено по шапке: адрес совпадает с тем, что читает вендор в
/// `drivers_power_supply_qcom_fg-util.c:1057`, и относится к той же паре
/// v2.0+, что и [`SPMI_REG_FG_IBATT_LSB_CP`]. Условие равенства — там же
/// на `:1064`, и оно ровно то же, что у тока.
pub const SPMI_REG_FG_VBATT_LSB_CP: u16 = 0x41A6;

/// Числитель шага напряжения банки: `V[мкВ] = raw * 122070 / 1000`.
///
/// Взято из вендорного декодера `drivers_power_supply_qcom_fg-util.c:1041-1042`
/// (`BATT_VOLTAGE_NUMR 122070`, `BATT_VOLTAGE_DENR 1000`) и его применения там
/// же на `:1079`; результат идёт прямо в `POWER_SUPPLY_PROP_VOLTAGE_NOW`
/// (`drivers_power_supply_qcom_qpnp-fg-gen4.c:5184-5188`), а эта величина в
/// power_supply — микровольты (там же `:5131`, `vbatt_uv/1000` — милливольты).
/// Итого 122,07 мкВ на разряд, около 8 мВ на весь диапазон банки.
pub const FG_VBATT_NUMER: u32 = 122_070;

/// Знаменатель шага напряжения банки (`BATT_VOLTAGE_DENR`,
/// `drivers_power_supply_qcom_fg-util.c:1042`).
pub const FG_VBATT_DENOM: u32 = 1_000;

/// Числитель шага тока банки: `I[мкА] = raw * 488281 / 1000`.
///
/// Взято из вендорного декодера `drivers_power_supply_qcom_fg-util.c:997-998`
/// (`BATT_CURRENT_NUMR 488281`, `BATT_CURRENT_DENR 1000`) и его применения там
/// же на `:1036-1037` (`sign_extend32(temp, 15)`, затем
/// `temp * BATT_CURRENT_NUMR / BATT_CURRENT_DENR`); результат идёт прямо в
/// `POWER_SUPPLY_PROP_CURRENT_NOW`
/// (`android_kernel_xiaomi_nabu/drivers/power/supply/qcom/qpnp-fg-gen4.c:5190-5191`),
/// а эта величина в power_supply — микроамперы. Итого 1/2048 А на разряд.
pub const FG_IBATT_NUMER: i32 = 488_281;

/// Знаменатель шага тока банки (`BATT_CURRENT_DENR`,
/// `drivers_power_supply_qcom_fg-util.c:998`).
pub const FG_IBATT_DENOM: i32 = 1_000;

/// Снимок регистров SMB5 (PM8150B) и тока банки — для диагностики `Chgr*`/`FgIbatUa`.
///
/// Поля — сырые байты как они лежат в SPMI: декодирование оставлено разбору
/// журнала, потому что шаги и смещения у каждой величины свои (см. константы
/// выше), а цена ошибки в декодере выше пользы.
#[derive(Debug, Clone, Copy)]
pub struct ChargeRegs {
    /// FCC бака (`0x1061`), 50 мА/разряд.
    pub fcc_raw: u8,
    /// Команда «заряд разрешён» (`0x1042`), бит 0.
    pub charge_enable: u8,
    /// Аппаратный запрет заряда (`0x1051`), бит 0.
    pub inhibit: u8,
    /// Фаза заряда (`0x1006`), биты `[2:0]`.
    pub chgr_status: u8,
    /// Напряжение окончания заряда (`0x1070`), 10 мВ/разряд от 3,6 В.
    pub fv_raw: u8,
    /// Предел входного тока USBIN (`0x1370`), 50 мА/разряд.
    pub icl_raw: u8,
    /// Разрешённые адаптеру напряжения (`0x1360`).
    pub usbin_allow: u8,
    /// Ток банки из топливного счётчика, мкА. Знак — как у вендора (бит 15
    /// сырого значения): **отрицательный — ток в банку (заряд)**, положительный —
    /// разряд. В этой `u32` отрицательная величина лежит дополнительным кодом,
    /// поэтому потребителю нужен модуль, а не само число:
    /// `(ibatt_ua as i32).unsigned_abs()`.
    ///
    /// Так же читает знак вендор: `qcom/smb5-lib.c` (ветка 16.0) считает банку
    /// заряжающейся при `ibat < -450 mA`, а `ti/cp_qc30.c` перед употреблением
    /// меняет знак тока счётчика. На живом планшете поле отрицательно именно
    /// тогда, когда банка достоверно заряжается, — «минус — это заряд».
    pub ibatt_ua: u32,
    /// Напряжение банки из топливного счётчика PM8150B, мкВ. **Беззнаковое**:
    /// знака у этой пары нет, в отличие от тока.
    ///
    /// Это независимый от LN8000 отсчёт банки. Нужен он потому, что
    /// [`ChargeRegs`] — единственное место, где видно, чему равно `vbat` на
    /// самом деле: `AdcChannel::Vbat` у LN8000 во время 2:1 меряет середину
    /// шины преобразователя (≈ Vin/2), а не банку, — а от `vbat` считается вся
    /// полоса переноса (см. `ln8000::encoding::window_target_uv`). Ошибка в
    /// `vbat` сдвигает полосу вдвое.
    ///
    /// Разряд весит 122,07 мкВ, и в `u32` полный диапазон 16 бит (0…65535)
    /// помещается с запасом — в отличие от тока, отрицательных значений здесь
    /// не бывает, поэтому дополнительного кода не нужно.
    pub fg_vbatt_uv: u32,
}

/// Читает регистры SMB5 (PM8150B) и ток банки из топливного счётчика.
///
/// # Зачем
///
/// На живом планшете насос держит 2:1 при входе ~8,6 В и ~0,55 А, то есть
/// переносит в узел ~1,1 А, а заряд банки растёт так, будто туда приходит
/// около 2 А. Вторая зарядная ветка платформы — бак (buck) SMB5 (PM8150B) —
/// драйвером не настраивается: её FCC остался таким, каким его оставила
/// прошивка, и до сих пор был невидим. Тока банки у LN8000 нет вовсе — его
/// меряет только топливный счётчик PM8150B. Обе величины читаются по SPMI тем
/// же путём, каким драйвер берёт процент ([`read_batt_soc_raw`]), и это
/// единственный способ отличить «насос отдаёт ток в банку» от «ток уходит в бак
/// SMB5».
///
/// # Что делает
///
/// Одна сессия SUPERUSER: открытие, три grant (CHGR, USBIN, `batt_info`), чтение
/// регистров, закрытие в [`Drop`]. Ни одной записи. Любой отказ чтения —
/// `None`: частично заполненный снимок выглядел бы как валидный, а по нему
/// потом принимают решения.
///
/// # Safety
///
/// `PASSIVE_LEVEL`; `device` жив.
pub unsafe fn read_charge_regs(device: WDFDEVICE) -> Option<ChargeRegs> {
    // SAFETY: пассивный уровень, устройство создано.
    let mut su = unsafe { SuperuserBus::open(device) }.ok()?;
    su.grant(SPMI_PERI_CHGR).ok()?;
    su.grant(SPMI_PERI_USBIN).ok()?;
    su.grant(SPMI_PERI_BATT_INFO).ok()?;
    let fcc_raw = su.read_u8(SPMI_SID_USBIN, SPMI_REG_CHGR_FCC).ok()?;
    let charge_enable = su
        .read_u8(SPMI_SID_USBIN, SPMI_REG_CHGR_CHARGING_ENABLE)
        .ok()?;
    let inhibit = su.read_u8(SPMI_SID_USBIN, SPMI_REG_CHGR_CFG2).ok()?;
    let chgr_status = su.read_u8(SPMI_SID_USBIN, SPMI_REG_CHGR_STATUS_1).ok()?;
    let fv_raw = su
        .read_u8(SPMI_SID_USBIN, SPMI_REG_CHGR_FLOAT_VOLTAGE)
        .ok()?;
    let icl_raw = su.read_u8(SPMI_SID_USBIN, SPMI_REG_USBIN_ICL).ok()?;
    let usbin_allow = su
        .read_u8(SPMI_SID_USBIN, SPMI_REG_USBIN_ADAPTER_ALLOW)
        .ok()?;
    let mut ibatt = [0_u8; 2];
    su.read_bytes(SPMI_SID_USBIN, SPMI_REG_FG_IBATT_LSB, &mut ibatt)
        .ok()?;
    let mut ibatt_cp = [0_u8; 2];
    su.read_bytes(SPMI_SID_USBIN, SPMI_REG_FG_IBATT_LSB_CP, &mut ibatt_cp)
        .ok()?;
    // Как в `read_batt_soc_raw` и в `fg_get_battery_current`: расхождение пары и
    // копии — не повод угадывать, значение придёт на следующем такте.
    if ibatt != ibatt_cp {
        return None;
    }
    // Напряжение банки — из той же периферии и той же сессией: у вендора обе
    // величины читаются одним и тем же циклом с одной и той же проверкой тени
    // (`drivers_power_supply_qcom_fg-util.c:1005-1027` для тока и `:1049-1071`
    // для напряжения). Порядок байт и отсутствие знака — по `:1077`
    // (`temp = buf[1] << 8 | buf[0]`), ветка `PMI8998_V1_REV_WA` (`:1073`) к
    // nabu не относится: на PM8150B флаг не выставляется вовсе — в
    // `qpnp-fg-gen4.c` он не упоминается, а ставит его только `qpnp-fg-gen3.c`
    // для `PMI8998_SUBTYPE`.
    let mut vbatt = [0_u8; 2];
    su.read_bytes(SPMI_SID_USBIN, SPMI_REG_FG_VBATT_LSB, &mut vbatt)
        .ok()?;
    let mut vbatt_cp = [0_u8; 2];
    su.read_bytes(SPMI_SID_USBIN, SPMI_REG_FG_VBATT_LSB_CP, &mut vbatt_cp)
        .ok()?;
    // Та же цена расхождения, что и у тока: снимок неполон, значит его нет.
    if vbatt != vbatt_cp {
        return None;
    }
    // Младший байт лежит в 0x41A0, старший в 0x41A1 (`temp = buf[1] << 8 | buf[0]`
    // в `drivers_power_supply_qcom_fg-util.c:1077`); знака нет — вендор здесь
    // `sign_extend32` не зовёт, в отличие от тока (`:1036`).
    let raw_v = u32::from(u16::from_le_bytes(vbatt));
    // Произведение считаем в `u64`: 65535 разрядов это 8,0 В в микровольтах,
    // и в `u32` оно не влезает — насыщающее умножение дало бы неверное
    // напряжение на верхнем разряде.
    let micro_uv = u64::from(raw_v) * u64::from(FG_VBATT_NUMER) / u64::from(FG_VBATT_DENOM);
    // Младший байт лежит в 0x41A2, старший в 0x41A3 (`temp = buf[1] << 8 | buf[0]`
    // в `drivers_power_supply_qcom_fg-util.c:1033`), знак — бит 15 (там же, `:1036`).
    let raw = i32::from(i16::from_le_bytes(ibatt));
    // Произведение считаем в `i64`: 32767 разрядов это 16 А, и в `i32` оно не
    // влезает — насыщающее умножение здесь дало бы неверный ток на разряде.
    let micro_ua = i64::from(raw) * i64::from(FG_IBATT_NUMER) / i64::from(FG_IBATT_DENOM);
    Some(ChargeRegs {
        fcc_raw,
        charge_enable,
        inhibit,
        chgr_status,
        fv_raw,
        icl_raw,
        usbin_allow,
        // Отрицательный ток сохраняет знак: марка `u32` несёт его дополнительным кодом.
        ibatt_ua: (micro_ua as i32) as u32,
        // Напряжение банки знака не имеет и в дополнительный код не переводится.
        fg_vbatt_uv: micro_uv as u32,
    })
}

/// Пишет FCC бака (buck) SMB5 (PM8150B) и возвращает то, что **прочитано
/// обратно** из регистра `0x1061`.
///
/// # Зачем
///
/// Живой замер 19.09 (сборка .652, MDY-08-EI): `ChgrFccRaw = 30`, то есть
/// 1,50 А — столько оставила прошивка, наш драйвер этот регистр не писал ни
/// разу; при этом `FgIbatUa` ≈ 2,9 А, `SysStsRaw = 0x04` (насос в 2:1 без
/// петель `IIN_LOOP`/`VFLOAT_LOOP`, то есть отдаёт сколько дают). Бак
/// PM8150B — вторая зарядная ветка платформы, и его 1,5 А лежат далеко ниже
/// того, что разрешает вендорный DT планшета: `qcom,fcc-max-ua = <5900000>`
/// (`arch/arm64/boot/dts/qcom/xiaomi/overlay/nabu/nabu-sm8150.dtsi:68`).
/// Поднять FCC — это ровно то, что делает Android, когда работает насос, и
/// единственный регистр, который наш драйвер вообще имеет право писать (см.
/// [`SPMI_PERI_CHGR`]).
///
/// # Почему без read-modify-write
///
/// Весь байт регистра и есть поле FCC: у
/// `CHGR_FAST_CHARGE_CURRENT_CFG_REG` (`drivers_power_supply_qcom_smb5-reg.h:79`)
/// в шапке вендора не объявлено ни маски, ни бита — ср. соседний
/// `CHGR_CFG2_REG` там же, где `CHARGER_INHIBIT_BIT` есть. Писатель вендора
/// кладёт в регистр ровно `(val_u - min_u) / step_u` одним байтом
/// (`drivers_power_supply_qcom_smb-lib.c:353-373`, `smblib_write` принимает
/// `u8`), и параметр PM8150B
/// задаёт для этого поля `min_u = 0` (`qpnp-smb5.c:128-134`). Читать старое
/// значение перед записью нечего: чужих битов в регистре нет.
///
/// # Что делает
///
/// Одна сессия SUPERUSER: открытие, grant CHGR, запись, обратное чтение,
/// закрытие в [`Drop`]. Обратное чтение — не формальность: IOCTL записи может
/// завершиться успехом, а регистр остаться прежним (периферия не выдана, чип
/// в сбросе), и молчаливая неудача выглядела бы как поднятый предел.
/// Возвращается прочитанное, а не запрошенное, чтобы вызывающий видел, что
/// действительно стоит в регистре.
///
/// # Errors
///
/// `None` — не прошло открытие, grant, запись или обратное чтение. Частичного
/// успеха нет: не прочитали обратно — считаем, что записи не было.
///
/// # Safety
///
/// `PASSIVE_LEVEL`; `device` жив.
pub unsafe fn write_fcc_raw(device: WDFDEVICE, raw: u8) -> Option<u8> {
    // SAFETY: пассивный уровень, устройство создано.
    let mut su = unsafe { SuperuserBus::open(device) }.ok()?;
    // Регистр лежит в банке `0x10xx`, поэтому выдаётся только CHGR: USBIN и
    // `batt_info` этой записи не касаются.
    su.grant(SPMI_PERI_CHGR).ok()?;
    su.write_u8(SPMI_SID_USBIN, SPMI_REG_CHGR_FCC, raw).ok()?;
    su.read_u8(SPMI_SID_USBIN, SPMI_REG_CHGR_FCC).ok()
}

/// Число символов в пути (префикс + 16 цифр).

/// Кодирует адрес SUPERUSER: `(sid << 16) | reg`.
#[must_use]
pub const fn superuser_addr_enc(sid: u8, reg: u16) -> u32 {
    ((sid as u32) << 16) | (reg as u32)
}

/// Сессия `\Device\Spmi\SUPERUSER`: один open → grant → R/W → close.
///
/// Слот SUPERUSER ограничен тремя одновременными открытиями. Держим handle
/// только на время negotiate и закрываем в [`Drop`], чтобы не блокировать
/// `qcpmic` / `qcpmicext` / `qcpmgpio`.
#[derive(Debug)]
pub struct SuperuserBus {
    target: WDFIOTARGET,
}

impl SuperuserBus {
    /// Открывает `\Device\Spmi\SUPERUSER` (share-all, R/W).
    ///
    /// # Errors
    ///
    /// Возвращает сырой `NTSTATUS` открытия / создания цели.
    ///
    /// # Safety
    ///
    /// Пассивный уровень IRQL; `device` жив.
    pub unsafe fn open(device: WDFDEVICE) -> Result<Self, i32> {
        let mut chars = [0_u16; PROBE_NAME_CHARS];
        let name = "\\Device\\Spmi\\SUPERUSER";
        let mut length = 0_usize;
        for byte in name.bytes() {
            if let Some(slot) = chars.get_mut(length) {
                *slot = u16::from(byte);
                length = length.saturating_add(1);
            }
        }
        let mut io_target: WDFIOTARGET = WDF_NO_HANDLE.cast();
        // SAFETY: устройство создано; дескриптор локальный.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfIoTargetCreate,
                device,
                WDF_NO_OBJECT_ATTRIBUTES,
                &raw mut io_target,
            )
        };
        if !nt_ok(status) {
            return Err(status);
        }
        let bytes = u16::try_from(length.saturating_mul(2)).unwrap_or(0);
        let mut params: WDF_IO_TARGET_OPEN_PARAMS = unsafe { core::mem::zeroed() };
        params.Size = size_of_ulong::<WDF_IO_TARGET_OPEN_PARAMS>();
        params.Type = WdfIoTargetOpenByName;
        params.TargetDeviceName = UNICODE_STRING {
            Length: bytes,
            MaximumLength: bytes,
            Buffer: chars.as_mut_ptr(),
        };
        params.DesiredAccess = 0xC000_0000;
        params.ShareAccess = PROBE_SHARE_ALL;
        params.CreateDisposition = wdk_sys::FILE_OPEN;
        params.FileAttributes = wdk_sys::FILE_ATTRIBUTE_NORMAL;
        params.CreateOptions = 0x0000_0040;
        // SAFETY: цель создана, пассивный уровень.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(WdfIoTargetOpen, io_target, &raw mut params)
        };
        if !nt_ok(status) {
            unsafe {
                call_unsafe_wdf_function_binding!(WdfObjectDelete, io_target.cast());
            }
            return Err(status);
        }
        Ok(Self { target: io_target })
    }

    /// Peri-grant: `u16 count` + `count × u16` peri ids.
    ///
    /// # Errors
    ///
    /// Сырой `NTSTATUS` IOCTL grant.
    pub fn grant(&mut self, peri: u16) -> Result<(), i32> {
        let mut grant = [0_u8; 4];
        grant[0] = 1;
        grant[1] = 0;
        grant[2] = (peri & 0xFF) as u8;
        grant[3] = ((peri >> 8) & 0xFF) as u8;
        self.ioctl_in(IOCTL_SPMI_SUPERUSER_GRANT, &grant)
    }

    /// Читает один байт: SID + регистр (`addr_enc = (sid<<16)|reg`).
    ///
    /// # Errors
    ///
    /// Сырой `NTSTATUS` IOCTL read.
    pub fn read_u8(&mut self, sid: u8, reg: u16) -> Result<u8, i32> {
        let mut buf = [0_u8; 1];
        self.read_bytes(sid, reg, &mut buf)?;
        Ok(buf[0])
    }

    /// Пишет один байт.
    ///
    /// # Errors
    ///
    /// Сырой `NTSTATUS` IOCTL write.
    pub fn write_u8(&mut self, sid: u8, reg: u16, value: u8) -> Result<(), i32> {
        self.write_bytes(sid, reg, &[value])
    }

    /// Читает `out.len()` байт начиная с `reg`.
    ///
    /// # Errors
    ///
    /// Сырой `NTSTATUS` или `STATUS_INVALID_PARAMETER`, если длина 0 / >255.
    pub fn read_bytes(&mut self, sid: u8, reg: u16, out: &mut [u8]) -> Result<(), i32> {
        let len = out.len();
        if len == 0 || len > 255 {
            return Err(wdk_sys::STATUS_INVALID_PARAMETER);
        }
        let mut header = [0_u8; SPMI_SUPERUSER_HEADER_LEN];
        let enc = superuser_addr_enc(sid, reg);
        header[4] = (enc & 0xFF) as u8;
        header[5] = ((enc >> 8) & 0xFF) as u8;
        header[6] = ((enc >> 16) & 0xFF) as u8;
        header[7] = ((enc >> 24) & 0xFF) as u8;
        header[8] = len as u8;
        self.ioctl_in_out(IOCTL_SPMI_SUPERUSER_READ, &header, out)
    }

    /// Пишет payload начиная с `reg`.
    ///
    /// # Errors
    ///
    /// Сырой `NTSTATUS` или `STATUS_INVALID_PARAMETER`, если длина 0 / >255.
    pub fn write_bytes(&mut self, sid: u8, reg: u16, data: &[u8]) -> Result<(), i32> {
        let len = data.len();
        if len == 0 || len > 255 {
            return Err(wdk_sys::STATUS_INVALID_PARAMETER);
        }
        let mut buf = [0_u8; SPMI_SUPERUSER_HEADER_LEN.saturating_add(255)];
        let enc = superuser_addr_enc(sid, reg);
        buf[4] = (enc & 0xFF) as u8;
        buf[5] = ((enc >> 8) & 0xFF) as u8;
        buf[6] = ((enc >> 16) & 0xFF) as u8;
        buf[7] = ((enc >> 24) & 0xFF) as u8;
        buf[8] = len as u8;
        let total = SPMI_SUPERUSER_HEADER_LEN.saturating_add(len);
        if let Some(dst) = buf.get_mut(SPMI_SUPERUSER_HEADER_LEN..total) {
            dst.copy_from_slice(data);
        }
        self.ioctl_in(IOCTL_SPMI_SUPERUSER_WRITE, &buf[..total])
    }

    fn ioctl_in(&mut self, ioctl: u32, input: &[u8]) -> Result<(), i32> {
        let mut empty = [];
        self.ioctl_in_out(ioctl, input, &mut empty)
    }

    fn ioctl_in_out(&mut self, ioctl: u32, input: &[u8], output: &mut [u8]) -> Result<(), i32> {
        let mut request: WDFREQUEST = WDF_NO_HANDLE.cast();
        let mut in_mem: WDFMEMORY = WDF_NO_HANDLE.cast();
        let mut out_mem: WDFMEMORY = WDF_NO_HANDLE.cast();
        let mut in_ptr: *mut core::ffi::c_void = core::ptr::null_mut();
        let mut out_ptr: *mut core::ffi::c_void = core::ptr::null_mut();

        // SAFETY: цель открыта; пассивный уровень.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfRequestCreate,
                WDF_NO_OBJECT_ATTRIBUTES,
                self.target,
                &raw mut request,
            )
        };
        if !nt_ok(status) {
            return Err(status);
        }

        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfMemoryCreate,
                WDF_NO_OBJECT_ATTRIBUTES,
                wdk_sys::_POOL_TYPE::NonPagedPool,
                0,
                input.len(),
                &raw mut in_mem,
                &raw mut in_ptr,
            )
        };
        if !nt_ok(status) {
            unsafe {
                call_unsafe_wdf_function_binding!(WdfObjectDelete, request.cast());
            }
            return Err(status);
        }
        unsafe {
            core::ptr::copy_nonoverlapping(input.as_ptr(), in_ptr.cast::<u8>(), input.len());
        }

        let out_handle = if output.is_empty() {
            WDF_NO_HANDLE.cast()
        } else {
            let status = unsafe {
                call_unsafe_wdf_function_binding!(
                    WdfMemoryCreate,
                    WDF_NO_OBJECT_ATTRIBUTES,
                    wdk_sys::_POOL_TYPE::NonPagedPool,
                    0,
                    output.len(),
                    &raw mut out_mem,
                    &raw mut out_ptr,
                )
            };
            if !nt_ok(status) {
                unsafe {
                    call_unsafe_wdf_function_binding!(WdfObjectDelete, in_mem.cast());
                    call_unsafe_wdf_function_binding!(WdfObjectDelete, request.cast());
                }
                return Err(status);
            }
            unsafe {
                core::ptr::write_bytes(out_ptr.cast::<u8>(), 0xFF, output.len());
            }
            out_mem
        };

        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfIoTargetFormatRequestForIoctl,
                self.target,
                request,
                ioctl,
                in_mem,
                core::ptr::null_mut(),
                out_handle,
                core::ptr::null_mut(),
            )
        };
        let result = if nt_ok(status) {
            let mut options: WDF_REQUEST_SEND_OPTIONS = unsafe { core::mem::zeroed() };
            options.Size = size_of_ulong::<WDF_REQUEST_SEND_OPTIONS>();
            options.Flags =
                (WDF_REQUEST_SEND_OPTION_SYNCHRONOUS | WDF_REQUEST_SEND_OPTION_TIMEOUT) as ULONG;
            options.Timeout = SPB_TIMEOUT_100NS;
            let sent = unsafe {
                call_unsafe_wdf_function_binding!(
                    WdfRequestSend,
                    request,
                    self.target,
                    &raw mut options,
                )
            };
            if sent == 0 {
                Err(-1)
            } else {
                let st =
                    unsafe { call_unsafe_wdf_function_binding!(WdfRequestGetStatus, request) };
                if nt_ok(st) {
                    if !output.is_empty() && !out_ptr.is_null() {
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                out_ptr.cast::<u8>(),
                                output.as_mut_ptr(),
                                output.len(),
                            );
                        }
                    }
                    Ok(())
                } else {
                    Err(st)
                }
            }
        } else {
            Err(status)
        };

        unsafe {
            if !output.is_empty() {
                call_unsafe_wdf_function_binding!(WdfObjectDelete, out_mem.cast());
            }
            call_unsafe_wdf_function_binding!(WdfObjectDelete, in_mem.cast());
            call_unsafe_wdf_function_binding!(WdfObjectDelete, request.cast());
        }
        result
    }
}

impl Drop for SuperuserBus {
    fn drop(&mut self) {
        if self.target.is_null() {
            return;
        }
        // SAFETY: цель создана в `open`; вызывается на пассивном уровне (negotiate).
        unsafe {
            call_unsafe_wdf_function_binding!(WdfIoTargetClose, self.target);
            call_unsafe_wdf_function_binding!(WdfObjectDelete, self.target.cast());
        }
        self.target = WDF_NO_HANDLE.cast();
    }
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
    /// Строка пути к узлу Resource Hub: оставлена для диагностики.
    #[allow(dead_code)]
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
