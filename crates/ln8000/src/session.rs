//! Telemetry and the journal of charge sessions.
//!
//! The module keeps two things at once:
//!
//! * **a ring buffer of samples**: the latest measurements (voltages, current,
//!   temperature, mode), good for a plot and for incident analysis;
//! * **charge sessions**: the intervals when the input has power; each one keeps
//!   its duration, peak current, peak temperature and a flag recording whether
//!   the device ran in 2:1 mode.
//!
//! Nothing is allocated: the buffers are fixed size, so the module suits both a
//! `no_std` driver and host tests.

use crate::encoding::OpMode;

/// Size of the sample ring buffer.
pub const SAMPLE_RING: usize = 256;
/// How many completed sessions are kept in memory.
pub const SESSION_HISTORY: usize = 32;

/// One telemetry sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TelemetrySample {
    /// Timestamp in milliseconds of a monotonic clock.
    pub ts_ms: u64,
    /// Battery voltage, µV.
    pub vbat_uv: u32,
    /// Input voltage, µV.
    pub vbus_uv: u32,
    /// Input current, µA.
    pub iin_ua: u32,
    /// Die temperature, tenths of °C.
    pub die_temp_dc: i32,
    /// Operating mode at the moment of the sample.
    pub op_mode: OpMode,
    /// Whether the input has power.
    pub input_present: bool,
    /// Whether [`Self::vbat_uv`] is valid (the ADC channel was read).
    ///
    /// A failed read yields zero, which is indistinguishable from a real zero:
    /// such a sample can neither cut current nor restore the limit to the profile.
    pub vbat_valid: bool,
    /// Whether [`Self::die_temp_dc`] is valid (the ADC channel was read).
    ///
    /// Zero degrees is both "cold" and "no data": the guard must tell these
    /// cases apart, or current restore fires on garbage and `Stop` on zero.
    pub die_temp_valid: bool,
}

/// Charge session: an interval during which the input had power.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChargeSession {
    /// Session start, ms.
    pub started_ms: u64,
    /// Session end, ms (`None` while the session is running).
    pub ended_ms: Option<u64>,
    /// How many samples landed in the session.
    pub samples: u32,
    /// Peak input current, µA.
    pub peak_iin_ua: u32,
    /// Peak die temperature, tenths of °C.
    pub peak_die_temp_dc: i32,
    /// Whether 2:1 mode was seen (fast charging).
    pub saw_switching: bool,
    /// Whether 1:1 bypass mode was seen.
    pub saw_bypass: bool,
}

impl ChargeSession {
    /// Empty session that started at `started_ms`.
    #[must_use]
    pub const fn new(started_ms: u64) -> Self {
        Self {
            started_ms,
            ended_ms: None,
            samples: 0,
            peak_iin_ua: 0,
            peak_die_temp_dc: i32::MIN,
            saw_switching: false,
            saw_bypass: false,
        }
    }

    /// Session duration in milliseconds (for a running one, up to `now_ms`).
    #[must_use]
    pub const fn duration_ms(&self, now_ms: u64) -> u64 {
        match self.ended_ms {
            Some(end) => end.saturating_sub(self.started_ms),
            None => now_ms.saturating_sub(self.started_ms),
        }
    }

    /// Whether there was at least one moment of fast charging in the session.
    #[must_use]
    pub const fn had_fast_mode(&self) -> bool {
        self.saw_switching
    }
}

/// Telemetry storage: the sample ring plus the session history.
#[derive(Debug)]
pub struct Telemetry {
    samples: [Option<TelemetrySample>; SAMPLE_RING],
    sample_next: usize,
    sample_total: u64,
    sessions: [Option<ChargeSession>; SESSION_HISTORY],
    session_next: usize,
    session_total: u64,
    current: Option<ChargeSession>,
}

impl Default for Telemetry {
    fn default() -> Self {
        Self::new()
    }
}

