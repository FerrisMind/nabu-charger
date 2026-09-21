//! Charger driver: states, adapter detection, current policy, recovery.
//!
//! The driver is deliberately non-blocking: it never sleeps and never measures time
//! itself. The outer loop calls [`Charger::detect_step`] until it gets
//! [`Detection::Ready`], advancing [`Clock`]. This design gives deterministic timeout
//! tests and lets the same code run both in kernel mode (passive `PassiveLevel` loop
//! with a WDF timer) and in the host CLI.

use crate::apsd::AdapterType;
use crate::clock::Clock;
use crate::error::{ChargerError, TransportError, TransportErrorKind};
use crate::icl::IclEncoding;
use crate::journal::{Event, EventKind, Journal, Level};
use crate::policy::{self, ChargePolicy, Qc35Support};
use crate::regs;
use crate::transport::ChargerTransport;

/// Driver session state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Session is not open.
    Closed,
    /// Link verified, detection has not started yet.
    Idle,
    /// Waiting for APSD to complete.
    Detecting,
    /// Adapter type identified, policy applied.
    Ready,
    /// Session is faulted after attempts were exhausted.
    Faulted,
}

impl State {
    /// Stable state name for the journal.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::Idle => "idle",
            Self::Detecting => "detecting",
            Self::Ready => "ready",
            Self::Faulted => "faulted",
        }
    }
}

impl core::fmt::Display for State {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.label())
    }
}

/// Driver settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChargerConfig {
    /// How many milliseconds to wait for APSD to complete before failing.
    pub detect_timeout_ms: u64,
    /// How many times detection may be rerun on an unstable adapter.
    pub max_detect_reruns: u8,
    /// How many times the link may be reopened on a transport failure.
    pub max_transport_retries: u8,
    /// Safe current limit applied when the session closes.
    pub safe_icl_ua: u32,
    /// Upper bound on the current limit that may be applied.
    pub max_icl_ua: u32,
    /// Verify every write by reading it back.
    pub verify_writes: bool,
    /// Quick Charge 3.5 support state.
    pub qc35: Qc35Support,
    /// Encoding grid for the current limit.
    pub icl: IclEncoding,
}

impl Default for ChargerConfig {
    fn default() -> Self {
        Self {
            detect_timeout_ms: 3_000,
            max_detect_reruns: 2,
            max_transport_retries: 3,
            safe_icl_ua: policy::MIN_ICL_UA,
            max_icl_ua: policy::MAX_ICL_UA,
            verify_writes: true,
            qc35: Qc35Support::default(),
            icl: IclEncoding::default(),
        }
    }
}

impl ChargerConfig {
    /// Profile for Xiaomi Pad 5 (`nabu`): QC3.5 supported, no authentication in fact.
    #[must_use]
    pub fn for_nabu() -> Self {
        Self {
            qc35: Qc35Support::Supported {
                authenticated: false,
            },
            ..Self::default()
        }
    }

    /// Profile for the bench and tests: fast detection, checks enabled.
    #[must_use]
    pub fn for_testing() -> Self {
        Self {
            detect_timeout_ms: 500,
            max_detect_reruns: 1,
            max_transport_retries: 2,
            ..Self::default()
        }
    }
}

/// Result of one detection step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detection {
    /// Detection is still running; the caller must advance time and try again.
    Pending {
        /// How many milliseconds detection has been running.
        elapsed_ms: u64,
        /// How many steps have been made.
        attempts: u8,
    },
    /// Adapter type identified.
    Ready(AdapterType),
}

/// Result of polling the state after detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Monitor {
    /// Nothing changed.
    Unchanged(AdapterType),
    /// Adapter replaced or re-identified: the policy must be applied again.
    Changed(AdapterType),
    /// Power was lost.
    Detached,
}

/// What the driver actually applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChargePlan {
    /// Adapter type.
    pub adapter: AdapterType,
    /// Selected policy.
    pub policy: ChargePolicy,
    /// Code written to the current limit register.
    pub icl_raw: u8,
    /// Actually applied current (after quantization to the grid).
    pub applied_icl_ua: u32,
}

/// Driver counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Successful register reads.
    pub reads: u64,
    /// Successful register writes.
    pub writes: u64,
    /// Retries after transport failures.
    pub retries: u64,
    /// Link resets.
    pub resets: u64,
    /// Recorded errors.
    pub errors: u64,
    /// Completed detections.
    pub detections: u64,
}

