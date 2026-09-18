//! Структуры SPB, объявленные в заголовке WDK.
//!
//! `wdk-sys` не генерирует эти типы (фича `spb` пустая), поэтому они объявлены
//! здесь дословно по `shared/spb.h` из установленного WDK 10.0.26100:
//!
//! ```c
//! typedef enum SPB_TRANSFER_DIRECTION {
//!     SpbTransferDirectionNone,          // 0
//!     SpbTransferDirectionFromDevice,    // 1 — чтение с устройства
//!     SpbTransferDirectionToDevice,      // 2 — запись в устройство
//!     SpbTransferDirectionMax
//! } SPB_TRANSFER_DIRECTION;
//!
//! typedef enum SPB_TRANSFER_BUFFER_FORMAT {
//!     SpbTransferBufferFormatInvalid,        // 0
//!     SpbTransferBufferFormatSimple,         // 1
//!     ... List = 2, SimpleNonPaged = 3, Mdl = 4
//! } SPB_TRANSFER_BUFFER_FORMAT;
//!
//! typedef struct SPB_TRANSFER_BUFFER_LIST_ENTRY { PVOID Buffer; ULONG BufferCb; };
//!
//! typedef struct SPB_TRANSFER_BUFFER {
//!     SPB_TRANSFER_BUFFER_FORMAT Format;
//!     union { SPB_TRANSFER_BUFFER_LIST_ENTRY Simple;
//!             struct { PSPB_TRANSFER_BUFFER_LIST_ENTRY List; ULONG ListCe; } BufferList;
//!             PMDL Mdl; };
//! };
//!
//! typedef struct SPB_TRANSFER_LIST_ENTRY {
//!     SPB_TRANSFER_DIRECTION Direction; ULONG DelayInUs; SPB_TRANSFER_BUFFER Buffer;
//! };
//!
//! typedef struct SPB_TRANSFER_LIST {
//!     ULONG Size; ULONG Reserved; ULONG TransferCount;
//!     SPB_TRANSFER_LIST_ENTRY Transfers[1];
//! };
//! ```

use core::ffi::c_void;

/// Тип устройства контроллера (`FILE_DEVICE_CONTROLLER` из `wdm.h`).
///
/// Приведён как часть вывода кода управления SPB: показать, откуда взялось
/// значение [`IOCTL_SPB_EXECUTE_SEQUENCE`].
#[allow(dead_code)]
pub const FILE_DEVICE_CONTROLLER: u32 = 0x0000_0004;

/// Код управления, выполняющий последовательность передач.
///
/// `CTL_CODE(FILE_DEVICE_CONTROLLER, 0x602, METHOD_BUFFERED, FILE_ANY_ACCESS)`
/// из `shared/spb.h`.
pub const IOCTL_SPB_EXECUTE_SEQUENCE: u32 = 0x0004_1808;

/// `CTL_CODE(FILE_DEVICE_CONTROLLER, 0x603, METHOD_BUFFERED, FILE_ANY_ACCESS)`.
pub const IOCTL_SPB_LOCK_CONNECTION: u32 = 0x0004_180C;

/// `CTL_CODE(FILE_DEVICE_CONTROLLER, 0x604, METHOD_BUFFERED, FILE_ANY_ACCESS)`.
#[allow(dead_code)]
pub const IOCTL_SPB_UNLOCK_CONNECTION: u32 = 0x0004_1810;

/// Направление передачи: чтение с устройства.
/// Направление `None`: завершающий элемент списка передач.
pub const SPB_DIRECTION_NONE: u32 = 0;

/// `CTL_CODE(FILE_DEVICE_CONTROLLER, 0x600, METHOD_BUFFERED, FILE_ANY_ACCESS)`.
pub const IOCTL_SPB_LOCK_CONTROLLER: u32 = 0x0004_1800;

/// `CTL_CODE(FILE_DEVICE_CONTROLLER, 0x601, METHOD_BUFFERED, FILE_ANY_ACCESS)`.
pub const IOCTL_SPB_UNLOCK_CONTROLLER: u32 = 0x0004_1804;

/// `CTL_CODE(FILE_DEVICE_CONTROLLER, 0x605, METHOD_BUFFERED, FILE_ANY_ACCESS)`.
pub const IOCTL_SPB_FULL_DUPLEX: u32 = 0x0004_1814;

/// `CTL_CODE(FILE_DEVICE_CONTROLLER, 0x606, METHOD_BUFFERED, FILE_ANY_ACCESS)`.
pub const IOCTL_SPB_MULTI_SPI_TRANSFER: u32 = 0x0004_1818;

/// Запрос подключения к периферии: эталонный клиент Qualcomm отправляет его
/// узлу до доступа к регистрам (`0x32C004` из разбора `qcpmicEIC8150.sys`).
pub const IOCTL_ATTACH: u32 = 0x0032_C004;

/// SPMI SUPERUSER: чтение байт (`CTL_CODE(0x85B5, 0x903, METHOD_BUFFERED, ANY)`).
pub const IOCTL_SPMI_SUPERUSER_READ: u32 = 0x85B5_240C;