impl Telemetry {
    /// Creates empty telemetry.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            samples: [None; SAMPLE_RING],
            sample_next: 0,
            sample_total: 0,
            sessions: [None; SESSION_HISTORY],
            session_next: 0,
            session_total: 0,
            current: None,
        }
    }

    /// Accepts a sample and updates the session.
    ///
    /// A session opens when power appears and closes when it disappears. A closed
    /// session goes into the history ring subject to rotation.
    pub fn push(&mut self, sample: TelemetrySample) {
        if let Some(slot) = self.samples.get_mut(self.sample_next) {
            *slot = Some(sample);
        }
        self.sample_next = (self.sample_next.saturating_add(1)) % SAMPLE_RING;
        self.sample_total = self.sample_total.saturating_add(1);

        if sample.input_present {
            if self.current.is_none() {
                self.session_total = self.session_total.saturating_add(1);
                self.current = Some(ChargeSession::new(sample.ts_ms));
            }
            if let Some(session) = self.current.as_mut() {
                session.samples = session.samples.saturating_add(1);
                if sample.iin_ua > session.peak_iin_ua {
                    session.peak_iin_ua = sample.iin_ua;
                }
                if sample.die_temp_dc > session.peak_die_temp_dc {
                    session.peak_die_temp_dc = sample.die_temp_dc;
                }
                match sample.op_mode {
                    OpMode::Switching => session.saw_switching = true,
                    OpMode::Bypass => session.saw_bypass = true,
                    _ => {}
                }
            }
        } else if let Some(mut session) = self.current.take() {
            session.ended_ms = Some(sample.ts_ms);
            self.store_session(session);
        }
    }

    /// Current (open) session.
    #[must_use]
    pub const fn current(&self) -> Option<&ChargeSession> {
        self.current.as_ref()
    }

    /// Last completed session.
    #[must_use]
    pub fn last_completed(&self) -> Option<&ChargeSession> {
        let index = if self.session_next == 0 {
            SESSION_HISTORY.saturating_sub(1)
        } else {
            self.session_next.saturating_sub(1)
        };
        self.sessions.get(index).and_then(Option::as_ref)
    }

    /// Total sessions started (including the current one).
    #[must_use]
    pub const fn session_total(&self) -> u64 {
        self.session_total
    }

    /// Total samples accepted.
    #[must_use]
    pub const fn sample_total(&self) -> u64 {
        self.sample_total
    }

    /// Last sample.
    #[must_use]
    pub fn last_sample(&self) -> Option<&TelemetrySample> {
        let index = if self.sample_next == 0 {
            SAMPLE_RING.saturating_sub(1)
        } else {
            self.sample_next.saturating_sub(1)
        };
        self.samples.get(index).and_then(Option::as_ref)
    }

    /// Calls the closure for every stored sample in arrival order.
    pub fn for_each_sample(&self, mut f: impl FnMut(&TelemetrySample)) {
        for index in 0..SAMPLE_RING {
            let position = (self.sample_next.saturating_add(index)) % SAMPLE_RING;
            if let Some(sample) = self.samples.get(position).and_then(Option::as_ref) {
                f(sample);
            }
        }
    }

    fn store_session(&mut self, session: ChargeSession) {
        if let Some(slot) = self.sessions.get_mut(self.session_next) {
            *slot = Some(session);
        }
        self.session_next = (self.session_next.saturating_add(1)) % SESSION_HISTORY;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(ts_ms: u64, present: bool, iin_ua: u32, temp_dc: i32) -> TelemetrySample {
        TelemetrySample {
            ts_ms,
            vbat_uv: 4_300_000,
            vbus_uv: 9_000_000,
            iin_ua,
            die_temp_dc: temp_dc,
            op_mode: if present {
                OpMode::Switching
            } else {
                OpMode::Standby
            },
            input_present: present,
            vbat_valid: true,
            die_temp_valid: true,
        }
    }

    #[test]
    fn session_opens_and_closes_on_power_change() {
        let mut telemetry = Telemetry::new();
        telemetry.push(sample(1_000, true, 500_000, 300));
        telemetry.push(sample(2_000, true, 1_500_000, 420));
        let current = telemetry.current().expect("session is running");
        assert_eq!(current.samples, 2);
        assert_eq!(current.peak_iin_ua, 1_500_000);
        assert_eq!(current.peak_die_temp_dc, 420);
        assert!(current.had_fast_mode());

        telemetry.push(sample(3_000, false, 0, 400));
        assert!(telemetry.current().is_none());
        let closed = telemetry.last_completed().expect("session closed");
        assert_eq!(closed.started_ms, 1_000);
        assert_eq!(closed.ended_ms, Some(3_000));
        assert_eq!(closed.duration_ms(9_999), 2_000);
        assert_eq!(telemetry.session_total(), 1);
    }

    #[test]
    fn samples_survive_rotation() {
        let mut telemetry = Telemetry::new();
        for index in 0..(SAMPLE_RING + 20) {
            let ts = u64::try_from(index).unwrap_or(0);
            telemetry.push(sample(ts, true, 100_000, 300));
        }
        assert_eq!(telemetry.sample_total(), (SAMPLE_RING + 20) as u64);
        let mut count = 0;
        telemetry.for_each_sample(|_| count += 1);
        assert_eq!(count, SAMPLE_RING);
        assert!(telemetry.last_sample().is_some());
    }

    #[test]
    fn session_history_rotates() {
        let mut telemetry = Telemetry::new();
        for index in 0..(SESSION_HISTORY + 5) {
            let base = u64::try_from(index).unwrap_or(0).saturating_mul(10);
            telemetry.push(sample(base, true, 200_000, 300));
            telemetry.push(sample(base + 5, false, 0, 300));
        }
        assert_eq!(telemetry.session_total(), (SESSION_HISTORY + 5) as u64);
        assert!(telemetry.last_completed().is_some());
    }

    #[test]
    fn bypass_is_recorded_separately_from_switching() {
        let mut telemetry = Telemetry::new();
        let mut plane = sample(1, true, 500_000, 300);
        plane.op_mode = OpMode::Bypass;
        telemetry.push(plane);
        let session = telemetry.current().expect("session");
        assert!(session.saw_bypass);
        assert!(!session.saw_switching);
        assert!(!session.had_fast_mode());
    }
}