/// Charger driver on top of an abstract transport.
///
/// # Examples
///
/// ```no_run
/// use core::testkit::ScriptedMockTransport;
/// use core::{Charger, ChargerConfig, ManualClock, NullJournal};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let clock = ManualClock::new();
/// let transport = ScriptedMockTransport::hvdcp3();
/// let mut charger =
///     Charger::open(transport, &clock, &NullJournal, ChargerConfig::for_nabu())?;
///
/// loop {
///     match charger.detect_step()? {
///         core::Detection::Ready(adapter) => {
///             let plan = charger.apply(adapter)?;
///             println!("{} → {} µA", adapter, plan.applied_icl_ua);
///             break;
///         }
///         core::Detection::Pending { .. } => clock.advance_ms(50),
///     }
/// }
/// charger.close();
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct Charger<'a, T: ChargerTransport, C: Clock, J: Journal> {
    transport: T,
    clock: &'a C,
    journal: &'a J,
    config: ChargerConfig,
    state: State,
    adapter: Option<AdapterType>,
    detect_started_ms: Option<u64>,
    reruns: u8,
    attempts: u8,
    seq: u64,
    request_id: u64,
    stats: Stats,
}

impl<'a, T: ChargerTransport, C: Clock, J: Journal> Charger<'a, T, C, J> {
    /// Opens a session: resets the link and verifies it by reading `APSD_STATUS`.
    ///
    /// # Errors
    ///
    /// * [`ChargerError::Transport`] - the peripheral is unavailable.
    /// * [`ChargerError::DeviceFault`] - the value read looks like a fault
    ///   (all bits set), which means a dead link rather than an attached power brick.
    pub fn open(
        transport: T,
        clock: &'a C,
        journal: &'a J,
        config: ChargerConfig,
    ) -> Result<Self, ChargerError> {
        let mut charger = Self {
            transport,
            clock,
            journal,
            config,
            state: State::Closed,
            adapter: None,
            detect_started_ms: None,
            reruns: 0,
            attempts: 0,
            seq: 0,
            request_id: 0,
            stats: Stats::default(),
        };

        let request = charger.next_request();
        let name = charger.transport.name();
        let reset_ok = charger.transport.reset().is_ok();
        charger.log(
            request,
            if reset_ok { Level::Debug } else { Level::Warn },
            EventKind::Reset { ok: reset_ok },
        );

        match charger.probe() {
            Ok(()) => {
                charger.transition(State::Idle);
                charger.log(
                    request,
                    Level::Info,
                    EventKind::Open {
                        transport: name,
                        ok: true,
                    },
                );
                Ok(charger)
            }
            Err(err) => {
                charger.stats.errors = charger.stats.errors.saturating_add(1);
                charger.log(
                    request,
                    Level::Error,
                    EventKind::Error {
                        op: "open",
                        error: err.code(),
                    },
                );
                charger.log(
                    request,
                    Level::Info,
                    EventKind::Open {
                        transport: name,
                        ok: false,
                    },
                );
                Err(err)
            }
        }
    }

    /// Current session state.
    #[must_use]
    pub const fn state(&self) -> State {
        self.state
    }

    /// Counters.
    #[must_use]
    pub const fn stats(&self) -> Stats {
        self.stats
    }

    /// Identified adapter type, if detection has already completed.
    #[must_use]
    pub const fn adapter(&self) -> Option<AdapterType> {
        self.adapter
    }

