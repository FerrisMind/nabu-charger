//! Ядро драйвера зарядки Xiaomi Pad 5 (`nabu`) под Windows on ARM.
//!
//! Крейт не зависит от платформы, не требует `std` и не выполняет ввод-вывод
//! самостоятельно: доступ к железу идёт через [`ChargerTransport`], время — через
//! [`Clock`], журнал — через [`Journal`]. Благодаря этому вся логика детекции
//! адаптера и выставления лимита тока проверяется на моке без реального устройства.
//!
//! # Зачем это нужно
//!
//! Под Windows планшет не заряжается ни от одного блока: аппаратная детекция
//! адаптера (APSD) в PMIC выполняется, но ни один компонент Windows её результат
//! не читает, поэтому входной ток не поднимается. Ядро закрывает ровно этот
//! пробел: читает результат детекции и применяет политику тока.
//!
//! # Что делает ядро
//!
//! 1. Проверяет связь с периферией зарядника ([`Charger::open`]).
//! 2. Дожидается окончания APSD и разбирает тип адаптера ([`Charger::detect_step`]).
//! 3. Считает и применяет лимит входного тока ([`Charger::apply`]).
//! 4. Следит за сменой адаптера ([`Charger::monitor`]).
//! 5. Корректно освобождает ресурсы ([`Charger::close`] и [`Drop`]).
//!
//! # Пример
//!
//! ```no_run
//! use core::testkit::ScriptedMockTransport;
//! use core::{AdapterType, Charger, ChargerConfig, ManualClock, NullJournal};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let transport = ScriptedMockTransport::hvdcp3();
//! let clock = ManualClock::new();
//! let journal = NullJournal;
//!
//! let mut charger = Charger::open(transport, &clock, &journal, ChargerConfig::default())?;
//! clock.advance_ms(1_000);
//! if let core::Detection::Ready(adapter) = charger.detect_step()? {
//!     assert_eq!(adapter, AdapterType::Hvdcp3);
//!     charger.apply(adapter)?;
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # Границы ответственности
//!
//! Крейт **не** пишет в реестр Windows, не управляет charge pump (LN8000) и не
//! работает с файловой системой. Транспорт к регистрам PMIC предоставляется
//! уровнем выше: в хост-инструментах это мок или TCP, в драйвере режима ядра —
//! SPMI через устройство `\Device\RESOURCE_HUB`.

#![cfg_attr(not(feature = "std"), no_std)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
#![cfg_attr(test, allow(clippy::indexing_slicing, clippy::arithmetic_side_effects))]

pub mod apsd;
pub mod clock;
pub mod driver;
pub mod error;
pub mod icl;
pub mod journal;
pub mod manifest;
pub mod policy;
pub mod regs;
pub mod transport;

#[cfg(any(test, feature = "testkit"))]
pub mod testkit;

pub use apsd::AdapterType;
pub use clock::{Clock, ManualClock};
pub use driver::{ChargePlan, Charger, ChargerConfig, Detection, Monitor, State, Stats};
pub use error::{ChargerError, TransportError, TransportErrorKind};
pub use icl::IclEncoding;
pub use journal::{Event, EventKind, Journal, Level, NullJournal};
#[cfg(feature = "std")]
pub use manifest::render_spec;
pub use manifest::{AdapterSpec, RegisterSpec, adapter_table};
pub use policy::{ChargePolicy, Qc2Voltage, Qc35Support, policy_for};
pub use regs::RegAddr;
pub use transport::ChargerTransport;
