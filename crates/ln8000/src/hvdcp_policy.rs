//! Pure HVDCP cold-plug / SUPERUSER-retry policy (no WDF, host-testable).
//!
//! KMDF calls these from the telemetry timer and `PrepareHardware` autostart so
//! a full SUPERUSER slot at boot does not permanently skip QC, and a later
//! cable edge after 5 V bypass can re-elevate without a driver reload.
//!
//! Also holds the pure APSD → elevate-path classification
//! ([`apsd_elevate_path`]): `0x28` is QC2 (`FORCE_9V`), not an AFC/5 V block.

/// Max SUPERUSER open retries after `PrepareHardware` slot-full.
pub const HVDCP_SUPERUSER_RETRY_MAX: u32 = 30;
/// Delay between SUPERUSER open retries (ms).
pub const HVDCP_SUPERUSER_RETRY_MS: u64 = 2_000;
/// Vin at/under this counts as cable absent for re-plug edge detect (µV).
pub const VIN_UNPLUG_MAX_UV: i32 = 1_000_000;
/// IOCTL / negotiate `error_code` when neither SUPERUSER nor Usbin RH opened.
pub const HVDCP_ERR_USBIN_UNAVAILABLE: i32 = -10;

/// `HvdcpPhase::Idle`.
pub const HVDCP_PHASE_IDLE: u32 = 0;
/// `HvdcpPhase::Done`.
pub const HVDCP_PHASE_DONE: u32 = 6;
/// `HvdcpPhase::Failed`.
pub const HVDCP_PHASE_FAILED: u32 = 7;
/// `HvdcpPhase::FiveVBypass`.
pub const HVDCP_PHASE_FIVE_V_BYPASS: u32 = 9;

/// Schedule a later SUPERUSER open when autostart got usbin-unavailable.
#[must_use]
pub const fn should_schedule_superuser_retry(negotiate_rc: i32) -> bool {
    negotiate_rc == HVDCP_ERR_USBIN_UNAVAILABLE
}

/// Whether a pending SUPERUSER retry should run now.
#[must_use]
pub const fn superuser_retry_due(pending: bool, attempts: u32, now_ms: u64, next_ms: u64) -> bool {
    pending && attempts < HVDCP_SUPERUSER_RETRY_MAX && now_ms >= next_ms
}

/// Rising cable edge after 5 V bypass / failed open / prior Done → re-negotiate.
///
/// `phase` uses the KMDF [`HvdcpPhase`] numeric codes.
#[must_use]
pub const fn should_renegotiate_on_input_edge(
    phase: u32,
    was_input_present: bool,
    now_input_present: bool,
) -> bool {
    if was_input_present || !now_input_present {
        return false;
    }
    matches!(
        phase,
        HVDCP_PHASE_IDLE | HVDCP_PHASE_FAILED | HVDCP_PHASE_DONE | HVDCP_PHASE_FIVE_V_BYPASS
    )
}

/// Classify Vin as cable present for cold-plug / re-plug detection.
#[must_use]
pub const fn input_present_from_vin(vin_uv: i32) -> bool {
    vin_uv > VIN_UNPLUG_MAX_UV
}

/// VBUS at/above this during the `FORCE_9V` wait means the brick answered the
/// QC signature and is ramping (µV).
///
/// Live MDY-11-EP (19.09): one 6.48 V sample inside a 1.2 s window, then straight
/// back to 5 V when the retreat forced it — the answer is real but slower than
/// any fixed window that a 5 V-only source would tolerate.
pub const FORCE9V_RISE_UV: i32 = 5_600_000;
/// First wait window for a `FORCE_9V` answer (ms).
pub const FORCE9V_SETTLE_MS: u32 = 1_600;
/// Extra time granted once a rise past [`FORCE9V_RISE_UV`] is seen (ms).
pub const FORCE9V_EXTEND_MS: u32 = 2_400;
/// Hard cap for the whole wait, extension included (ms). Bounds how long the
/// SUPERUSER bus can be held on a source that ignores the signature.
pub const FORCE9V_HARD_CAP_MS: u32 = 5_000;

/// What the `FORCE_9V` wait loop should do with the newest sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Force9vWait {
    /// Keep polling inside the current deadline.
    Continue,
    /// The brick answered: move the deadline out (allowed once).
    Extend,
    /// Stop polling — the bus reached the 2:1 gate, or the deadline is up.
    Stop,
}

