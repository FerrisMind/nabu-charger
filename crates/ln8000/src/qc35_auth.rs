//! Pure QC3.5 authenticate policy (Android `qc3p5_authenticate` / smb5-lib).
//!
//! Host-testable: classifies `SRC_CAP` power, gates auth vs QC3 fallback, and
//! exposes the `+-+-+-` / `++−−` pulse sequences as data. KMDF executes the
//! SPMI INC/DEC IO; AFC remains unsupported on nabu.

/// Gap between QC3.5 auth INC/DEC pulses (ms), Android `usleep_range(5000,5010)`.
pub const QC35_AUTH_PULSE_GAP_MS: u32 = 5;
/// `QC3P5_T_TA_DETECTION_TIMEOUT_PMIC_MS` — wait for ~6 V detect window.
pub const QC35_DETECT_TIMEOUT_MS: u32 = 200;
/// `QC3P5_T_TA_CAP_TIMEOUT_PMIC_MS` — wait for `SRC_CAP` VBUS after `+-+-+-`.
pub const QC35_CAP_TIMEOUT_MS: u32 = 250;
/// Poll period while waiting for QC3.5 VBUS windows (ms).
pub const QC35_VIN_POLL_MS: u32 = 20;
/// Soft INC budget to enter the 5.5–6.4 V detect window from ~5 V.
pub const QC35_PREP_MAX_INC: u32 = 8;
/// Adapter-side fine step (µV), `HVDCP3P5_STEP_UV`.
pub const QC35_STEP_UV: u32 = 20_000;

/// `VBUS_5P5_V_UV` — low edge of post-QC3 auth detect window.
pub const QC35_DETECT_LO_UV: i32 = 5_500_000;
/// `VBUS_6P4_V_UV` — high edge of detect window (Vin ≥ this → skip auth).
pub const QC35_DETECT_HI_UV: i32 = 6_400_000;
/// `VBUS_6P65_V_UV` — `SRC_CAP` low (also 18 W class low).
pub const QC35_CAP_LO_UV: i32 = 6_650_000;
/// `VBUS_9P8_V_UV` — `SRC_CAP` high (40 W class high).
pub const QC35_CAP_HI_UV: i32 = 9_800_000;
/// 18 W `SRC_CAP` high (`VBUS_7P35_V_UV`).
pub const QC35_18W_HI_UV: i32 = 7_350_000;
/// 27 W `SRC_CAP` low (`VBUS_7P6_V_UV`).
pub const QC35_27W_LO_UV: i32 = 7_600_000;
/// 27 W `SRC_CAP` high (`VBUS_8P4_V_UV`).
pub const QC35_27W_HI_UV: i32 = 8_400_000;
/// 40 W `SRC_CAP` low (`VBUS_8P55_V_UV`).
pub const QC35_40W_LO_UV: i32 = 8_550_000;

/// Android nabu `QC3P5_CHARGER_ICL` (2 A, step 50 mA → code 40) for 18/27 W.
pub const ICL_RAW_QC35_2A: u8 = 40;
/// USBIN ICL for 40 W class (3 A, step 50 mA → code 60).
pub const ICL_RAW_QC35_40W: u8 = 60;

/// Single INC or DEC in a QC3.5 auth pulse train.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Qc35AuthPulse {
    /// `SINGLE_INCREMENT_BIT` — raise VBUS one step.
    Inc,
    /// `SINGLE_DECREMENT_BIT` — lower VBUS one step.
    Dec,
}

/// Android `+-+-+-` — request `SRC_CAP` after the detect window (3× INC+DEC).
pub const QC35_SRC_CAP_PULSES: &[Qc35AuthPulse] = &[
    Qc35AuthPulse::Inc,
    Qc35AuthPulse::Dec,
    Qc35AuthPulse::Inc,
    Qc35AuthPulse::Dec,
    Qc35AuthPulse::Inc,
    Qc35AuthPulse::Dec,
];

/// Android `++−−` — confirm transition to QC3.5 after `SRC_CAP` classify.
pub const QC35_CONFIRM_PULSES: &[Qc35AuthPulse] = &[
    Qc35AuthPulse::Inc,
    Qc35AuthPulse::Inc,
    Qc35AuthPulse::Dec,
    Qc35AuthPulse::Dec,
];

/// Gate: whether KMDF should run auth pulses or fall straight to QC3 elevate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Qc35AuthGate {
    /// Vin is below 6.4 V and readable — prep/detect then pulse.
    Attempt,
    /// Vin already ≥ 6.4 V (warm QC3 elevate) — skip auth, keep QC3.
    SkipVinTooHigh,
    /// Vin ≤ 0 / ADC unavailable — skip auth, keep QC3.
    SkipVinInvalid,
}

