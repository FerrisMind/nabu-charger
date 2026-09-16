//! Кодирование лимита входного тока в значение регистра.
//!
//! В SMB лимит задаётся не в микроамперax, а номером ступени: значение
//! приводится к сетке `min + n * step`. Константы взяты из эталонного драйвера
//! Android (`DCIN_ICL_MIN_UA`, `DCIN_ICL_STEP_UA`, `USBIN_100MA`, `USBIN_500MA`).
//!
//! Точная разрядность поля зависит от ревизии кристалла, поэтому сетка вынесена
//! в структуру [`IclEncoding`] и может быть переопределена вызывающей стороной.
//! Ядро всегда проверяет запись чтением (см. `ChargerConfig::verify_writes`).

use crate::error::ChargerError;

/// Минимальный лимит входного тока по сетке SMB.
pub const ICL_MIN_UA: u32 = 100_000;

/// Шаг сетки лимита входного тока.
pub const ICL_STEP_UA: u32 = 100_000;

/// Верхняя граница поля лимита в регистре (32 ступени).
pub const ICL_RAW_MAX: u8 = 0x1F;

/// Сетка кодирования лимита входного тока.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IclEncoding {
    /// Нижняя ступень сетки в микроамперax.
    pub min_ua: u32,
    /// Шаг сетки в микроамперax.
    pub step_ua: u32,
    /// Максимальное значение кода.
    pub raw_max: u8,
}

impl Default for IclEncoding {
    fn default() -> Self {
        Self {
            min_ua: ICL_MIN_UA,
            step_ua: ICL_STEP_UA,
            raw_max: ICL_RAW_MAX,
        }
    }
}

impl IclEncoding {
    /// Максимальный ток, представимый этой сеткой.
    #[must_use]
    pub const fn max_ua(&self) -> u32 {
        self.min_ua
            .saturating_add(self.step_ua.saturating_mul(self.raw_max as u32))
    }

    /// Переводит ток в код регистра.
    ///
    /// Ток округляется **вниз** до ближайшей ступени: занизить ток безопасно,
    /// завысить — значит перегрузить порт адаптера.
    ///
    /// # Errors
    ///
    /// [`ChargerError::CurrentOutOfRange`], если ток ниже сетки или выше её
    /// максимума, либо если шаг сетки нулевой.
    pub fn encode(&self, icl_ua: u32) -> Result<u8, ChargerError> {
        let out_of_range = ChargerError::CurrentOutOfRange {
            requested_ua: icl_ua,
            max_ua: self.max_ua(),
        };
        if self.step_ua == 0 || icl_ua < self.min_ua {
            return Err(out_of_range);
        }
        let Some(steps) = icl_ua.saturating_sub(self.min_ua).checked_div(self.step_ua) else {
            return Err(out_of_range);
        };
        if steps > u32::from(self.raw_max) {
            return Err(out_of_range);
        }
        // steps уже сверен с raw_max (<= 255), поэтому преобразование не теряет данные.
        u8::try_from(steps).map_err(|_| out_of_range)
    }

    /// Переводит код регистра обратно в микроамперы.
    #[must_use]
    pub fn decode(&self, raw: u8) -> u32 {
        self.min_ua
            .saturating_add(self.step_ua.saturating_mul(u32::from(raw)))
    }

    /// Приводит произвольный ток к сетке вниз, не выходя за нижнюю границу.
    #[must_use]
    pub fn quantize_down(&self, icl_ua: u32) -> u32 {
        if self.step_ua == 0 || icl_ua <= self.min_ua {
            return self.min_ua;
        }
        let Some(steps) = icl_ua.saturating_sub(self.min_ua).checked_div(self.step_ua) else {
            return self.min_ua;
        };
        let capped = if steps > u32::from(self.raw_max) {
            self.raw_max
        } else {
            u8::try_from(steps).unwrap_or(self.raw_max)
        };
        self.decode(capped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_and_decodes_round_trip() {
        let grid = IclEncoding::default();
        for raw in 0..=ICL_RAW_MAX {
            let ua = grid.decode(raw);
            assert_eq!(grid.encode(ua).expect("код должен кодироваться"), raw);
        }
    }

    #[test]
    fn quantizes_down_to_the_grid() {
        let grid = IclEncoding::default();
        assert_eq!(grid.quantize_down(3_000_000), 3_000_000);
        assert_eq!(grid.quantize_down(3_050_000), 3_000_000);
        assert_eq!(grid.quantize_down(500_000), 500_000);
        assert_eq!(grid.quantize_down(10_000), ICL_MIN_UA);
    }

    #[test]
    fn rejects_current_below_the_grid() {
        let grid = IclEncoding::default();
        let err = grid.encode(50_000).expect_err("ниже сетки");
        assert!(matches!(
            err,
            crate::ChargerError::CurrentOutOfRange {
                requested_ua: 50_000,
                ..
            }
        ));
    }

    #[test]
    fn rejects_current_above_the_grid() {
        let grid = IclEncoding::default();
        let err = grid
            .encode(grid.max_ua().saturating_add(100_000))
            .expect_err("выше сетки");
        assert!(matches!(err, crate::ChargerError::CurrentOutOfRange { .. }));
    }

    #[test]
    fn zero_step_is_rejected() {
        let grid = IclEncoding {
            min_ua: 100_000,
            step_ua: 0,
            raw_max: 31,
        };
        assert!(grid.encode(500_000).is_err());
        assert_eq!(grid.quantize_down(500_000), ICL_MIN_UA);
    }
}
