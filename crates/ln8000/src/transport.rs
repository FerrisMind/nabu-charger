//! Абстракция шины I²C, на которой сидит LN8000.
//!
//! Ядро драйвера не знает, как именно устроен транспорт. Реализации:
//!
//! | Реализация | Где | Назначение |
//! |---|---|---|
//! | [`crate::testkit::MockPumpBus`] | этот крейт | тесты и демо без железа |
//! | `SpbBus` | `crates/kmdf` | реальная шина: SpbCx/I²C поверх ACPI-ресурса `PEIC` |

use crate::error::BusError;
use crate::regs::RegAddr;

/// Доступ к регистрам LN8000.
///
/// Адресация однобайтовая: чип принимает адрес регистра первым байтом
/// транзакции.
pub trait RegisterBus {
    /// Читает регистр.
    ///
    /// # Errors
    ///
    /// [`BusError`] при любом сбое шины.
    fn read(&mut self, addr: RegAddr) -> Result<u8, BusError>;

    /// Записывает регистр.
    ///
    /// # Errors
    ///
    /// [`BusError`] при любом сбое шины.
    fn write(&mut self, addr: RegAddr, value: u8) -> Result<(), BusError>;

    /// Сбрасывает состояние канала связи.
    ///
    /// # Errors
    ///
    /// [`BusError`], если канал не удалось восстановить.
    fn reset(&mut self) -> Result<(), BusError>;

    /// Короткое имя транспорта для журнала.
    fn name(&self) -> &'static str;

    /// Обновляет биты регистра, не трогая остальные (read-modify-write).
    ///
    /// # Errors
    ///
    /// Пробрасывает ошибку чтения или записи.
    fn update_bits(&mut self, addr: RegAddr, mask: u8, value: u8) -> Result<(), BusError> {
        let current = self.read(addr)?;
        let updated = (current & !mask) | (value & mask);
        self.write(addr, updated)
    }

    /// Читает 10-битное значение из пары соседних регистров.
    ///
    /// Так устроены результаты АЦП LN8000: код занимает два байта (см.
    /// `ln8000_bulk_read_reg(..., 2)` в эталонном драйвере). Транспорт вправе
    /// переопределить метод и прочитать пару одной транзакцией.
    ///
    /// # Errors
    ///
    /// Пробрасывает ошибку чтения.
    fn read_pair(&mut self, addr: RegAddr) -> Result<u16, BusError> {
        let low = self.read(addr)?;
        let high = self.read(addr.wrapping_add(1))?;
        Ok(u16::from(low) | (u16::from(high) << 8))
    }
}
