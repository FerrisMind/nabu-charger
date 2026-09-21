//! Input current policy: how much may be drawn from the identified adapter.
//!
//! The constants are taken from the reference Android driver
//! (`qcom/smb5-lib.h`, `ti/cp_qc30.h`, branch 16.0). Currents are in microamperes.

use crate::apsd::AdapterType;

/// Minimum input current limit (`DCIN_ICL_MIN_UA` in Android).
pub const MIN_ICL_UA: u32 = 100_000;

/// Upper bound the core allows to apply (`DCIN_ICL_MAX_UA` plus margin).
pub const MAX_ICL_UA: u32 = 4_500_000;

/// Current of a standard USB 2.0/3.x port: 500 mA.
pub const ICL_SDP_UA: u32 = 500_000;
/// Charging port current per BC1.2: 1.5 A.
pub const ICL_DCP_UA: u32 = 1_500_000;
/// Charging port with data current per BC1.2: 1.5 A.
pub const ICL_CDP_UA: u32 = 1_500_000;
/// Current for HVDCP2 (`HVDCP2_CURRENT_UA`).
pub const ICL_HVDCP2_UA: u32 = 1_500_000;
/// Current for HVDCP3 (`HVDCP_CURRENT_UA`).
pub const ICL_HVDCP3_UA: u32 = 3_000_000;
/// Current for HVDCP3.5 on the SMB side (single switch limit).
pub const ICL_HVDCP3P5_SMB_UA: u32 = 3_000_000;
/// Rated current of the own power brick through the charge pump (`HVDCP3P5_40W_CURRENT_UA`).
pub const HVDCP3P5_PUMP_BUS_UA: u32 = 4_500_000;

/// Voltage requested from the adapter in QC2 mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Qc2Voltage {
    /// 5 V.
    V5,
    /// 9 V.
    V9,
    /// 12 V.
    V12,
}

impl Qc2Voltage {
    /// Value of bits 7:6 of the `HVDCP_PULSE_COUNT_MAX` register.
    #[must_use]
    pub const fn raw(self) -> u8 {
        match self {
            Self::V5 => 0x00,
            Self::V9 => 0x40,
            Self::V12 => 0x80,
        }
    }

    /// Label for the journal.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::V5 => "5V",
            Self::V9 => "9V",
            Self::V12 => "12V",
        }
    }
}

/// Quick Charge 3.5 support state on this platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Qc35Support {
    /// The platform does not support QC3.5; the type is downgraded to HVDCP3/HVDCP2.
    Unsupported,
    /// The platform supports QC3.5; `authenticated` says whether authentication passed.
    Supported {
        /// QC3.5 authentication result.
        authenticated: bool,
    },
}

impl Default for Qc35Support {
    fn default() -> Self {
        // The nabu tablet supports QC3.5, but authentication has not passed by default.
        Self::Supported {
            authenticated: false,
        }
    }
}

/// Final decision for the identified adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChargePolicy {
    /// Input current limit in microamperes.
    pub icl_ua: u32,
    /// Whether the adapter voltage must be raised (QC2).
    pub qc2_voltage: Option<Qc2Voltage>,
    /// Whether the charge pump may be attached as a second stage.
    pub pump_eligible: bool,
    /// Rationale for the journal.
    pub rationale: &'static str,
}

