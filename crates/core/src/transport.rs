//! Абстракция транспорта: как ядро попадает в регистры PMIC.
//!
//! Ядро знает только этот трейт. Реализации:
//!
//! | Реализация | Где живёт | Назначение |
//! |---|---|---|
//! | [`crate::testkit::ScriptedMockTransport`] | этот крейт (`testkit`) | тесты и демо без железа |
//! | `MockTransport` | `host` | мок с журналом транзакций |
//! | `TcpTransport` | `host` | реальный транспорт по сети (стенд, эмулятор) |
//! | `SpmiTransport` | `kmdf` | реальное железо: SPMI через `\Device\RESOURCE_HUB` |

use crate::error::TransportError;
use crate::regs::RegAddr;

/// Доступ к регистрам периферии зарядника.
///
/// Реализация обязана быть устойчивой к повторным вызовам после сбоя: ядро
/// вызывает [`ChargerTransport::reset`] и продолжает работу, не перезапуская
/// процесс.
pub trait ChargerTransport {
    /// Читает один байт регистра.
    ///
    /// # Errors
    ///
    /// Любая ошибка доступа к железу возвращается как [`TransportError`].
    fn read(&mut self, addr: RegAddr) -> Result<u8, TransportError>;

    /// Записывает один байт регистра.
    ///
    /// # Errors
    ///
    /// Любая ошибка доступа к железу возвращается как [`TransportError`].
    fn write(&mut self, addr: RegAddr, value: u8) -> Result<(), TransportError>;

    /// Сбрасывает состояние канала связи (переоткрытие устройства, очистка буферов).
    ///
    /// Вызывается ядром при восстановлении после сбоя транспорта.
    ///
    /// # Errors
    ///
    /// [`TransportError`], если канал не удалось восстановить.
    fn reset(&mut self) -> Result<(), TransportError>;

    /// Короткое имя транспорта для журнала (например, `mock`, `tcp`, `spmi`).
    fn name(&self) -> &'static str;

    /// Читает регистр с маской: возвращает только значимые биты.
    ///
    /// # Errors
    ///
    /// Пробрасывает ошибку [`ChargerTransport::read`].
    fn read_masked(&mut self, addr: RegAddr, mask: u8) -> Result<u8, TransportError> {
        Ok(self.read(addr)? & mask)
    }

    /// Обновляет биты регистра, не трогая остальные (read-modify-write).
    ///
    /// # Errors
    ///
    /// Пробрасывает ошибку чтения или записи.
    fn update_bits(&mut self, addr: RegAddr, mask: u8, value: u8) -> Result<(), TransportError> {
        let current = self.read(addr)?;
        let updated = (current & !mask) | (value & mask);
        self.write(addr, updated)
    }
}