/// One step of the `FORCE_9V` wait loop.
///
/// `switching_min_uv` is the caller's 2:1 gate ([`SWITCHING_MIN_VIN_UV`] in the
/// KMDF layer) — passed in so this module stays free of pump encoding.
#[must_use]
pub const fn force9v_step(
    vin_uv: i32,
    waited_ms: u32,
    deadline_ms: u32,
    extended: bool,
    switching_min_uv: i32,
) -> Force9vWait {
    if vin_uv >= switching_min_uv || waited_ms >= deadline_ms {
        return Force9vWait::Stop;
    }
    if !extended && vin_uv >= FORCE9V_RISE_UV {
        return Force9vWait::Extend;
    }
    Force9vWait::Continue
}

/// Deadline after the first rise is seen: one extension, capped.
#[must_use]
pub const fn force9v_extended_deadline(deadline_ms: u32) -> u32 {
    if deadline_ms.saturating_add(FORCE9V_EXTEND_MS) > FORCE9V_HARD_CAP_MS {
        FORCE9V_HARD_CAP_MS
    } else {
        deadline_ms + FORCE9V_EXTEND_MS
    }
}

/// `DCP_CHARGER_BIT` in `APSD_RESULT_STATUS` (`smb5-reg.h`).
pub const APSD_BIT_DCP: u8 = 1 << 3;
/// `QC_2P0_BIT` in `APSD_RESULT_STATUS`.
pub const APSD_BIT_QC2: u8 = 1 << 5;
/// `QC_3P0_BIT` in `APSD_RESULT_STATUS`.
pub const APSD_BIT_QC3: u8 = 1 << 6;
/// `QC_CHARGER_BIT` in `APSD_STATUS` (not in the result byte).
pub const APSD_STAT_BIT_QC_CHARGER: u8 = 1 << 1;

/// Vendor promotion applied to the APSD result before it is classified
/// (`smb5-lib.c:610-630`, `smblib_get_apsd_result`).
///
/// Android does not trust a plain DCP result on its own: if `APSD_STATUS` has
/// `QC_CHARGER_BIT` set, anything that is not already HVDCP3 is re-read as
/// HVDCP2 (`0x28`). Without this step a brick that reports `0x08` while its
/// D+/D- carry the QC signature is classified a plain DCP and — on a build that
/// does not elevate DCP — never gets its 9 V. The promotion only widens the
/// result; it never downgrades QC3.
#[must_use]
pub const fn promote_qc_charger(result: u8, apsd_status: u8) -> u8 {
    if (apsd_status & APSD_STAT_BIT_QC_CHARGER) != 0 && (result & APSD_BIT_QC3) == 0 {
        result | APSD_BIT_DCP | APSD_BIT_QC2
    } else {
        result
    }
}

/// Elevate path Android takes for one APSD result byte (`smb5-lib.c`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApsdElevate {
    /// QC3 / QC3.5 (`QC_3P0_BIT` or a latched continuous detect) — INC pulses.
    Qc3Pulse,
    /// QC2 or plain DCP — `FORCE_9V` (`smb5-lib.c`, branch QC2).
    Force9v,
    /// SDP / CDP / unknown — never elevate (protects the PMIC).
    Reject,
}