/// Computes the policy for an adapter type.
///
/// For an unknown source the minimum safe current is applied: slow charging is
/// better than overloading the port.
#[must_use]
pub fn policy_for(adapter: AdapterType, qc35: Qc35Support) -> ChargePolicy {
    match adapter {
        AdapterType::Sdp => ChargePolicy {
            icl_ua: ICL_SDP_UA,
            qc2_voltage: None,
            pump_eligible: false,
            rationale: "standard USB port, 500 mA limit",
        },
        AdapterType::Cdp => ChargePolicy {
            icl_ua: ICL_CDP_UA,
            qc2_voltage: None,
            pump_eligible: false,
            rationale: "charging port with data, BC1.2 1.5 A",
        },
        AdapterType::Dcp => ChargePolicy {
            icl_ua: ICL_DCP_UA,
            qc2_voltage: None,
            pump_eligible: false,
            rationale: "charging-only port, BC1.2 1.5 A",
        },
        AdapterType::Ocp | AdapterType::Float | AdapterType::Unknown => ChargePolicy {
            icl_ua: MIN_ICL_UA,
            qc2_voltage: None,
            pump_eligible: false,
            rationale: "source not identified, minimum safe current",
        },
        AdapterType::Hvdcp2 => ChargePolicy {
            icl_ua: ICL_HVDCP2_UA,
            qc2_voltage: Some(Qc2Voltage::V9),
            pump_eligible: false,
            rationale: "Quick Charge 2.0, 9 V and 1.5 A",
        },
        AdapterType::Hvdcp3 => ChargePolicy {
            icl_ua: ICL_HVDCP3_UA,
            qc2_voltage: Some(Qc2Voltage::V9),
            pump_eligible: true,
            rationale: "Quick Charge 3.0, 9 V and 3 A, charge pump possible",
        },
        AdapterType::Hvdcp3P5 => {
            let pump_eligible = matches!(
                qc35,
                Qc35Support::Supported {
                    authenticated: true
                }
            );
            ChargePolicy {
                icl_ua: ICL_HVDCP3P5_SMB_UA,
                qc2_voltage: Some(Qc2Voltage::V9),
                pump_eligible,
                rationale: "Quick Charge 3.5, the tablet's own power brick",
            }
        }
    }
}

/// Bounds the current limit from above by the values from the core configuration.
///
/// # Errors
///
/// [`crate::ChargerError::CurrentOutOfRange`] - if the current is below the minimum
/// or above the allowed maximum.
pub fn clamp_icl(icl_ua: u32, max_ua: u32) -> Result<u32, crate::error::ChargerError> {
    if icl_ua < MIN_ICL_UA {
        return Err(crate::error::ChargerError::CurrentOutOfRange {
            requested_ua: icl_ua,
            max_ua,
        });
    }
    if icl_ua > max_ua {
        return Err(crate::error::ChargerError::CurrentOutOfRange {
            requested_ua: icl_ua,
            max_ua,
        });
    }
    Ok(icl_ua)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apsd::AdapterType;

    #[test]
    fn policy_table_matches_reference_driver() {
        let qc35 = Qc35Support::Unsupported;
        assert_eq!(policy_for(AdapterType::Sdp, qc35).icl_ua, ICL_SDP_UA);
        assert_eq!(policy_for(AdapterType::Cdp, qc35).icl_ua, ICL_CDP_UA);
        assert_eq!(policy_for(AdapterType::Dcp, qc35).icl_ua, ICL_DCP_UA);
        assert_eq!(policy_for(AdapterType::Hvdcp2, qc35).icl_ua, ICL_HVDCP2_UA);
        assert_eq!(policy_for(AdapterType::Hvdcp3, qc35).icl_ua, ICL_HVDCP3_UA);
    }

    #[test]
    fn unknown_source_gets_minimal_current() {
        let policy = policy_for(AdapterType::Unknown, Qc35Support::default());
        assert_eq!(policy.icl_ua, MIN_ICL_UA);
        assert!(policy.qc2_voltage.is_none());
        assert!(!policy.pump_eligible);
    }

    #[test]
    fn hvdcp_requests_nine_volts() {
        let policy = policy_for(AdapterType::Hvdcp3, Qc35Support::default());
        assert_eq!(policy.qc2_voltage, Some(Qc2Voltage::V9));
        assert_eq!(Qc2Voltage::V9.raw(), 0x40);
        assert_eq!(Qc2Voltage::V12.raw(), 0x80);
        assert_eq!(Qc2Voltage::V5.raw(), 0x00);
    }

    #[test]
    fn pump_requires_authentication_for_qc35() {
        let authenticated = policy_for(
            AdapterType::Hvdcp3P5,
            Qc35Support::Supported {
                authenticated: true,
            },
        );
        let plain = policy_for(
            AdapterType::Hvdcp3P5,
            Qc35Support::Supported {
                authenticated: false,
            },
        );
        assert!(authenticated.pump_eligible);
        assert!(!plain.pump_eligible);
    }

    #[test]
    fn clamp_rejects_out_of_range() {
        assert!(clamp_icl(500_000, MAX_ICL_UA).is_ok());
        assert!(clamp_icl(50_000, MAX_ICL_UA).is_err());
        assert!(clamp_icl(MAX_ICL_UA.saturating_add(1), MAX_ICL_UA).is_err());
    }
}