    /// Transport name for reports.
    #[must_use]
    pub fn transport_name(&self) -> &'static str {
        self.transport.name()
    }

    /// Settings the session was opened with.
    #[must_use]
    pub const fn config(&self) -> &ChargerConfig {
        &self.config
    }

    /// One adapter detection step.
    ///
    /// The method does not block: while APSD has not completed it returns
    /// [`Detection::Pending`], and the caller must advance time and call the method
    /// again. Once the rerun budget is exhausted it returns an error and the state
    /// becomes [`State::Faulted`].
    ///
    /// # Errors
    ///
    /// * [`ChargerError::NotOpen`] - the session is closed.
    /// * [`ChargerError::DetectionTimeout`] - APSD did not complete in the allotted time.
    /// * [`ChargerError::AdapterCheckTimeout`] - the adapter is unstable, reruns exhausted.
    /// * [`ChargerError::UnknownAdapterPattern`] - the hardware returned an unknown pattern.
    /// * [`ChargerError::Transport`] - link failure after retries were exhausted.
    pub fn detect_step(&mut self) -> Result<Detection, ChargerError> {
        if self.state == State::Closed {
            return Err(ChargerError::NotOpen);
        }
        let request = self.next_request();
        let now = self.clock.now_ms();
        let started = if let Some(started) = self.detect_started_ms {
            started
        } else {
            self.detect_started_ms = Some(now);
            self.transition(State::Detecting);
            now
        };

        let status = match self.read_reg(regs::APSD_STATUS, request) {
            Ok(value) => value,
            Err(err) => return Err(self.fail(request, "detect.read_status", err)),
        };

        let result = if status & regs::APSD_DTC_STATUS_DONE != 0 {
            match self.read_reg(regs::APSD_RESULT_STATUS, request) {
                Ok(value) => value,
                Err(err) => return Err(self.fail(request, "detect.read_result", err)),
            }
        } else {
            0
        };

        self.attempts = self.attempts.saturating_add(1);
        let elapsed_ms = now.saturating_sub(started);

        match AdapterType::decode(status, result, self.config.qc35) {
            Ok(adapter) => {
                self.stats.detections = self.stats.detections.saturating_add(1);
                self.adapter = Some(adapter);
                self.transition(State::Ready);
                self.log(
                    request,
                    Level::Info,
                    EventKind::Detect {
                        adapter: adapter.label(),
                        raw_status: status,
                        raw_result: result,
                        waited_ms: elapsed_ms,
                    },
                );
                Ok(Detection::Ready(adapter))
            }
            Err(err @ ChargerError::DetectionNotComplete) => {
                if elapsed_ms >= self.config.detect_timeout_ms {
                    if self.reruns >= self.config.max_detect_reruns {
                        let timeout = ChargerError::DetectionTimeout {
                            waited_ms: elapsed_ms,
                        };
                        return Err(self.fail(request, "detect.timeout", timeout));
                    }
                    self.retry_detection(request, "timeout")?;
                }
                let _ = err;
                Ok(Detection::Pending {
                    elapsed_ms,
                    attempts: self.attempts,
                })
            }
            Err(err @ ChargerError::AdapterCheckTimeout { .. }) => {
                if self.reruns >= self.config.max_detect_reruns {
                    return Err(self.fail(request, "detect.unstable", err));
                }
                self.retry_detection(request, "adapter_check_timeout")?;
                Ok(Detection::Pending {
                    elapsed_ms,
                    attempts: self.attempts,
                })
            }
            Err(err) => Err(self.fail(request, "detect.decode", err)),
        }
    }

    /// Computes and applies the input current policy for the identified adapter.
    ///
    /// Written are: the limit code to `USBIN_CURRENT_LIMIT_CFG`, the enable bits to
    /// `CMD_ICL_OVERRIDE` and, for Quick Charge, the voltage to
    /// `HVDCP_PULSE_COUNT_MAX`. With `verify_writes` every write is verified by
    /// reading it back.
    ///
    /// # Errors
    ///
    /// * [`ChargerError::NotOpen`] - the session is closed.
    /// * [`ChargerError::CurrentOutOfRange`] - the current is not representable on the grid.
    /// * [`ChargerError::VerifyFailed`] - the value read did not match the value written.
    /// * [`ChargerError::Transport`] - link failure after retries were exhausted.
    pub fn apply(&mut self, adapter: AdapterType) -> Result<ChargePlan, ChargerError> {
        if self.state == State::Closed {
            return Err(ChargerError::NotOpen);
        }
        let request = self.next_request();
        let policy = policy::policy_for(adapter, self.config.qc35);
        let bounded = if policy.icl_ua > self.config.max_icl_ua {
            self.config.max_icl_ua
        } else {
            policy.icl_ua
        };
        let target_ua = self.config.icl.quantize_down(bounded);
        let icl_raw = match self.config.icl.encode(target_ua) {
            Ok(raw) => raw,
            Err(err) => return Err(self.fail(request, "apply.encode_icl", err)),
        };

        if let Err(err) = self.write_reg(regs::USBIN_CURRENT_LIMIT_CFG, icl_raw, request) {
            return Err(self.fail(request, "apply.write_icl", err));
        }
        let override_bits = regs::ICL_OVERRIDE | regs::ICL_OVERRIDE_AFTER_APSD;
        if let Err(err) = self.update_reg(
            regs::CMD_ICL_OVERRIDE,
            override_bits,
            override_bits,
            request,
        ) {
            return Err(self.fail(request, "apply.write_override", err));
        }
        if let Some(voltage) = policy.qc2_voltage {
            if let Err(err) = self.update_reg(
                regs::HVDCP_PULSE_COUNT_MAX,
                regs::QC2_VOLTAGE_MASK,
                voltage.raw(),
                request,
            ) {
                return Err(self.fail(request, "apply.write_qc2", err));
            }
        }

        self.adapter = Some(adapter);
        self.transition(State::Ready);
        self.log(
            request,
            Level::Info,
            EventKind::Policy {
                adapter: adapter.label(),
                icl_ua: target_ua,
                icl_raw,
                qc2_voltage: policy.qc2_voltage.map(policy::Qc2Voltage::label),
                pump_eligible: policy.pump_eligible,
            },
        );
        Ok(ChargePlan {
            adapter,
            policy,
            icl_raw,
            applied_icl_ua: target_ua,
        })
    }

    /// Polls the state: whether the adapter changed and whether power was lost.
    ///
    /// # Errors
    ///
    /// * [`ChargerError::NotOpen`] - the session is closed.
    /// * [`ChargerError::Transport`] - link failure after retries were exhausted.
    pub fn monitor(&mut self) -> Result<Monitor, ChargerError> {
        if self.state == State::Closed {
            return Err(ChargerError::NotOpen);
        }
        let request = self.next_request();
        let status = match self.read_reg(regs::APSD_STATUS, request) {
            Ok(value) => value,
            Err(err) => return Err(self.fail(request, "monitor.read_status", err)),
        };
        if status & regs::APSD_DTC_STATUS_DONE == 0 {
            self.adapter = None;
            return Ok(Monitor::Detached);
        }
        let result = match self.read_reg(regs::APSD_RESULT_STATUS, request) {
            Ok(value) => value,
            Err(err) => return Err(self.fail(request, "monitor.read_result", err)),
        };
        match AdapterType::decode(status, result, self.config.qc35) {
            Ok(adapter) => {
                let previous = self.adapter;
                self.adapter = Some(adapter);
                if previous == Some(adapter) {
                    Ok(Monitor::Unchanged(adapter))
                } else {
                    self.log(
                        request,
                        Level::Warn,
                        EventKind::Detect {
                            adapter: adapter.label(),
                            raw_status: status,
                            raw_result: result,
                            waited_ms: 0,
                        },
                    );
                    Ok(Monitor::Changed(adapter))
                }
            }
            Err(ChargerError::DetectionNotComplete) => {
                self.adapter = None;
                Ok(Monitor::Detached)
            }
            Err(err) => Err(self.fail(request, "monitor.decode", err)),
        }
    }

    /// Reinitializes after a failure: reopens the link and resets detection state.
    ///
    /// # Errors
    ///
    /// * [`ChargerError::NotOpen`] - the session is closed (call [`Charger::open`] first).
    /// * [`ChargerError::Transport`] - the link could not be recovered.
    pub fn reinit(&mut self) -> Result<(), ChargerError> {
        if self.state == State::Closed {
            return Err(ChargerError::NotOpen);
        }
        let request = self.next_request();
        let ok = self.transport.reset().is_ok();
        self.stats.resets = self.stats.resets.saturating_add(1);
        self.log(request, Level::Info, EventKind::Reset { ok });
        if !ok {
            let err = TransportError::disconnected("link not recovered");
            return Err(self.fail(request, "reinit.reset", ChargerError::Transport(err)));
        }
        self.detect_started_ms = None;
        self.reruns = 0;
        self.attempts = 0;
        self.adapter = None;
        self.transition(State::Idle);
        self.probe()?;
        Ok(())
    }

    /// Closes the session, restoring the safe current limit.
    ///
    /// Called manually or automatically from [`Drop`]. Power is not switched off
    /// here: that is not the driver's job.
    pub fn close(&mut self) {
        if self.state == State::Closed {
            return;
        }
        let request = self.next_request();
        let safe = self.config.icl.quantize_down(self.config.safe_icl_ua);
        let ok = match self.config.icl.encode(safe) {
            Ok(raw) => self
                .write_reg(regs::USBIN_CURRENT_LIMIT_CFG, raw, request)
                .is_ok(),
            Err(_) => false,
        };
        self.transition(State::Closed);
        self.log(request, Level::Info, EventKind::Close { ok });
    }

    // --- internals ---

    fn next_request(&mut self) -> u64 {
        self.request_id = self.request_id.saturating_add(1);
        self.request_id
    }

    fn probe(&mut self) -> Result<(), ChargerError> {
        let request = self.next_request();
        match self.read_reg(regs::APSD_STATUS, request) {
            // 0xFF means an unread link, not "all faults at once".
            Ok(0xFF) => Err(ChargerError::DeviceFault { status: 0xFF }),
            Ok(_) => Ok(()),
            Err(err) => Err(err),
        }
    }

    fn retry_detection(&mut self, request: u64, reason: &'static str) -> Result<(), ChargerError> {
        if self.reruns >= self.config.max_detect_reruns {
            return Ok(());
        }
        self.reruns = self.reruns.saturating_add(1);
        self.log(
            request,
            Level::Warn,
            EventKind::Retry {
                op: "detect.rerun",
                attempt: self.reruns,
                reason,
            },
        );
        // Restart the detection state machine: without it an unstable adapter stays unidentified.
        match self.update_reg(regs::CMD_APSD, regs::APSD_RERUN, regs::APSD_RERUN, request) {
            Ok(()) => Ok(()),
            Err(err) => Err(self.fail(request, "detect.rerun", err)),
        }
    }

    fn read_reg(&mut self, addr: u16, request: u64) -> Result<u8, ChargerError> {
        let started = self.clock.now_ms();
        let mut attempt: u8 = 0;
        loop {
            match self.transport.read(addr) {
                Ok(value) => {
                    self.stats.reads = self.stats.reads.saturating_add(1);
                    self.log(
                        request,
                        Level::Trace,
                        EventKind::Read {
                            addr,
                            value,
                            elapsed_us: self.elapsed_us(started),
                        },
                    );
                    return Ok(value);
                }
                Err(err) => {
                    attempt = attempt.saturating_add(1);
                    if !recoverable(&err) || attempt > self.config.max_transport_retries {
                        self.stats.errors = self.stats.errors.saturating_add(1);
                        return Err(ChargerError::Transport(err));
                    }
                    self.recover(request, "read", attempt, err)?;
                }
            }
        }
    }

    fn write_reg(&mut self, addr: u16, value: u8, request: u64) -> Result<(), ChargerError> {
        let started = self.clock.now_ms();
        let mut attempt: u8 = 0;
        loop {
            match self.transport.write(addr, value) {
                Ok(()) => {
                    self.stats.writes = self.stats.writes.saturating_add(1);
                    self.log(
                        request,
                        Level::Trace,
                        EventKind::Write {
                            addr,
                            value,
                            elapsed_us: self.elapsed_us(started),
                        },
                    );
                    return self.verify(addr, value, request);
                }
                Err(err) => {
                    attempt = attempt.saturating_add(1);
                    if !recoverable(&err) || attempt > self.config.max_transport_retries {
                        self.stats.errors = self.stats.errors.saturating_add(1);
                        return Err(ChargerError::Transport(err));
                    }
                    self.recover(request, "write", attempt, err)?;
                }
            }
        }
    }

    fn update_reg(
        &mut self,
        addr: u16,
        mask: u8,
        value: u8,
        request: u64,
    ) -> Result<(), ChargerError> {
        let current = self.read_reg(addr, request)?;
        let updated = (current & !mask) | (value & mask);
        self.write_reg(addr, updated, request)
    }

    fn verify(&mut self, addr: u16, wrote: u8, request: u64) -> Result<(), ChargerError> {
        if !self.config.verify_writes {
            return Ok(());
        }
        let read = self.read_reg(addr, request)?;
        if read != wrote {
            let err = ChargerError::VerifyFailed { addr, wrote, read };
            self.stats.errors = self.stats.errors.saturating_add(1);
            self.log(
                request,
                Level::Error,
                EventKind::Error {
                    op: "verify",
                    error: err.code(),
                },
            );
            return Err(err);
        }
        Ok(())
    }

    fn recover(
        &mut self,
        request: u64,
        op: &'static str,
        attempt: u8,
        err: TransportError,
    ) -> Result<(), ChargerError> {
        self.stats.retries = self.stats.retries.saturating_add(1);
        self.log(
            request,
            Level::Warn,
            EventKind::Retry {
                op,
                attempt,
                reason: err.kind.as_str(),
            },
        );
        let ok = self.transport.reset().is_ok();
        self.stats.resets = self.stats.resets.saturating_add(1);
        self.log(request, Level::Debug, EventKind::Reset { ok });
        if ok {
            Ok(())
        } else {
            self.stats.errors = self.stats.errors.saturating_add(1);
            Err(ChargerError::Transport(err))
        }
    }

    fn fail(&mut self, request: u64, op: &'static str, err: ChargerError) -> ChargerError {
        self.stats.errors = self.stats.errors.saturating_add(1);
        self.transition(State::Faulted);
        self.log(
            request,
            Level::Error,
            EventKind::Error {
                op,
                error: err.code(),
            },
        );
        err
    }

    fn transition(&mut self, to: State) {
        if self.state == to {
            return;
        }
        let from = self.state.label();
        let request = self.request_id;
        self.state = to;
        self.log(
            request,
            Level::Debug,
            EventKind::StateChange {
                from,
                to: to.label(),
            },
        );
    }

    fn elapsed_us(&self, started_ms: u64) -> u64 {
        self.clock
            .now_ms()
            .saturating_sub(started_ms)
            .saturating_mul(1_000)
    }

    fn log(&mut self, request_id: u64, level: Level, kind: EventKind) {
        self.seq = self.seq.saturating_add(1);
        let event = Event {
            seq: self.seq,
            ts_ms: self.clock.now_ms(),
            request_id,
            level,
            kind,
        };
        self.journal.event(&event);
    }
}

