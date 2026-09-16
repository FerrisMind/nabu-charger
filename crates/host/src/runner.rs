//! Блокирующий цикл сессии: превращает неблокирующие шаги ядра в сценарий.

use charger_core::{
    AdapterType, ChargePlan, Charger, ChargerError, ChargerTransport, Clock, Detection, Journal,
    State, Stats,
};
use std::time::Duration;

/// Параметры прогона сессии.
#[derive(Debug, Clone, Copy)]
pub struct RunOptions {
    /// Пауза между шагами ожидания детекции.
    pub poll_interval: Duration,
    /// Предохранитель: сколько шагов допускается до отказа.
    pub max_steps: u32,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(20),
            max_steps: 1_000,
        }
    }
}

/// Итог сессии.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionOutcome {
    /// Распознанный адаптер.
    pub adapter: AdapterType,
    /// Применённый план.
    pub plan: ChargePlan,
    /// Состояние драйвера в конце.
    pub state: State,
    /// Счётчики.
    pub stats: Stats,
}

/// Ждёт завершения детекции и применяет политику.
///
/// `on_adapter` вызывается в момент распознавания адаптера — удобно, чтобы
/// напечатать строку отчёта или отправить уведомление.
///
/// # Errors
///
/// Любая ошибка ядра пробрасывается наружу; таймаут предохранителя даёт
/// [`ChargerError::DetectionTimeout`].
pub fn run_until_ready<T, C, J, F>(
    charger: &mut Charger<'_, T, C, J>,
    clock: &C,
    options: RunOptions,
    mut on_adapter: F,
) -> Result<SessionOutcome, ChargerError>
where
    T: ChargerTransport,
    C: Clock,
    J: Journal,
    F: FnMut(AdapterType) -> Result<(), ChargerError>,
{
    let mut steps: u32 = 0;
    loop {
        steps = steps.saturating_add(1);
        if steps > options.max_steps {
            return Err(ChargerError::DetectionTimeout {
                waited_ms: clock.now_ms(),
            });
        }
        match charger.detect_step()? {
            Detection::Pending { .. } => std::thread::sleep(options.poll_interval),
            Detection::Ready(adapter) => {
                on_adapter(adapter)?;
                let plan = charger.apply(adapter)?;
                return Ok(SessionOutcome {
                    adapter,
                    plan,
                    state: charger.state(),
                    stats: charger.stats(),
                });
            }
        }
    }
}

/// Опрашивает состояние устройства и переприменяет политику при смене адаптера.
///
/// # Errors
///
/// Пробрасывает ошибки ядра; при смене адаптера возвращает новый план.
pub fn monitor_cycle<T, C, J>(
    charger: &mut Charger<'_, T, C, J>,
) -> Result<Option<ChargePlan>, ChargerError>
where
    T: ChargerTransport,
    C: Clock,
    J: Journal,
{
    match charger.monitor()? {
        charger_core::Monitor::Unchanged(_) | charger_core::Monitor::Detached => Ok(None),
        charger_core::Monitor::Changed(adapter) => Ok(Some(charger.apply(adapter)?)),
    }
}
