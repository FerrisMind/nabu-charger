//! Публичный интерфейс SPB (Simple Peripheral Bus) и мелкая арифметика вокруг него.
//!
//! Зачем отдельный крейт: эти типы объявлены в заголовке WDK `shared/spb.h`, но
//! `wdk-sys` их не генерирует (фича `spb` пустая), поэтому их приходится
//! объявлять вручную. Вынесенные сюда структуры, сборка списка передач и путь
//! Resource Hub не зависят от WDF, а значит проверяются обычными тестами.
//!
//! # Откуда взяты типы
//!
//! ```c
//! // shared/spb.h (WDK 10.0.26100)
//! typedef enum SPB_TRANSFER_DIRECTION { None = 0, FromDevice = 1, ToDevice = 2 } ;
//! typedef enum SPB_TRANSFER_BUFFER_FORMAT { Invalid = 0, Simple = 1, List = 2,
//!                                           SimpleNonPaged = 3, Mdl = 4 };
//! typedef struct SPB_TRANSFER_BUFFER_LIST_ENTRY { PVOID Buffer; ULONG BufferCb; };
//! typedef struct SPB_TRANSFER_BUFFER {
//!     SPB_TRANSFER_BUFFER_FORMAT Format;
//!     union { SPB_TRANSFER_BUFFER_LIST_ENTRY Simple;
//!             struct { PSPB_TRANSFER_BUFFER_LIST_ENTRY List; ULONG ListCe; } BufferList;
//!             PMDL Mdl; };
//! };
//! typedef struct SPB_TRANSFER_LIST_ENTRY {
//!     SPB_TRANSFER_DIRECTION Direction; ULONG DelayInUs; SPB_TRANSFER_BUFFER Buffer;
//! };
//! typedef struct SPB_TRANSFER_LIST {
//!     ULONG Size; ULONG Reserved; ULONG TransferCount;
//!     SPB_TRANSFER_LIST_ENTRY Transfers[1];
//! };
//! ```
//!
//! # Почему это правильный путь доступа к периферии
//!
//! Реверс `qcpmicEIC8150.sys` показал, что штатный клиент Qualcomm обращается к
//! регистрам PMIC через **тот же** код управления `0x41808`:
//!
//! ```text
//! 0x41808 = CTL_CODE(FILE_DEVICE_CONTROLLER, 0x602, METHOD_BUFFERED, FILE_ANY_ACCESS)
//!         = IOCTL_SPB_EXECUTE_SEQUENCE
//! ```
//!
//! (разбор — `docs/SPMI-PATH.md`). Значит приватного протокола доступа к
//! регистрам нет: используется публичный список передач SPB.

#![no_std]
#![forbid(unsafe_code)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::arithmetic_side_effects
    )
)]

use core::ffi::c_void;
use core::mem::offset_of;

/// Тип устройства контроллера (`FILE_DEVICE_CONTROLLER` из `wdm.h`).
pub const FILE_DEVICE_CONTROLLER: u32 = 0x0000_0004;

/// Функция кода управления `IOCTL_SPB_EXECUTE_SEQUENCE`.
pub const SPB_FUNCTION_EXECUTE_SEQUENCE: u32 = 0x0602;

/// Выполняет последовательность передач на устройстве шины.
///
/// `CTL_CODE(FILE_DEVICE_CONTROLLER, 0x602, METHOD_BUFFERED, FILE_ANY_ACCESS)`.
pub const IOCTL_SPB_EXECUTE_SEQUENCE: u32 = 0x0004_1808;

/// Направление передачи: обмен не нужен.
pub const SPB_DIRECTION_NONE: u32 = 0;
/// Направление передачи: чтение с устройства.
pub const SPB_DIRECTION_FROM_DEVICE: u32 = 1;
/// Направление передачи: запись в устройство.
pub const SPB_DIRECTION_TO_DEVICE: u32 = 2;

/// Формат буфера: простая буферная область.
pub const SPB_FORMAT_SIMPLE: u32 = 1;

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
    /// Первый элемент; остальные лежат в памяти сразу за ним.
    pub transfers: [SpbTransferListEntry; 1],
}

/// Ошибки сборки последовательности.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SequenceError {
    /// Области памяти не хватает либо она не выровнена под список.
    AreaTooSmall,
    /// Передач больше, чем помещается в поле счётчика.
    TooManyTransfers,
}

