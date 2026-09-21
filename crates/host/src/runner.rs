//! Blocking session loop: turns the non-blocking core steps into a scenario.

use charger_core::{
    AdapterType, ChargePlan, Charger, ChargerError, ChargerTransport, Clock, Detection, Journal,
    State, Stats,
};
use std::time::Duration;

/// Session run options.
#[derive(Debug, Clone, Copy)]
pub struct RunOptions {
    /// Pause between detection polling steps.
    pub poll_interval: Duration,
    /// Fuse: how many steps are allowed before giving up.
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

/// Session outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionOutcome {
    /// Recognized adapter.
    pub adapter: AdapterType,
    /// Applied plan.
    pub plan: ChargePlan,
    /// Driver state at the end.
    pub state: State,
    /// Counters.
    pub stats: Stats,
}

/// Waits for detection to complete and applies the policy.
///
/// `on_adapter` is called the moment the adapter is recognized, which is handy
/// for printing a report line or sending a notification.
///
/// # Errors
///
/// Any core error is propagated out; a fuse timeout yields
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

/// Polls the device state and reapplies the policy when the adapter changes.
///
/// # Errors
///
/// Propagates core errors; on an adapter change returns the new plan.
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
