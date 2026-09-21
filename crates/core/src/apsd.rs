//! Parsing of the hardware adapter detection (APSD) result.
//!
//! The logic mirrors the `smblib_get_apsd_result()` function of the reference Android
//! driver (`qcom/smb5-lib.c`, branch 16.0, configuration `CONFIG_MACH_XIAOMI_NABU`),
//! so that under Windows the adapter type is identified the same way as under Android.

use crate::error::ChargerError;
use crate::policy::Qc35Support;
use crate::regs;

/// Type of the connected power adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AdapterType {
    /// No power, or the type is not identified.
    Unknown,
    /// Standard USB port (500 mA limit).
    Sdp,
    /// Other charging port.
    Ocp,
    /// Charging port with data.
    Cdp,
    /// Charging-only port.
    Dcp,
    /// Non-standard source (FLOAT).
    Float,
    /// Quick Charge 2.0.
    Hvdcp2,
    /// Quick Charge 3.0.
    Hvdcp3,
    /// Quick Charge 3.5 (the mode used by the tablet's own power brick).
    Hvdcp3P5,
}

impl AdapterType {
    /// Stable short name for the journal and tables.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Unknown => "UNKNOWN",
            Self::Sdp => "SDP",
            Self::Ocp => "OCP",
            Self::Cdp => "CDP",
            Self::Dcp => "DCP",
            Self::Float => "FLOAT",
            Self::Hvdcp2 => "HVDCP2",
            Self::Hvdcp3 => "HVDCP3",
            Self::Hvdcp3P5 => "HVDCP3P5",
        }
    }

    /// Whether the adapter is a fast one (Quick Charge).
    #[must_use]
    pub const fn is_hvdcp(self) -> bool {
        matches!(self, Self::Hvdcp2 | Self::Hvdcp3 | Self::Hvdcp3P5)
    }

    /// Pattern the hardware sets in `APSD_RESULT_STATUS`.
    ///
    /// There is no separate pattern for [`AdapterType::Hvdcp3P5`]: the hardware reports
    /// HVDCP3, and version 3.5 is confirmed by separate authentication, so the HVDCP3
    /// pattern is returned.
    #[must_use]
    pub const fn apsd_pattern(self) -> u8 {
        match self {
            Self::Unknown => 0,
            Self::Sdp => regs::PATTERN_SDP,
            Self::Ocp => regs::PATTERN_OCP,
            Self::Cdp => regs::PATTERN_CDP,
            Self::Dcp => regs::PATTERN_DCP,
            Self::Float => regs::PATTERN_FLOAT,
            Self::Hvdcp2 => regs::PATTERN_HVDCP2,
            Self::Hvdcp3 | Self::Hvdcp3P5 => regs::PATTERN_HVDCP3,
        }
    }

    /// Parses a pattern without considering the Quick Charge flag.
    ///
    /// Returns [`ChargerError::UnknownAdapterPattern`] if the combination of bits is
    /// not described by the hardware.
    ///
    /// # Errors
    ///
    /// [`ChargerError::UnknownAdapterPattern`] - if the value matched none of the
    /// known patterns.
    pub fn from_pattern(raw: u8) -> Result<Self, ChargerError> {
        match raw {
            0 => Ok(Self::Unknown),
            regs::PATTERN_SDP => Ok(Self::Sdp),
            regs::PATTERN_OCP => Ok(Self::Ocp),
            regs::PATTERN_CDP => Ok(Self::Cdp),
            regs::PATTERN_DCP => Ok(Self::Dcp),
            regs::PATTERN_FLOAT => Ok(Self::Float),
            regs::PATTERN_HVDCP2 => Ok(Self::Hvdcp2),
            regs::PATTERN_HVDCP3 => Ok(Self::Hvdcp3),
            other => Err(ChargerError::UnknownAdapterPattern { raw: other }),
        }
    }

    /// Full parse of the `APSD_STATUS` and `APSD_RESULT_STATUS` register pair.
    ///
    /// Mirrors `smblib_get_apsd_result()`:
    ///
    /// 1. the "detection complete" bit is required, else [`ChargerError::DetectionNotComplete`];
    /// 2. the HVDCP check timeout bit gives [`ChargerError::AdapterCheckTimeout`];
    /// 3. the pattern from `APSD_RESULT_STATUS` is mapped to a base type;
    /// 4. if the Quick Charge bit is set, the base type is refined: HVDCP3 stays
    ///    HVDCP3 (or becomes HVDCP3P5 when authentication is confirmed),
    ///    everything else is treated as HVDCP2.
    ///
    /// # Errors
    ///
    /// * [`ChargerError::DetectionNotComplete`] - detection is still running.
    /// * [`ChargerError::AdapterCheckTimeout`] - the hardware recorded an HVDCP timeout.
    /// * [`ChargerError::UnknownAdapterPattern`] - unknown combination of bits.
    pub fn decode(
        apsd_status: u8,
        apsd_result: u8,
        qc35: Qc35Support,
    ) -> Result<Self, ChargerError> {
        if apsd_status & regs::APSD_DTC_STATUS_DONE == 0 {
            return Err(ChargerError::DetectionNotComplete);
        }
        if apsd_status & regs::HVDCP_CHECK_TIMEOUT != 0 {
            return Err(ChargerError::AdapterCheckTimeout {
                raw_status: apsd_status,
            });
        }

        let raw = apsd_result & regs::APSD_RESULT_STATUS_MASK;
        let base = Self::from_pattern(raw)?;
        if apsd_status & regs::QC_CHARGER == 0 {
            return Ok(base);
        }

        // QC3.5 is confirmed by separate authentication; without it we honestly report HVDCP3.
        let resolved = match base {
            Self::Hvdcp3P5 => Self::Hvdcp3P5,
            Self::Hvdcp3 => {
                if matches!(
                    qc35,
                    Qc35Support::Supported {
                        authenticated: true
                    }
                ) {
                    Self::Hvdcp3P5
                } else {
                    Self::Hvdcp3
                }
            }
            // The QC flag is set but the pattern is neither HVDCP3.5 nor HVDCP3,
            // so what is attached is a Quick Charge 2 adapter.
            _ => Self::Hvdcp2,
        };
        Ok(resolved)
    }

    /// Whether the port is charging as seen by the operating system.
    ///
    /// Any type other than [`AdapterType::Unknown`] means a source is present.
    #[must_use]
    pub const fn is_power_present(self) -> bool {
        !matches!(self, Self::Unknown)
    }
}