impl SpbTransferList {
    /// Размер заголовка списка (без элементов): 48 байт по `spb.h`.
    #[must_use]
    pub const fn header_size() -> usize {
        core::mem::size_of::<Self>()
    }

    /// Размер одного элемента: 32 байта по `spb.h`.
    #[must_use]
    pub const fn entry_size() -> usize {
        core::mem::size_of::<SpbTransferListEntry>()
    }

    /// Полный размер области для `count` передач.
    ///
    /// Заголовок [`SpbTransferList`] уже содержит место под первую передачу
    /// (`Transfers[1]`), поэтому на каждую следующую добавляется один элемент:
    /// `sizeof(SPB_TRANSFER_LIST) + (count - 1) * sizeof(SPB_TRANSFER_LIST_ENTRY)`.
    #[must_use]
    pub const fn area_size(count: usize) -> usize {
        Self::header_size()
            .saturating_add(Self::entry_size().saturating_mul(count.saturating_sub(1)))
    }
}

/// Инициализирует запись списка передач «простой буфер».
#[must_use]
pub fn entry_init(direction: u32, buffer: *mut c_void, buffer_cb: u32) -> SpbTransferListEntry {
    SpbTransferListEntry {
        direction,
        delay_in_us: 0,
        buffer: SpbTransferBuffer {
            format: SPB_FORMAT_SIMPLE,
            simple: SpbTransferBufferListEntry { buffer, buffer_cb },
        },
    }
}

/// Собирает список передач в предоставленной области памяти.
///
/// Смещения берутся из самой структуры (`offset_of!`), поэтому записанные байты
/// гарантированно совпадают с раскладкой, которую видит драйвер шины.
///
/// # Errors
///
/// * [`SequenceError::AreaTooSmall`] — область меньше [`SpbTransferList::area_size`]
///   или не выровнена под список.
/// * [`SequenceError::TooManyTransfers`] — передач больше `u32::MAX`.
pub fn describe(
    area: &mut [u8],
    transfers: &[SpbTransferListEntry],
) -> Result<usize, SequenceError> {
    let needed = SpbTransferList::area_size(transfers.len());
    let alignment = core::mem::align_of::<SpbTransferList>();
    if area.len() < needed || area.as_ptr().align_offset(alignment) != 0 {
        return Err(SequenceError::AreaTooSmall);
    }
    let count = u32::try_from(transfers.len()).map_err(|_| SequenceError::TooManyTransfers)?;

    let header_size = SpbTransferList::header_size();
    let size_field = u32::try_from(header_size).map_err(|_| SequenceError::TooManyTransfers)?;
    write_u32(area, offset_of!(SpbTransferList, size), size_field);
    write_u32(area, offset_of!(SpbTransferList, reserved), 0);
    write_u32(area, offset_of!(SpbTransferList, transfer_count), count);

    let entry_size = SpbTransferList::entry_size();
    // Первая передача лежит внутри заголовка (`Transfers[1]`), остальные —
    // вплотную за ним, начиная со смещения самого массива передач.
    let entries_base = offset_of!(SpbTransferList, transfers);
    for (index, transfer) in transfers.iter().enumerate() {
        let base = entries_base.saturating_add(index.saturating_mul(entry_size));
        write_u32(
            area,
            base.saturating_add(offset_of!(SpbTransferListEntry, direction)),
            transfer.direction,
        );
        write_u32(
            area,
            base.saturating_add(offset_of!(SpbTransferListEntry, delay_in_us)),
            transfer.delay_in_us,
        );
        let buffer_base = base.saturating_add(offset_of!(SpbTransferListEntry, buffer));
        write_u32(
            area,
            buffer_base.saturating_add(offset_of!(SpbTransferBuffer, format)),
            transfer.buffer.format,
        );
        let simple_base = buffer_base.saturating_add(offset_of!(SpbTransferBuffer, simple));
        write_pointer(
            area,
            simple_base.saturating_add(offset_of!(SpbTransferBufferListEntry, buffer)),
            transfer.buffer.simple.buffer,
        );
        write_u32(
            area,
            simple_base.saturating_add(offset_of!(SpbTransferBufferListEntry, buffer_cb)),
            transfer.buffer.simple.buffer_cb,
        );
    }

    Ok(needed)
}