/// Classifies `APSD_RESULT_STATUS` (plus `QC_CHANGE_STATUS` continuous bit) into
/// the Android elevate path.
///
/// `0x28` (`DCP|QC_2P0`) is **HVDCP2 / QC2** in the reference APSD table:
/// `smb5-lib.c` runs `FORCE_9V` and votes a 1.5 A ICL for it. It is not an
/// "AFC-like 5 V" block — nabu has no AFC protocol at all. A brick that stays
/// near 5 V after `FORCE_9V` is handled by the caller as a retreat to the 5 V
/// high-current bypass, never as an APSD classification.
#[must_use]
pub const fn apsd_elevate_path(result: u8, qc3_continuous: bool) -> ApsdElevate {
    if (result & APSD_BIT_QC3) != 0 || qc3_continuous {
        ApsdElevate::Qc3Pulse
    } else if (result & APSD_BIT_QC2) != 0 || (result & APSD_BIT_DCP) != 0 {
        ApsdElevate::Force9v
    } else {
        ApsdElevate::Reject
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn superuser_unavailable_schedules_retry() {
        assert!(should_schedule_superuser_retry(HVDCP_ERR_USBIN_UNAVAILABLE));
        assert!(!should_schedule_superuser_retry(0));
        assert!(!should_schedule_superuser_retry(-13));
        assert!(!should_schedule_superuser_retry(-14));
    }

    #[test]
    fn superuser_retry_due_respects_budget_and_deadline() {
        assert!(superuser_retry_due(true, 0, 2_000, 2_000));
        assert!(!superuser_retry_due(false, 0, 2_000, 2_000));
        assert!(!superuser_retry_due(true, 0, 1_999, 2_000));
        assert!(!superuser_retry_due(
            true,
            HVDCP_SUPERUSER_RETRY_MAX,
            9_999,
            0
        ));
        assert!(superuser_retry_due(
            true,
            HVDCP_SUPERUSER_RETRY_MAX - 1,
            9_999,
            0
        ));
    }

    #[test]
    fn input_edge_renegotiates_after_5v_or_failure() {
        assert!(should_renegotiate_on_input_edge(
            HVDCP_PHASE_FIVE_V_BYPASS,
            false,
            true
        ));
        assert!(should_renegotiate_on_input_edge(
            HVDCP_PHASE_FAILED,
            false,
            true
        ));
        assert!(should_renegotiate_on_input_edge(
            HVDCP_PHASE_DONE,
            false,
            true
        ));
        assert!(should_renegotiate_on_input_edge(
            HVDCP_PHASE_IDLE,
            false,
            true
        ));
        assert!(!should_renegotiate_on_input_edge(
            HVDCP_PHASE_FIVE_V_BYPASS,
            true,
            true
        ));
        assert!(!should_renegotiate_on_input_edge(
            HVDCP_PHASE_FIVE_V_BYPASS,
            false,
            false
        ));
        // Mid-machine QC3 pulse must not fire from telemetry.
        assert!(!should_renegotiate_on_input_edge(5, false, true));
    }

    #[test]
    fn input_present_threshold_is_below_5v() {
        assert!(!input_present_from_vin(0));
        assert!(!input_present_from_vin(VIN_UNPLUG_MAX_UV));
        assert!(input_present_from_vin(VIN_UNPLUG_MAX_UV + 1));
        assert!(input_present_from_vin(5_000_000));
    }

    #[test]
    fn phase_codes_match_kmdf_hvdcp_phase() {
        assert_eq!(HVDCP_PHASE_IDLE, 0);
        assert_eq!(HVDCP_PHASE_DONE, 6);
        assert_eq!(HVDCP_PHASE_FAILED, 7);
        assert_eq!(HVDCP_PHASE_FIVE_V_BYPASS, 9);
        assert_eq!(HVDCP_ERR_USBIN_UNAVAILABLE, -10);
    }

    #[test]
    fn apsd_0x28_is_qc2_force9v_not_a_5v_bypass() {
        // TA200/TA220 and QC2 bricks report 0x28 = DCP|QC_2P0 = HVDCP2.
        // Reference (`smb5-lib.c`, APSD table + QC2 branch): FORCE_9V, 1.5 A.
        assert_eq!(apsd_elevate_path(0x28, false), ApsdElevate::Force9v);
        assert_eq!(
            apsd_elevate_path(APSD_BIT_QC2 | APSD_BIT_DCP, false),
            ApsdElevate::Force9v
        );
        // Plain DCP and the bare QC2 bit also take FORCE_9V (QC2 auth path).
        assert_eq!(apsd_elevate_path(APSD_BIT_DCP, false), ApsdElevate::Force9v);
        assert_eq!(apsd_elevate_path(APSD_BIT_QC2, false), ApsdElevate::Force9v);
        // QC3 / QC3.5 continuous must keep the pulse path.
        assert_eq!(
            apsd_elevate_path(APSD_BIT_QC3 | APSD_BIT_DCP, false),
            ApsdElevate::Qc3Pulse
        );
        assert_eq!(apsd_elevate_path(APSD_BIT_DCP, true), ApsdElevate::Qc3Pulse);
        // SDP / CDP / unknown never elevate.
        assert_eq!(apsd_elevate_path(0, false), ApsdElevate::Reject);
        assert_eq!(apsd_elevate_path(1 << 1, false), ApsdElevate::Reject);
    }

    #[test]
    fn qc_charger_bit_promotes_unknown_result_to_hvdcp2() {
        // `smb5-lib.c`: `if (apsd_stat & QC_CHARGER_BIT) result = HVDCP2`.
        assert_eq!(promote_qc_charger(0, APSD_STAT_BIT_QC_CHARGER), 0x28);
        assert_eq!(
            promote_qc_charger(APSD_BIT_DCP, APSD_STAT_BIT_QC_CHARGER),
            0x28
        );
        assert_eq!(
            promote_qc_charger(APSD_BIT_QC2, APSD_STAT_BIT_QC_CHARGER),
            0x28
        );
        // HVDCP3 is never downgraded by the promotion.
        assert_eq!(
            promote_qc_charger(APSD_BIT_QC3 | APSD_BIT_DCP, APSD_STAT_BIT_QC_CHARGER),
            APSD_BIT_QC3 | APSD_BIT_DCP
        );
        // No QC bit in the status → result passes through untouched.
        assert_eq!(promote_qc_charger(APSD_BIT_DCP, 0), APSD_BIT_DCP);
        assert_eq!(promote_qc_charger(0, 0), 0);
        // The promoted result then takes the QC2 elevate path.
        assert_eq!(
            apsd_elevate_path(promote_qc_charger(0, APSD_STAT_BIT_QC_CHARGER), false),
            ApsdElevate::Force9v
        );
    }

    #[test]
    fn force9v_wait_stops_the_moment_the_gate_is_met() {
        // The 2:1 gate is reached on the sample itself — no extra poll.
        assert_eq!(
            force9v_step(8_000_000, 0, FORCE9V_SETTLE_MS, false, 8_000_000),
            Force9vWait::Stop
        );
        // A brick still at 5 V keeps the loop polling, then the deadline ends it.
        assert_eq!(
            force9v_step(4_800_000, 0, FORCE9V_SETTLE_MS, false, 8_000_000),
            Force9vWait::Continue
        );
        assert_eq!(
            force9v_step(4_800_000, FORCE9V_SETTLE_MS, FORCE9V_SETTLE_MS, false, 8_000_000),
            Force9vWait::Stop
        );
    }

    #[test]
    fn force9v_wait_extends_once_on_a_partial_rise() {
        // Live MDY-11-EP: 6.48 V seen, ramp slower than the first window.
        assert_eq!(
            force9v_step(6_480_000, 400, FORCE9V_SETTLE_MS, false, 8_000_000),
            Force9vWait::Extend
        );
        let moved = force9v_extended_deadline(FORCE9V_SETTLE_MS);
        assert_eq!(moved, FORCE9V_SETTLE_MS + FORCE9V_EXTEND_MS);
        // The extension is granted once: a second partial rise does not push it.
        assert_eq!(
            force9v_step(6_480_000, moved - 1, moved, true, 8_000_000),
            Force9vWait::Continue
        );
        assert_eq!(
            force9v_step(6_480_000, moved, moved, true, 8_000_000),
            Force9vWait::Stop
        );
        // A 5 V source never triggers the extension at all.
        assert_eq!(
            force9v_step(4_900_000, 100, FORCE9V_SETTLE_MS, false, 8_000_000),
            Force9vWait::Continue
        );
    }

    #[test]
    fn force9v_deadline_never_passes_the_hard_cap() {
        assert_eq!(
            force9v_extended_deadline(FORCE9V_HARD_CAP_MS),
            FORCE9V_HARD_CAP_MS
        );
        assert!(force9v_extended_deadline(FORCE9V_SETTLE_MS) <= FORCE9V_HARD_CAP_MS);
        assert_eq!(force9v_extended_deadline(u32::MAX), FORCE9V_HARD_CAP_MS);
        // A deadline that is already past the cap stops without extending.
        assert_eq!(
            force9v_step(6_000_000, FORCE9V_HARD_CAP_MS, FORCE9V_HARD_CAP_MS, false, 8_200_000),
            Force9vWait::Stop
        );
    }
}
