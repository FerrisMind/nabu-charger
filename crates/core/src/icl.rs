//! Encoding of the input current limit into a register value.
//!
//! In SMB the limit is set not in microamperes but as a step number: the value is
//! mapped onto the grid `min + n * step`. The constants are taken from the reference
//! Android driver (`DCIN_ICL_MIN_UA`, `DCIN_ICL_STEP_UA`, `USBIN_100MA`, `USBIN_500MA`).
//!
//! The exact field width depends on the chip revision, so the grid is moved into the
//! [`IclEncoding`] structure and can be overridden by the caller.
//! The core always verifies a write by reading it back (see `ChargerConfig::verify_writes`).

use crate::error::ChargerError;

/// Minimum input current limit on the SMB grid.
pub const ICL_MIN_UA: u32 = 100_000;

/// Input current limit grid step.
pub const ICL_STEP_UA: u32 = 100_000;

/// Upper bound of the limit field in the register (32 steps).
pub const ICL_RAW_MAX: u8 = 0x1F;

/// Encoding grid for the input current limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IclEncoding {
    /// Lowest grid step in microamperes.
    pub min_ua: u32,
    /// Grid step in microamperes.
    pub step_ua: u32,
    /// Maximum code value.
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
    /// Maximum current representable by this grid.
    #[must_use]
    pub const fn max_ua(&self) -> u32 {
        self.min_ua
            .saturating_add(self.step_ua.saturating_mul(self.raw_max as u32))
    }

    /// Converts a current into a register code.
    ///
    /// The current is rounded **down** to the nearest step: lowering the current is
    /// safe, raising it means overloading the adapter port.
    ///
    /// # Errors
    ///
    /// [`ChargerError::CurrentOutOfRange`] if the current is below the grid or above
    /// its maximum, or if the grid step is zero.
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
        // steps has already been checked against raw_max (<= 255), so the cast loses no data.
        u8::try_from(steps).map_err(|_| out_of_range)
    }

    /// Converts a register code back into microamperes.
    #[must_use]
    pub fn decode(&self, raw: u8) -> u32 {
        self.min_ua
            .saturating_add(self.step_ua.saturating_mul(u32::from(raw)))
    }

    /// Rounds an arbitrary current down to the grid, without going below the lower bound.
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
            assert_eq!(grid.encode(ua).expect("the code must encode"), raw);
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
        let err = grid.encode(50_000).expect_err("below the grid");
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
            .expect_err("above the grid");
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
