//! Ядро драйвера charge pump LN8000 для Xiaomi Pad 5 (`nabu`).
//!
//! LN8000 — вторая ступень зарядки: преобразователь 2:1, который позволяет
//! брать от блока 9 В и отдавать в батарею удвоенный ток. В Android его
//! обслуживает драйвер `ln8000_charger.c`; под Windows драйвера нет — этот
//! крейт повторяет логику того драйвера в переносимом виде.
//!
//! # Что делает ядро
//!
//! 1. Проверяет, что на шине именно LN8000 ([`Pump::open`]).
//! 2. Настраивает пороги и защиты ([`Pump::configure`]) — как `ln8000_init_device()`.
//! 3. Включает режим 2:1 и проверяет, что чип его принял ([`Pump::enable_switching`]).
//! 4. Читает состояние и отказы ([`Pump::status`]), снимает показания АЦП
//!    ([`Pump::read_adc`]).
//! 5. Умеет программный сброс ([`Pump::soft_reset`]) и перевод в standby
//!    ([`Pump::standby`], [`Pump::close`], [`Drop`]).
//!
//! # Границы ответственности
//!
//! * Крейт не знает про I²C-контроллер: транспорт — за трейтом [`RegisterBus`].
//! * Крейт не спит: паузу после сброса и обслуживание сторожевого таймера
//!   выполняет вызывающая сторона.
//! * Крейт не управляет согласованием напряжения с блоком питания: это задача
//!   Type-C/PD-части платформы, а не charge pump.
//!
//! # Пример
//!
//! ```
//! use ln8000::testkit::MockPumpBus;
//! use ln8000::{AdcChannel, OpMode, Pump, PumpConfig};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let mut pump = Pump::open(MockPumpBus::new(), PumpConfig::for_qc35_class_b())?;
//! pump.configure()?;
//! assert_eq!(pump.enable_switching()?, OpMode::Switching);
//!
//! let status = pump.status()?;
//! assert!(!status.has_critical_fault());
//! assert_eq!(status.op_mode, OpMode::Switching);
//!
//! let vbat = pump.read_adc(AdcChannel::Vbat)?;
//! println!("напряжение батареи: {vbat} мкВ");
//! # Ok(())
//! # }
//! ```

#![cfg_attr(not(feature = "std"), no_std)]
#![deny(missing_docs)]
#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
#![cfg_attr(test, allow(clippy::indexing_slicing, clippy::arithmetic_side_effects))]

pub mod driver;
pub mod encoding;
pub mod error;
pub mod regs;
pub mod status;
pub mod transport;

#[cfg(any(test, feature = "testkit"))]
pub mod testkit;

pub use driver::{Pump, PumpConfig, PumpState};
pub use encoding::{
    AdcHibernateDelay, AdcMode, OpMode, WatchdogPeriod, decode_iin_limit, decode_vbat_float,
    encode_iin_limit, encode_ntc_alarm, encode_vac_ovp, encode_vbat_float,
};
pub use error::{BusError, BusErrorKind, PumpError};
pub use status::{AdcChannel, Status};
pub use transport::RegisterBus;