impl core::fmt::Display for AdapterType {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.label())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Qc35Support;

    #[test]
    fn decodes_base_patterns() {
        let done = regs::APSD_DTC_STATUS_DONE;
        let qc35 = Qc35Support::Unsupported;
        assert_eq!(
            AdapterType::decode(done, regs::PATTERN_SDP, qc35).expect("SDP"),
            AdapterType::Sdp
        );
        assert_eq!(
            AdapterType::decode(done, regs::PATTERN_DCP, qc35).expect("DCP"),
            AdapterType::Dcp
        );
        assert_eq!(
            AdapterType::decode(done, regs::PATTERN_CDP, qc35).expect("CDP"),
            AdapterType::Cdp
        );
        assert_eq!(
            AdapterType::decode(done, regs::PATTERN_FLOAT, qc35).expect("FLOAT"),
            AdapterType::Float
        );
    }

    #[test]
    fn qc_bit_upgrades_type() {
        let done = regs::APSD_DTC_STATUS_DONE | regs::QC_CHARGER;
        assert_eq!(
            AdapterType::decode(done, regs::PATTERN_HVDCP2, Qc35Support::Unsupported)
                .expect("HVDCP2"),
            AdapterType::Hvdcp2
        );
        assert_eq!(
            AdapterType::decode(done, regs::PATTERN_HVDCP3, Qc35Support::Unsupported)
                .expect("HVDCP3"),
            AdapterType::Hvdcp3
        );
        // The QC flag with an unknown base type is treated as HVDCP2.
        assert_eq!(
            AdapterType::decode(done, regs::PATTERN_SDP, Qc35Support::Unsupported).expect("HVDCP2"),
            AdapterType::Hvdcp2
        );
    }

    #[test]
    fn qc35_authentication_promotes_type() {
        let done = regs::APSD_DTC_STATUS_DONE | regs::QC_CHARGER;
        assert_eq!(
            AdapterType::decode(
                done,
                regs::PATTERN_HVDCP3,
                Qc35Support::Supported {
                    authenticated: true
                }
            )
            .expect("HVDCP3.5"),
            AdapterType::Hvdcp3P5
        );
        assert_eq!(
            AdapterType::decode(
                done,
                regs::PATTERN_HVDCP3,
                Qc35Support::Supported {
                    authenticated: false
                }
            )
            .expect("HVDCP3"),
            AdapterType::Hvdcp3
        );
    }

    #[test]
    fn incomplete_detection_is_reported() {
        let err = AdapterType::decode(0, regs::PATTERN_DCP, Qc35Support::Unsupported)
            .expect_err("detection not complete");
        assert!(matches!(err, crate::ChargerError::DetectionNotComplete));
    }

    #[test]
    fn hvdcp_check_timeout_is_reported() {
        let status = regs::APSD_DTC_STATUS_DONE | regs::HVDCP_CHECK_TIMEOUT;
        let err = AdapterType::decode(status, regs::PATTERN_HVDCP3, Qc35Support::Unsupported)
            .expect_err("adapter is unstable");
        assert!(matches!(
            err,
            crate::ChargerError::AdapterCheckTimeout { .. }
        ));
    }

    #[test]
    fn unknown_pattern_is_reported() {
        let err = AdapterType::decode(regs::APSD_DTC_STATUS_DONE, 0x3F, Qc35Support::Unsupported)
            .expect_err("unknown pattern");
        assert!(matches!(
            err,
            crate::ChargerError::UnknownAdapterPattern { raw: 0x3F }
        ));
    }

    #[test]
    fn helper_predicates_match_table() {
        assert!(AdapterType::Hvdcp3P5.is_hvdcp());
        assert!(!AdapterType::Dcp.is_hvdcp());
        assert!(AdapterType::Sdp.is_power_present());
        assert!(!AdapterType::Unknown.is_power_present());
        assert_eq!(AdapterType::Hvdcp3.apsd_pattern(), regs::PATTERN_HVDCP3);
        assert_eq!(AdapterType::Hvdcp3P5.label(), "HVDCP3P5");
    }
}