/// Outcome of the pure auth policy (success class or QC3 fallback).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Qc35AuthResult {
    /// Authenticated; `power_w` is 18, 27, or 40.
    Authenticated {
        /// `SRC_CAP` power class in watts.
        power_w: u32,
    },
    /// Auth skipped or failed — caller **must** keep the QC3 path (no `APSD_RERUN`).
    FallbackQc3,
}

/// Decide whether to attempt QC3.5 auth given Vin after QC3 / continuous detect.
#[must_use]
pub const fn decide_qc35_auth_attempt(vin_uv: i32) -> Qc35AuthGate {
    if vin_uv <= 0 {
        Qc35AuthGate::SkipVinInvalid
    } else if vin_uv >= QC35_DETECT_HI_UV {
        Qc35AuthGate::SkipVinTooHigh
    } else {
        Qc35AuthGate::Attempt
    }
}

/// True when Vin is inside the Android QC3.5 detect window (5.5–6.4 V exclusive).
#[must_use]
pub const fn qc35_detect_window(vin_uv: i32) -> bool {
    vin_uv > QC35_DETECT_LO_UV && vin_uv < QC35_DETECT_HI_UV
}

/// True when Vin is inside the broad `SRC_CAP` accept window (6.65–9.8 V inclusive).
#[must_use]
pub const fn qc35_cap_window(vin_uv: i32) -> bool {
    vin_uv >= QC35_CAP_LO_UV && vin_uv <= QC35_CAP_HI_UV
}

/// Classify QC3.5 `SRC_CAP` power limit (W) from VBUS, or `None` if unsupported.
///
/// Mirrors Android: 6.65–7.35 → 18 W, 7.6–8.4 → 27 W, 8.55–9.8 → 40 W.
#[must_use]
pub const fn qc35_power_limit_w(vin_uv: i32) -> Option<u32> {
    if vin_uv >= QC35_CAP_LO_UV && vin_uv <= QC35_18W_HI_UV {
        Some(18)
    } else if vin_uv >= QC35_27W_LO_UV && vin_uv <= QC35_27W_HI_UV {
        Some(27)
    } else if vin_uv >= QC35_40W_LO_UV && vin_uv <= QC35_CAP_HI_UV {
        Some(40)
    } else {
        None
    }
}

/// USBIN ICL raw code for a QC3.5 power class (2 A for 18/27 W; 3 A for 40 W).
#[must_use]
pub const fn qc35_icl_raw(power_w: u32) -> u8 {
    if power_w >= 40 {
        ICL_RAW_QC35_40W
    } else {
        ICL_RAW_QC35_2A
    }
}