/// Последовательность «запись байта»: адрес, затем значение.
///
/// # Errors
///
/// [`SequenceError::AreaTooSmall`] — область меньше двух передач.
pub fn write_one(
    area: &mut [u8],
    address: &mut [u8; 1],
    value: &mut [u8; 1],
) -> Result<usize, SequenceError> {
    let transfers = [
        entry_init(
            SPB_DIRECTION_TO_DEVICE,
            address.as_mut_ptr().cast::<c_void>(),
            1,
        ),
        entry_init(
            SPB_DIRECTION_TO_DEVICE,
            value.as_mut_ptr().cast::<c_void>(),
            1,
        ),
    ];
    describe(area, &transfers)
}

/// Последовательность «чтение байта»: адрес, затем чтение.
///
/// # Errors
///
/// [`SequenceError::AreaTooSmall`] — область меньше двух передач.
pub fn read_one(
    area: &mut [u8],
    address: &mut [u8; 1],
    data: &mut [u8; 1],
) -> Result<usize, SequenceError> {
    let transfers = [
        entry_init(
            SPB_DIRECTION_TO_DEVICE,
            address.as_mut_ptr().cast::<c_void>(),
            1,
        ),
        entry_init(
            SPB_DIRECTION_FROM_DEVICE,
            data.as_mut_ptr().cast::<c_void>(),
            1,
        ),
    ];
    describe(area, &transfers)
}

fn write_u32(area: &mut [u8], offset: usize, value: u32) {
    let bytes = value.to_ne_bytes();
    if let Some(slot) = area.get_mut(offset..offset.saturating_add(bytes.len())) {
        slot.copy_from_slice(&bytes);
    }
}

fn write_pointer(area: &mut [u8], offset: usize, value: *mut c_void) {
    let bytes = (value as usize).to_ne_bytes();
    if let Some(slot) = area.get_mut(offset..offset.saturating_add(bytes.len())) {
        slot.copy_from_slice(&bytes);
    }
}

/// Префикс пути подключения Resource Hub (`reshub.h`).
pub const HUB_PATH_PREFIX: &[u8] = b"\\Device\\RESOURCE_HUB\\";

/// Число символов в пути подключения.
pub const HUB_PATH_CHARS: usize = 40;

/// Строит путь устройства для идентификатора подключения.
///
/// Правило из `shared/reshub.h`: `RESOURCE_HUB_CREATE_PATH_FROM_ID` →
/// `RESOURCE_HUB_ID_TO_FILE_NAME`, формат `%0*I64x` ширины 16, то есть префикс
/// и ровно 16 шестнадцатеричных цифр в нижнем регистре.
#[must_use]
pub fn resource_hub_path(id: u64) -> HubPath {
    let mut path = HubPath {
        chars: [0; HUB_PATH_CHARS],
        len: 0,
    };
    for byte in HUB_PATH_PREFIX {
        path.push_char(u16::from(*byte));
    }
    let mut nibble = 15_i32;
    while nibble >= 0 {
        let shift = u32::try_from(nibble).unwrap_or(0).saturating_mul(4);
        let digit = ((id >> shift) & 0xF) as u8;
        let character = if digit < 10 {
            b'0'.saturating_add(digit)
        } else {
            b'a'.saturating_add(digit.saturating_sub(10))
        };
        path.push_char(u16::from(character));
        nibble = nibble.saturating_sub(1);
    }
    path
}

/// Путь Resource Hub в виде массива UTF-16.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HubPath {
    chars: [u16; HUB_PATH_CHARS],
    len: usize,
}

impl HubPath {
    fn push_char(&mut self, character: u16) {
        if let Some(slot) = self.chars.get_mut(self.len) {
            *slot = character;
            self.len = self.len.saturating_add(1);
        }
    }

    /// Число значимых символов.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Признак пустого пути (для корректного идентификатора не бывает).
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Символы UTF-16 без завершающего нуля.
    #[must_use]
    pub fn chars(&self) -> &[u16] {
        self.chars.get(..self.len).unwrap_or(&[])
    }

    /// Указатель для поля `UNICODE_STRING.Buffer`.
    #[must_use]
    pub fn as_ptr(&self) -> *const u16 {
        self.chars.as_ptr()
    }