impl<T: ChargerTransport, C: Clock, J: Journal> Drop for Charger<'_, T, C, J> {
    /// Closes the session and restores the safe current limit.
    ///
    /// Errors are deliberately swallowed: panicking in `Drop` is forbidden, and the
    /// state after the driver is unloaded must not depend on hardware availability.
    fn drop(&mut self) {
        self.close();
    }
}

fn recoverable(err: &TransportError) -> bool {
    matches!(
        err.kind,
        TransportErrorKind::Timeout | TransportErrorKind::Disconnected | TransportErrorKind::Io
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::ManualClock;
    use crate::testkit::{Fault, ScriptedMockTransport, VecJournal};

    fn mock(adapter: AdapterType) -> ScriptedMockTransport {
        ScriptedMockTransport::for_adapter(adapter)
    }

    #[test]
    fn open_probes_the_link() {
        let clock = ManualClock::new();
        let journal = VecJournal::new();
        let charger = Charger::open(
            mock(AdapterType::Hvdcp3),
            &clock,
            &journal,
            ChargerConfig::for_testing(),
        )
        .expect("the session must open");
        assert_eq!(charger.state(), State::Idle);
        assert_eq!(charger.transport_name(), "mock");
        assert_eq!(journal.count_of("open"), 1);
        assert!(journal.count_of("read") >= 1);
    }

    #[test]
    fn open_fails_when_link_is_dead() {
        let clock = ManualClock::new();
        let journal = VecJournal::new();
        let mut transport = mock(AdapterType::Hvdcp3);
        transport.push_fault(Fault::ReadError {
            addr: regs::APSD_STATUS,
            kind: TransportErrorKind::Timeout,
            times: 5,
        });
        let config = ChargerConfig {
            max_transport_retries: 0,
            ..ChargerConfig::for_testing()
        };
        let result = Charger::open(transport, &clock, &journal, config);
        assert!(result.is_err(), "a dead link must not open");
        match result {
            Err(err) => assert_eq!(err.code(), "transport"),
            Ok(_) => unreachable!(),
        }
    }

    #[test]
    fn detect_reports_pending_then_ready() {
        let clock = ManualClock::new();
        let journal = VecJournal::new();
        let transport = mock(AdapterType::Hvdcp3);
        transport.set_detection_ready_after(3);
        let mut charger = Charger::open(transport, &clock, &journal, ChargerConfig::for_testing())
            .expect("the session must open");

        let first = charger.detect_step().expect("the step must run");
        assert!(matches!(first, Detection::Pending { .. }));

        let second = charger.detect_step().expect("the step must run");
        assert!(matches!(second, Detection::Pending { .. }));

        let third = charger.detect_step().expect("the step must run");
        assert_eq!(third, Detection::Ready(AdapterType::Hvdcp3));
        assert_eq!(charger.state(), State::Ready);
        assert_eq!(charger.adapter(), Some(AdapterType::Hvdcp3));
    }

    #[test]
    fn detect_times_out_and_faults() {
        let clock = ManualClock::new();
        let journal = VecJournal::new();
        let transport = ScriptedMockTransport::detached();
        transport.set_detection_ready_after(200);
        let config = ChargerConfig {
            detect_timeout_ms: 100,
            max_detect_reruns: 0,
            ..ChargerConfig::for_testing()
        };
        let mut charger =
            Charger::open(transport, &clock, &journal, config).expect("the session must open");
        assert!(matches!(
            charger.detect_step().expect("first step"),
            Detection::Pending { .. }
        ));

        clock.advance_ms(500);
        let err = charger.detect_step().expect_err("a timeout is expected");
        assert!(
            matches!(err, ChargerError::DetectionTimeout { .. }),
            "got error: {err:?}"
        );
        assert_eq!(charger.state(), State::Faulted);
        assert!(journal.count_of("error") >= 1);
    }

    #[test]
    fn detect_reruns_on_unstable_adapter() {
        let clock = ManualClock::new();
        let journal = VecJournal::new();
        let mut transport = mock(AdapterType::Hvdcp3);
        transport.set_reg(
            regs::APSD_STATUS,
            regs::APSD_DTC_STATUS_DONE | regs::HVDCP_CHECK_TIMEOUT,
        );
        let config = ChargerConfig {
            max_detect_reruns: 1,
            ..ChargerConfig::for_testing()
        };
        let mut charger =
            Charger::open(transport, &clock, &journal, config).expect("the session must open");

        assert!(matches!(
            charger.detect_step().expect("first step"),
            Detection::Pending { .. }
        ));
        assert!(journal.count_of("retry") == 1, "detection must be rerun");
        assert!(charger.stats().writes >= 1, "the rerun writes to CMD_APSD");

        let err = charger.detect_step().expect_err("reruns exhausted");
        assert!(matches!(err, ChargerError::AdapterCheckTimeout { .. }));
    }

    #[test]
    fn unknown_pattern_is_reported() {
        let clock = ManualClock::new();
        let journal = VecJournal::new();
        let mut charger = Charger::open(
            ScriptedMockTransport::unknown_pattern(),
            &clock,
            &journal,
            ChargerConfig::for_testing(),
        )
        .expect("the session must open");
        let err = charger.detect_step().expect_err("pattern not recognized");
        match err {
            ChargerError::UnknownAdapterPattern { raw } => assert_eq!(raw, 0x3F),
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn apply_writes_registers_and_verifies() {
        let clock = ManualClock::new();
        let journal = VecJournal::new();
        let mut charger = Charger::open(
            mock(AdapterType::Hvdcp3),
            &clock,
            &journal,
            ChargerConfig::for_testing(),
        )
        .expect("the session must open");
        charger.detect_step().expect("detection");
        let plan = charger.apply(AdapterType::Hvdcp3).expect("policy");

        assert_eq!(plan.applied_icl_ua, 3_000_000);
        assert_eq!(plan.icl_raw, 0x1D, "3 A on the 100 mA grid → code 29");
        assert!(plan.policy.pump_eligible);
        assert_eq!(charger.state(), State::Ready);
        assert_eq!(journal.count_of("policy"), 1);
    }

    #[test]
    fn apply_accepts_low_power_adapter() {
        let clock = ManualClock::new();
        let journal = VecJournal::new();
        let mut charger = Charger::open(
            mock(AdapterType::Sdp),
            &clock,
            &journal,
            ChargerConfig::for_testing(),
        )
        .expect("the session must open");
        let plan = charger.apply(AdapterType::Sdp).expect("policy for SDP");
        assert_eq!(plan.applied_icl_ua, 500_000);
        assert_eq!(plan.icl_raw, 4, "500 mA on the 100 mA grid → code 4");
        assert!(!plan.policy.pump_eligible);
    }

    #[test]
    fn verify_failure_is_reported() {
        let clock = ManualClock::new();
        let journal = VecJournal::new();
        let mut transport = mock(AdapterType::Hvdcp3);
        transport.push_fault(Fault::WrongReadBack {
            addr: regs::USBIN_CURRENT_LIMIT_CFG,
            value: 0x00,
            times: 1,
        });
        let mut charger = Charger::open(transport, &clock, &journal, ChargerConfig::for_testing())
            .expect("the session must open");
        charger.detect_step().expect("detection");
        let err = charger
            .apply(AdapterType::Hvdcp3)
            .expect_err("write verification must fail");
        match err {
            ChargerError::VerifyFailed { addr, wrote, read } => {
                assert_eq!(addr, regs::USBIN_CURRENT_LIMIT_CFG);
                assert_eq!(wrote, 0x1D);
                assert_eq!(read, 0x00);
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn transport_retry_recovers_after_single_failure() {
        let clock = ManualClock::new();
        let journal = VecJournal::new();
        let mut transport = mock(AdapterType::Hvdcp3);
        transport.push_fault(Fault::ReadError {
            addr: regs::APSD_STATUS,
            kind: TransportErrorKind::Timeout,
            times: 1,
        });
        let mut charger = Charger::open(transport, &clock, &journal, ChargerConfig::for_testing())
            .expect("the retry must save the session");
        assert_eq!(charger.stats().retries, 1);
        assert_eq!(charger.stats().resets, 1, "reset while recovering the link");
        assert_eq!(
            charger.detect_step().expect("detection"),
            Detection::Ready(AdapterType::Hvdcp3)
        );
    }

    #[test]
    fn reinit_restores_session_after_fault() {
        let clock = ManualClock::new();
        let journal = VecJournal::new();
        let mut transport = mock(AdapterType::Hvdcp3);
        // The faults target the result register: open (which reads only APSD_STATUS)
        // succeeds, while the detection after it hits a dead link.
        transport.push_fault(Fault::ReadError {
            addr: regs::APSD_RESULT_STATUS,
            kind: TransportErrorKind::Disconnected,
            times: 2,
        });
        let config = ChargerConfig {
            max_transport_retries: 1,
            ..ChargerConfig::for_testing()
        };
        let mut charger = Charger::open(transport, &clock, &journal, config)
            .expect("open succeeds before the retries are exhausted");

        let result = charger.detect_step();
        assert!(result.is_err(), "detection must fail: {result:?}");
        if let Err(err) = result {
            assert_eq!(err.code(), "transport");
            assert_eq!(charger.state(), State::Faulted);
        }

        charger.reinit().expect("reinitialization must succeed");
        assert_eq!(charger.state(), State::Idle);
        assert_eq!(
            charger
                .detect_step()
                .expect("detection after reinitialization"),
            Detection::Ready(AdapterType::Hvdcp3)
        );
    }

    #[test]
    fn close_sets_safe_current_and_closes_session() {
        let clock = ManualClock::new();
        let journal = VecJournal::new();
        let mut charger = Charger::open(
            mock(AdapterType::Hvdcp3),
            &clock,
            &journal,
            ChargerConfig::for_testing(),
        )
        .expect("the session must open");
        charger.detect_step().expect("detection");
        charger.apply(AdapterType::Hvdcp3).expect("policy");
        charger.close();
        assert_eq!(charger.state(), State::Closed);
        assert_eq!(journal.count_of("close"), 1);
        let err = charger.detect_step().expect_err("the session is closed");
        assert!(matches!(err, ChargerError::NotOpen));
    }

    #[test]
    fn drop_emits_close_event() {
        let clock = ManualClock::new();
        let journal = VecJournal::new();
        {
            let _charger = Charger::open(
                mock(AdapterType::Dcp),
                &clock,
                &journal,
                ChargerConfig::for_testing(),
            )
            .expect("the session must open");
        }
        assert_eq!(journal.count_of("close"), 1, "Drop must close the session");
    }

    #[test]
    fn monitor_reports_detached_when_no_power() {
        let clock = ManualClock::new();
        let journal = VecJournal::new();
        let mut charger = Charger::open(
            ScriptedMockTransport::detached(),
            &clock,
            &journal,
            ChargerConfig::for_testing(),
        )
        .expect("the session must open");
        assert_eq!(charger.monitor().expect("poll"), Monitor::Detached);
    }

    #[test]
    fn monitor_reports_unchanged_adapter() {
        let clock = ManualClock::new();
        let journal = VecJournal::new();
        let mut charger = Charger::open(
            mock(AdapterType::Dcp),
            &clock,
            &journal,
            ChargerConfig::for_testing(),
        )
        .expect("the session must open");
        assert_eq!(
            charger.detect_step().expect("detection"),
            Detection::Ready(AdapterType::Dcp)
        );
        assert_eq!(
            charger.monitor().expect("poll"),
            Monitor::Unchanged(AdapterType::Dcp)
        );
    }

    #[test]
    fn every_event_has_request_id_and_timestamp() {
        let clock = ManualClock::new();
        let journal = VecJournal::new();
        let mut charger = Charger::open(
            mock(AdapterType::Hvdcp3),
            &clock,
            &journal,
            ChargerConfig::for_testing(),
        )
        .expect("the session must open");
        clock.advance_ms(1500);
        charger.detect_step().expect("detection");
        charger.apply(AdapterType::Hvdcp3).expect("policy");

        assert!(journal.len() > 5);
        let mut seen_request = false;
        journal.for_each(|index, event| {
            assert_eq!(
                event.seq,
                index as u64 + 1,
                "records are numbered consecutively"
            );
            if event.request_id > 0 {
                seen_request = true;
            }
        });
        assert!(seen_request, "records must carry a request id");
        let detect = journal.first_of("detect").expect("detection record");
        assert_eq!(detect.ts_ms, 1500, "timestamp comes from the core clock");
        assert!(detect.request_id > 0);
    }
}
