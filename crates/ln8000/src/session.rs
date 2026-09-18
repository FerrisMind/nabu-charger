//! Телеметрия и журнал сеансов заряда.
//!
//! Модуль ведёт две вещи одновременно:
//!
//! * **кольцевой буфер отсчётов** — последние измерения (напряжения, ток,
//!   температура, режим), годные для графика и для разбора инцидента;
//! * **сеансы заряда** — интервалы, когда на входе есть питание; для каждого
//!   хранятся длительность, пиковый ток, пиковая температура и признак того,
//!   что устройство работало в режиме 2:1.
//!
//! Ничего не аллоцируется: буферы фиксированного размера, поэтому модуль
//! пригоден и для `no_std`-драйвера, и для хостовых тестов.

use crate::encoding::OpMode;

/// Размер кольцевого буфера отсчётов.
pub const SAMPLE_RING: usize = 256;
/// Сколько завершённых сеансов хранится в памяти.
pub const SESSION_HISTORY: usize = 32;

/// Один отсчёт телеметрии.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TelemetrySample {
    /// Метка времени в миллисекундах монотонных часов.
    pub ts_ms: u64,
    /// Напряжение батареи, мкВ.
    pub vbat_uv: u32,
    /// Напряжение входа, мкВ.
    pub vbus_uv: u32,
    /// Входной ток, мкА.
    pub iin_ua: u32,
    /// Температура кристалла, десятые доли °C.
    pub die_temp_dc: i32,
    /// Режим работы в момент отсчёта.
    pub op_mode: OpMode,
    /// Есть ли питание на входе.
    pub input_present: bool,
    /// Достоверно ли [`Self::vbat_uv`] (канал АЦП прочитан).
    ///
    /// Отказ чтения даёт ноль, который неотличим от настоящего нуля: по такому
    /// отсчёту нельзя ни резать ток, ни возвращать лимит к профильному.
    pub vbat_valid: bool,
    /// Достоверна ли [`Self::die_temp_dc`] (канал АЦП прочитан).
    ///
    /// Ноль градусов — это и «холодно», и «нет данных»: защита обязана различать
    /// эти случаи, иначе возврат тока сработает по мусору, а `Stop` — по нулю.
    pub die_temp_valid: bool,
}

/// Сеанс заряда: интервал, когда на входе было питание.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChargeSession {
    /// Начало сеанса, мс.
    pub started_ms: u64,
    /// Конец сеанса, мс (`None`, пока сеанс идёт).
    pub ended_ms: Option<u64>,
    /// Сколько отсчётов попало в сеанс.
    pub samples: u32,
    /// Пиковый входной ток, мкА.
    pub peak_iin_ua: u32,
    /// Пиковая температура кристалла, десятые °C.
    pub peak_die_temp_dc: i32,
    /// Видели ли режим 2:1 (ускоренная зарядка).
    pub saw_switching: bool,
    /// Видели ли режим bypass 1:1.
    pub saw_bypass: bool,
}

impl ChargeSession {
    /// Пустой сеанс, начавшийся в `started_ms`.
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

    /// Длительность сеанса в миллисекундах (для идущего — до `now_ms`).
    #[must_use]
    pub const fn duration_ms(&self, now_ms: u64) -> u64 {
        match self.ended_ms {
            Some(end) => end.saturating_sub(self.started_ms),
            None => now_ms.saturating_sub(self.started_ms),
        }
    }

    /// Был ли за сеанс хоть один момент ускоренной зарядки.
    #[must_use]
    pub const fn had_fast_mode(&self) -> bool {
        self.saw_switching
    }
}

/// Хранилище телеметрии: кольцо отсчётов плюс история сеансов.
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
    /// Создаёт пустую телеметрию.
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

    /// Принимает отсчёт и обновляет сеанс.
    ///
    /// Сеанс начинается, когда появляется питание, и закрывается, когда оно
    /// пропадает. Закрытый сеанс уходит в кольцо истории с ротацией.
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

    /// Текущий (незакрытый) сеанс.
    #[must_use]
    pub const fn current(&self) -> Option<&ChargeSession> {
        self.current.as_ref()
    }

    /// Последний завершённый сеанс.
    #[must_use]
    pub fn last_completed(&self) -> Option<&ChargeSession> {
        let index = if self.session_next == 0 {
            SESSION_HISTORY.saturating_sub(1)
        } else {
            self.session_next.saturating_sub(1)
        };
        self.sessions.get(index).and_then(Option::as_ref)
    }

    /// Всего начатых сеансов (включая текущий).
    #[must_use]
    pub const fn session_total(&self) -> u64 {
        self.session_total
    }

    /// Всего принятых отсчётов.
    #[must_use]
    pub const fn sample_total(&self) -> u64 {
        self.sample_total
    }

    /// Последний отсчёт.
    #[must_use]
    pub fn last_sample(&self) -> Option<&TelemetrySample> {
        let index = if self.sample_next == 0 {
            SAMPLE_RING.saturating_sub(1)
        } else {
            self.sample_next.saturating_sub(1)
        };
        self.samples.get(index).and_then(Option::as_ref)
    }

    /// Вызывает замыкание для каждого сохранённого отсчёта в порядке поступления.
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
        let current = telemetry.current().expect("сеанс идёт");
        assert_eq!(current.samples, 2);
        assert_eq!(current.peak_iin_ua, 1_500_000);
        assert_eq!(current.peak_die_temp_dc, 420);
        assert!(current.had_fast_mode());

        telemetry.push(sample(3_000, false, 0, 400));
        assert!(telemetry.current().is_none());
        let closed = telemetry.last_completed().expect("сеанс закрыт");
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
        let session = telemetry.current().expect("сеанс");
        assert!(session.saw_bypass);
        assert!(!session.saw_switching);
        assert!(!session.had_fast_mode());
    }
}