/// Pure authenticate evaluation from Vin samples (no SPMI).
///
/// * `vin_before_auth` — Vin when entering auth (after QC3 continuous).
/// * `detect_vin` — Vin after prep / detect wait (`None` = timeout / miss).
/// * `src_cap_vin` — Vin after `+-+-+-` (`None` = timeout).
///
/// Confirm `++−−` is assumed issued only after a successful classify; failure
/// at any earlier step yields [`Qc35AuthResult::FallbackQc3`].
#[must_use]
pub const fn qc35_authenticate_outcome(
    vin_before_auth: i32,
    detect_vin: Option<i32>,
    src_cap_vin: Option<i32>,
) -> Qc35AuthResult {
    match decide_qc35_auth_attempt(vin_before_auth) {
        Qc35AuthGate::SkipVinInvalid | Qc35AuthGate::SkipVinTooHigh => {
            return Qc35AuthResult::FallbackQc3;
        }
        Qc35AuthGate::Attempt => {}
    }

    let Some(det) = detect_vin else {
        return Qc35AuthResult::FallbackQc3;
    };
    if !qc35_detect_window(det) {
        return Qc35AuthResult::FallbackQc3;
    }

    let Some(cap) = src_cap_vin else {
        return Qc35AuthResult::FallbackQc3;
    };
    if !qc35_cap_window(cap) {
        return Qc35AuthResult::FallbackQc3;
    }

    match qc35_power_limit_w(cap) {
        Some(power_w) => Qc35AuthResult::Authenticated { power_w },
        None => Qc35AuthResult::FallbackQc3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_attempt_when_vin_in_or_below_detect() {
        assert_eq!(decide_qc35_auth_attempt(5_000_000), Qc35AuthGate::Attempt);
        assert_eq!(decide_qc35_auth_attempt(6_000_000), Qc35AuthGate::Attempt);
        assert_eq!(
            qc35_authenticate_outcome(5_200_000, Some(6_000_000), Some(7_000_000)),
            Qc35AuthResult::Authenticated { power_w: 18 }
        );
        assert_eq!(
            qc35_authenticate_outcome(6_000_000, Some(6_000_000), Some(8_000_000)),
            Qc35AuthResult::Authenticated { power_w: 27 }
        );
        assert_eq!(
            qc35_authenticate_outcome(5_800_000, Some(5_900_000), Some(9_000_000)),
            Qc35AuthResult::Authenticated { power_w: 40 }
        );
    }

    #[test]
    fn skip_auth_when_vin_already_high() {
        assert_eq!(
            decide_qc35_auth_attempt(6_400_000),
            Qc35AuthGate::SkipVinTooHigh
        );
        assert_eq!(
            decide_qc35_auth_attempt(9_600_000),
            Qc35AuthGate::SkipVinTooHigh
        );
        // Warm QC3 elevate: never attempt pulses; fall back to QC3 elevate.
        assert_eq!(
            qc35_authenticate_outcome(9_600_000, Some(9_600_000), Some(9_600_000)),
            Qc35AuthResult::FallbackQc3
        );
    }

    #[test]
    fn fail_fallback_to_qc3() {
        assert_eq!(decide_qc35_auth_attempt(0), Qc35AuthGate::SkipVinInvalid);
        assert_eq!(
            qc35_authenticate_outcome(5_000_000, None, Some(7_000_000)),
            Qc35AuthResult::FallbackQc3
        );
        assert_eq!(
            qc35_authenticate_outcome(5_000_000, Some(6_000_000), None),
            Qc35AuthResult::FallbackQc3
        );
        // Detect miss (still ~5 V after prep budget).
        assert_eq!(
            qc35_authenticate_outcome(5_000_000, Some(5_200_000), Some(7_000_000)),
            Qc35AuthResult::FallbackQc3
        );
        // Cap Vin in gap between 18 W and 27 W bands.
        assert_eq!(
            qc35_authenticate_outcome(6_000_000, Some(6_000_000), Some(7_500_000)),
            Qc35AuthResult::FallbackQc3
        );
    }

    #[test]
    fn power_class_mapping_matches_android_windows() {
        assert_eq!(qc35_power_limit_w(6_650_000), Some(18));
        assert_eq!(qc35_power_limit_w(7_000_000), Some(18));
        assert_eq!(qc35_power_limit_w(7_350_000), Some(18));
        assert_eq!(qc35_power_limit_w(7_600_000), Some(27));
        assert_eq!(qc35_power_limit_w(8_000_000), Some(27));
        assert_eq!(qc35_power_limit_w(8_400_000), Some(27));
        assert_eq!(qc35_power_limit_w(8_550_000), Some(40));
        assert_eq!(qc35_power_limit_w(9_000_000), Some(40));
        assert_eq!(qc35_power_limit_w(9_800_000), Some(40));
        assert_eq!(qc35_power_limit_w(6_000_000), None);
        assert_eq!(qc35_power_limit_w(7_500_000), None);
        assert_eq!(qc35_power_limit_w(8_500_000), None);
        assert_eq!(qc35_icl_raw(18), ICL_RAW_QC35_2A);
        assert_eq!(qc35_icl_raw(27), ICL_RAW_QC35_2A);
        assert_eq!(qc35_icl_raw(40), ICL_RAW_QC35_40W);
    }

    #[test]
    fn pulse_sequences_are_plus_minus_then_plus_plus_minus_minus() {
        assert_eq!(QC35_SRC_CAP_PULSES.len(), 6);
        assert_eq!(
            QC35_SRC_CAP_PULSES,
            &[
                Qc35AuthPulse::Inc,
                Qc35AuthPulse::Dec,
                Qc35AuthPulse::Inc,
                Qc35AuthPulse::Dec,
                Qc35AuthPulse::Inc,
                Qc35AuthPulse::Dec,
            ]
        );
        assert_eq!(QC35_CONFIRM_PULSES.len(), 4);
        assert_eq!(
            QC35_CONFIRM_PULSES,
            &[
                Qc35AuthPulse::Inc,
                Qc35AuthPulse::Inc,
                Qc35AuthPulse::Dec,
                Qc35AuthPulse::Dec,
            ]
        );
        assert!(qc35_detect_window(6_000_000));
        assert!(!qc35_detect_window(5_000_000));
        assert!(!qc35_detect_window(6_400_000));
        assert!(qc35_cap_window(7_000_000));
        assert!(!qc35_cap_window(6_000_000));
    }
}