/// SPMI SUPERUSER: запись байт.
pub const IOCTL_SPMI_SUPERUSER_WRITE: u32 = 0x85B5_2410;

/// SPMI SUPERUSER: битовая операция (длина 1).
#[allow(dead_code)]
pub const IOCTL_SPMI_SUPERUSER_BITOP: u32 = 0x85B5_2414;

/// SPMI SUPERUSER: grant списка периферий (`u16 count` + `count × u16`).
pub const IOCTL_SPMI_SUPERUSER_GRANT: u32 = 0x85B5_2418;

/// Длина заголовка R/W SUPERUSER: `{u32 flags, u32 addr_enc, u32 len}`.
pub const SPMI_SUPERUSER_HEADER_LEN: usize = 12;

/// Магия во входе запроса подключения (`0x42696541` — `AeiB`).
pub const ATTACH_MAGIC: u32 = 0x4269_6541;

/// Длина ответа узла на запрос подключения.
pub const ATTACH_REPLY_LEN: usize = 1024;
pub const SPB_DIRECTION_FROM_DEVICE: u32 = 1;/// Направление передачи: запись в устройство.
pub const SPB_DIRECTION_TO_DEVICE: u32 = 2;

/// Формат буфера: простая буферная область.
pub const SPB_FORMAT_SIMPLE: u32 = 1;

/// Формат «список буферов»: буфер описан массивом элементов.
#[allow(dead_code)]
pub const SPB_FORMAT_LIST: u32 = 2;

/// Формат MDL: буфер описан через MDL.
#[allow(dead_code)]
pub const SPB_FORMAT_MDL: u32 = 4;

/// Элемент списка буферов.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SpbTransferBufferListEntry {
    /// Указатель на данные.
    pub buffer: *mut c_void,
    /// Длина данных в байтах.
    pub buffer_cb: u32,
}

/// Буфер передачи (используется вариант `Simple`).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SpbTransferBuffer {
    /// Формат буфера.
    pub format: u32,
    /// Простая буферная область.
    pub simple: SpbTransferBufferListEntry,
}

/// Одна передача в последовательности.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SpbTransferListEntry {
    /// Направление передачи.
    pub direction: u32,
    /// Задержка перед передачей, мкс.
    pub delay_in_us: u32,
    /// Буфер передачи.
    pub buffer: SpbTransferBuffer,
}

/// Список передач: заголовок плюс элементы.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SpbTransferList {
    /// Размер структуры (`sizeof(SPB_TRANSFER_LIST)`).
    pub size: u32,
    /// Зарезервировано, должно быть нулём.
    pub reserved: u32,
    /// Число передач.
    pub transfer_count: u32,
    /// Первый элемент (в памяти за ним идут остальные).
    pub transfers: [SpbTransferListEntry; 1],
}

impl SpbTransferList {
    /// `sizeof(SPB_TRANSFER_LIST)` — именно это значение обязано лежать в поле
    /// `size` (заголовок вместе с ОДНОЙ записью), сколько бы передач ни было.
    #[must_use]
    pub const fn header_size() -> usize {
        core::mem::size_of::<Self>()
    }

    /// Размер одного элемента.
    #[must_use]
    pub const fn entry_size() -> usize {
        core::mem::size_of::<SpbTransferListEntry>()
    }

    /// Полный размер области памяти под список для `count` передач:
    /// `sizeof(SPB_TRANSFER_LIST) + sizeof(entry) * (count - 1)` — ровно так
    /// определён `SPB_TRANSFER_LIST_AND_ENTRIES(count)` в WDK.
    #[must_use]
    pub const fn area_size(count: usize) -> usize {
        if count <= 1 {
            return Self::header_size();
        }
        Self::header_size().saturating_add(Self::entry_size().saturating_mul(count - 1))
    }
}

/// Раскладка `SPB_TRANSFER_LIST` — проверяется компилятором, а не глазами.
///
/// `sizeof(SPB_TRANSFER_LIST)` = 48 (заголовок 16 + одна запись 32),
/// `sizeof(SPB_TRANSFER_LIST_ENTRY)` = 32, область под `n` передач —
/// 48/80/112. Любое расхождение ломает сборку.
const _: () = assert!(SpbTransferList::header_size() == 48);
const _: () = assert!(SpbTransferList::entry_size() == 32);
const _: () = assert!(SpbTransferList::area_size(1) == 48);
const _: () = assert!(SpbTransferList::area_size(2) == 80);
const _: () = assert!(SpbTransferList::area_size(3) == 112);

/// Инициализирует запись списка передач «простой буфер».
pub fn entry_init(
    direction: u32,
    buffer: *mut c_void,
    buffer_cb: u32,
) -> SpbTransferListEntry {
    SpbTransferListEntry {
        direction,
        delay_in_us: 0,
        buffer: SpbTransferBuffer {
            format: SPB_FORMAT_SIMPLE,
            simple: SpbTransferBufferListEntry { buffer, buffer_cb },
        },
    }
}