    /// Длина в байтах для `UNICODE_STRING.Length`.
    #[must_use]
    pub fn byte_len(&self) -> u16 {
        let doubled = self.len.saturating_mul(2);
        u16::try_from(doubled).unwrap_or(u16::MAX)
    }

    /// Представление в ASCII (для журнала и тестов).
    #[must_use]
    pub fn as_ascii(&self) -> [u8; HUB_PATH_CHARS] {
        let mut out = [0_u8; HUB_PATH_CHARS];
        for (index, character) in self.chars().iter().enumerate() {
            if let Some(slot) = out.get_mut(index) {
                *slot = u8::try_from(*character).unwrap_or(b'?');
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Хранилище, выровненное под список передач: драйвер шины требует, чтобы
    /// `SPB_TRANSFER_LIST` лежал по выровненному адресу.
    #[repr(align(8))]
    struct Aligned<const N: usize>([u8; N]);

    impl<const N: usize> Aligned<N> {
        fn new() -> Self {
            Self([0; N])
        }

        fn area(&mut self) -> &mut [u8] {
            &mut self.0
        }
    }

    fn u32_at(area: impl AsRef<[u8]>, offset: usize) -> u32 {
        let bytes = area.as_ref();
        let mut value = [0_u8; 4];
        if let Some(slice) = bytes.get(offset..offset.saturating_add(4)) {
            value.copy_from_slice(slice);
        }
        u32::from_ne_bytes(value)
    }

    fn usize_at(area: impl AsRef<[u8]>, offset: usize) -> usize {
        let bytes = area.as_ref();
        let mut value = [0_u8; 8];
        if let Some(slice) = bytes.get(offset..offset.saturating_add(8)) {
            value.copy_from_slice(slice);
        }
        usize::from_ne_bytes(value)
    }

    #[test]
    fn ioctl_code_matches_header_derivation() {
        let code = (FILE_DEVICE_CONTROLLER << 16) | (SPB_FUNCTION_EXECUTE_SEQUENCE << 2);
        assert_eq!(code, IOCTL_SPB_EXECUTE_SEQUENCE);
        assert_eq!(IOCTL_SPB_EXECUTE_SEQUENCE, 0x0004_1808);
    }

    #[test]
    fn structure_layout_matches_spb_header() {
        // Значения из shared/spb.h для 64-битной сборки: 48 и 32 байта.
        assert_eq!(SpbTransferList::header_size(), 48);
        assert_eq!(SpbTransferList::entry_size(), 32);
        assert_eq!(core::mem::size_of::<SpbTransferBuffer>(), 24);
        assert_eq!(core::mem::align_of::<SpbTransferList>(), 8);
        // Число передач лежит не в начале заголовка, а по смещению 8.
        assert_eq!(offset_of!(SpbTransferList, transfer_count), 8);
        assert_eq!(offset_of!(SpbTransferList, transfers), 16);
        // Внутри записи: буфер начинается на 8, а внутри буфера — формат,
        // простой буфер на 8, указатель и длина простого буфера 0 и 8.
        assert_eq!(offset_of!(SpbTransferListEntry, buffer), 8);
        assert_eq!(offset_of!(SpbTransferBuffer, format), 0);
        assert_eq!(offset_of!(SpbTransferBuffer, simple), 8);
        assert_eq!(offset_of!(SpbTransferBufferListEntry, buffer), 0);
        assert_eq!(offset_of!(SpbTransferBufferListEntry, buffer_cb), 8);
    }

    #[test]
    fn area_size_grows_by_entry() {
        assert_eq!(
            SpbTransferList::area_size(1),
            SpbTransferList::header_size()
        );
        assert_eq!(
            SpbTransferList::area_size(3),
            SpbTransferList::header_size() + 2 * SpbTransferList::entry_size()
        );
    }

    #[test]
    fn read_sequence_describes_address_then_data() {
        let mut storage = Aligned::<128>::new();
        let area = storage.area();
        let mut address = [0x1E_u8];
        let mut data = [0_u8];
        let written = read_one(area, &mut address, &mut data).expect("сборка прошла");
        assert_eq!(written, SpbTransferList::area_size(2));
        assert_eq!(
            u32_at(&area, offset_of!(SpbTransferList, size)) as usize,
            48
        );
        assert_eq!(
            u32_at(&area, offset_of!(SpbTransferList, transfer_count)),
            2
        );

        let entry_size = SpbTransferList::entry_size();
        let entries = offset_of!(SpbTransferList, transfers);
        let base = |index: usize| entries + index * entry_size;
        assert_eq!(
            u32_at(&area, base(0) + offset_of!(SpbTransferListEntry, direction)),
            SPB_DIRECTION_TO_DEVICE
        );
        assert_eq!(
            u32_at(&area, base(1) + offset_of!(SpbTransferListEntry, direction)),
            SPB_DIRECTION_FROM_DEVICE
        );
        let buffer_base = base(0) + offset_of!(SpbTransferListEntry, buffer);
        assert_eq!(
            u32_at(&area, buffer_base + offset_of!(SpbTransferBuffer, format)),
            SPB_FORMAT_SIMPLE
        );
        let simple_base = buffer_base + offset_of!(SpbTransferBuffer, simple);
        assert_eq!(
            usize_at(
                &area,
                simple_base + offset_of!(SpbTransferBufferListEntry, buffer)
            ),
            address.as_mut_ptr() as usize
        );
        assert_eq!(
            u32_at(
                &area,
                simple_base + offset_of!(SpbTransferBufferListEntry, buffer_cb)
            ),
            1
        );
    }

    #[test]
    fn write_sequence_sends_address_then_value() {
        let mut storage = Aligned::<128>::new();
        let area = storage.area();
        let mut address = [0x1E_u8];
        let mut value = [0x40_u8];
        let written = write_one(area, &mut address, &mut value).expect("сборка прошла");
        assert_eq!(written, SpbTransferList::area_size(2));
        let entry_size = SpbTransferList::entry_size();
        let entries = offset_of!(SpbTransferList, transfers);
        for index in 0..2 {
            assert_eq!(
                u32_at(
                    &area,
                    entries + index * entry_size + offset_of!(SpbTransferListEntry, direction)
                ),
                SPB_DIRECTION_TO_DEVICE
            );
        }
    }

    #[test]
    fn too_small_area_is_rejected() {
        let mut area = [0_u8; 4];
        let mut address = [0_u8];
        let mut data = [0_u8];
        assert_eq!(
            read_one(&mut area, &mut address, &mut data),
            Err(SequenceError::AreaTooSmall)
        );
    }

    #[test]
    fn misaligned_area_is_rejected() {
        let mut storage = [0_u8; 160];
        let mut address = [0_u8];
        let mut data = [0_u8];
        let area = storage.get_mut(1..).expect("срез");
        if area.as_ptr().align_offset(8) != 0 {
            assert_eq!(
                read_one(area, &mut address, &mut data),
                Err(SequenceError::AreaTooSmall)
            );
        }
    }

    #[test]
    fn hub_path_uses_sixteen_lowercase_digits() {
        let path = resource_hub_path(0x0000_0000_0000_1234);
        let ascii = path.as_ascii();
        assert_eq!(
            &ascii[..HUB_PATH_PREFIX.len()],
            HUB_PATH_PREFIX,
            "префикс должен совпадать с reshub.h"
        );
        let text = core::str::from_utf8(&ascii[..path.len()]).expect("ascii");
        assert!(text.ends_with("0000000000001234"), "получено: {text}");
        assert_eq!(path.len(), HUB_PATH_PREFIX.len() + 16);
        assert_eq!(
            usize::from(path.byte_len()),
            (HUB_PATH_PREFIX.len() + 16) * 2
        );
    }

    #[test]
    fn area_size_counts_first_entry_in_header() {
        // Заголовок вмещает первую передачу, поэтому две передачи требуют ровно
        // на один элемент больше заголовка.
        assert_eq!(
            SpbTransferList::area_size(1),
            SpbTransferList::header_size()
        );
        assert_eq!(
            SpbTransferList::area_size(2),
            SpbTransferList::header_size() + SpbTransferList::entry_size()
        );
    }

    #[test]
    fn hub_path_is_stable_for_known_connection() {
        let path = resource_hub_path(0x0000_001c_0000_0000);
        let ascii = path.as_ascii();
        let text = core::str::from_utf8(&ascii[..path.len()]).expect("ascii");
        assert!(text.ends_with("0000001c00000000"), "получено: {text}");
    }
}
