//! HVDCP / Quick Charge state machine over USBIN SPMI (PM8150B SID 2).
//!
//! Prefers `\Device\Spmi\SUPERUSER` (peri-grant `0x13`). Usbin Resource Hub
//! remains a secondary path when an ACPI overlay exposes a connection id.
//! Stock ACPI (`UsbinConn=0`) still negotiates via SUPERUSER when a slot is free.
//!
//! Register sequence mirrors Android `smb5-lib.c` / `CMD_HVDCP_2`:
//! clear latched IRQs → clear `BC1P2_START_ON_CC` → soft ICL floor 500 mA →
//! unlock QC2 voltage gates (`0x1360` / `0x135B`) → enable HVDCP at `0x1362` →
//! settle → APSD rerun `0x1341` (skipped when QC/DCP already latched) →
//! read `0x1307`/`0x1308` → QC3.5 auth (`qc3p5_authenticate` pulse pattern) or
//! QC3 INC / QC2 FORCE via `0x1343` → sample `QC_CHANGE_STATUS` (`0x1309`).
//!
//! **QC3.5:** after QC3 / continuous detect, attempt +-+-+- then ++−− with 5 ms
//! gaps and VBUS windows; on success mark `SuQc35Auth=1`. Auth failure falls
//! through to QC3 elevate — never demotes with `APSD_RERUN`.
//!
//! **5 V / TA220:** when FORCE/pulses leave Vin ~5 V, post-path raises ICL to
//! ≈2.7 A and enters LN8000 bypass. This is a **retreat**, not a classification:
//! APSD `0x28` (`DCP|QC_2P0`) is ordinary QC2 in the reference table and takes
//! `FORCE_9V` + `HVDCP2_CURRENT_UA` (1.5 A). Samsung AFC protocol is **not**
//! supported on nabu (absent from Android DT) — max power is 5 V high-current
//! bypass when the brick refuses to elevate.
//!
//! Android also calls `smblib_request_dpdm()` (`dpdm-supply = &usb2_phy0` on nabu).
//! That is a USB2 PHY regulator, not a USBIN SPMI bit — Windows must release
//! D+/D− via USBFn/URS (see `14-hvdcp/97-hvdcp-force-once.ps1 -ReleaseDpdm`).

use crate::spb::{SpbBus, SuperuserBus, SPMI_PERI_USBIN, SPMI_SID_USBIN};
use wdk_sys::WDFDEVICE;

/// `APSD_STATUS` (USBIN_BASE + 0x07).
pub const REG_APSD_STATUS: u16 = 0x1307;
/// `APSD_RESULT_STATUS` (USBIN_BASE + 0x08).
pub const REG_APSD_RESULT: u16 = 0x1308;
/// `QC_CHANGE_STATUS` (USBIN_BASE + 0x09) — HW voltage / continuous bits.
pub const REG_QC_CHANGE_STATUS: u16 = 0x1309;
/// `QC_PULSE_COUNT_STATUS` (USBIN_BASE + 0x0A) — HW pulse counter.
///
/// **The register does not exist in the smb5 header** (`smb5-reg.h` does not declare
/// it; the address and the `QC_PULSE_COUNT_MASK` mask are only in `smb-reg.h:472-475`,
/// and the only driver that reads it is `drivers_power_supply_qcom_smb-lib.c:805-830`,
/// under `PMI8998_SUBTYPE`/`PM660_SUBTYPE`). It is read **for diagnostics only**
/// (`SuQcPulseHw`): no decision is made from it, the pulse count is kept by our own
/// `state.pulse_cnt`. Do not treat the value in the report as the PM8150B pulse count.
pub const REG_QC_PULSE_COUNT: u16 = 0x130A;
/// Qualcomm peri `INT_LATCHED_CLR` (USBIN_BASE + 0x14).
pub const REG_INT_LATCHED_CLR: u16 = 0x1314;
/// `USBIN_CMD_IL` (USBIN_BASE + 0x40).
pub const REG_USBIN_CMD_IL: u16 = 0x1340;
/// `CMD_APSD` (USBIN_BASE + 0x41).
pub const REG_CMD_APSD: u16 = 0x1341;
/// `CMD_ICL_OVERRIDE` (USBIN_BASE + 0x42).
pub const REG_CMD_ICL_OVERRIDE: u16 = 0x1342;
/// `CMD_HVDCP_2` (USBIN_BASE + 0x43).
pub const REG_CMD_HVDCP_2: u16 = 0x1343;
/// `TYPE_C_CFG` (USBIN_BASE + 0x58) — hosts `BC1P2_START_ON_CC`.
pub const REG_TYPE_C_CFG: u16 = 0x1358;
/// `HVDCP_PULSE_COUNT_MAX` (USBIN_BASE + 0x5B) — QC2 max voltage in bits 7:6.
pub const REG_HVDCP_PULSE_COUNT_MAX: u16 = 0x135B;
/// `USBIN_ADAPTER_ALLOW_CFG` (USBIN_BASE + 0x60).
pub const REG_USBIN_ADAPTER_ALLOW: u16 = 0x1360;
/// `USBIN_OPTIONS_1_CFG` (USBIN_BASE + 0x62).
pub const REG_USBIN_OPTIONS_1: u16 = 0x1362;
/// `USBIN_CURRENT_LIMIT_CFG` (USBIN_BASE + 0x70), step 50 mA.
pub const REG_USBIN_ICL_CFG: u16 = 0x1370;

/// `HVDCP_EN_BIT`.
pub const BIT_HVDCP_EN: u8 = 1 << 2;
/// `BC1P2_SRC_DETECT_BIT`.
pub const BIT_BC1P2_SRC_DETECT: u8 = 1 << 3;
/// `HVDCP_AUTONOMOUS_MODE_EN_CFG_BIT` — clear for host-driven pulses.
pub const BIT_HVDCP_AUTONOMOUS: u8 = 1 << 5;
/// `HVDCP_AUTH_ALG_EN_CFG_BIT`.
pub const BIT_HVDCP_AUTH_ALG_EN: u8 = 1 << 6;

/// `BC1P2_START_ON_CC_BIT` / `APSD_START_ON_CC_BIT` in `TYPE_C_CFG`.
pub const BIT_BC1P2_START_ON_CC: u8 = 1 << 7;
/// `USBIN_SUSPEND_BIT` in `USBIN_CMD_IL`.
pub const BIT_USBIN_SUSPEND: u8 = 1 << 0;
/// `ICL_OVERRIDE_BIT`.
pub const BIT_ICL_OVERRIDE: u8 = 1 << 0;

/// `APSD_RERUN_BIT`.
pub const BIT_APSD_RERUN: u8 = 1 << 0;
/// `APSD_DTC_STATUS_DONE_BIT`.
pub const BIT_APSD_DONE: u8 = 1 << 0;
/// `QC_CHARGER_BIT` in APSD_STATUS.
pub const BIT_QC_CHARGER: u8 = 1 << 1;

/// `DCP_CHARGER_BIT` in APSD_RESULT.
pub const BIT_DCP: u8 = 1 << 3;
/// `QC_2P0_BIT`.
pub const BIT_QC2: u8 = 1 << 5;
/// `QC_3P0_BIT`.
pub const BIT_QC3: u8 = 1 << 6;

/// `SINGLE_INCREMENT_BIT`.
pub const BIT_SINGLE_INC: u8 = 1 << 0;
/// `SINGLE_DECREMENT_BIT` — lower VBUS one QC3 step.
pub const BIT_SINGLE_DEC: u8 = 1 << 1;
/// `FORCE_5V_BIT` (`smb5-reg.h` BIT(3)) — safe abort / reset to 5 V.
pub const BIT_FORCE_5V: u8 = 1 << 3;
/// `FORCE_9V_BIT`.
pub const BIT_FORCE_9V: u8 = 1 << 4;

/// `HVDCP_PULSE_COUNT_MAX_QC2_MASK` (bits 7:6).
pub const MASK_HVDCP_PULSE_COUNT_MAX_QC2: u8 = 0xC0;
/// `HVDCP_PULSE_COUNT_MAX_QC2_9V` encoding in bits 7:6.
pub const HVDCP_PULSE_COUNT_MAX_QC2_9V: u8 = 0x40;
/// `USBIN_ADAPTER_ALLOW_MASK`.
pub const MASK_USBIN_ADAPTER_ALLOW: u8 = 0x0F;
/// `USBIN_ADAPTER_ALLOW_5V_TO_12V` — Android nabu `qpnp-smb5` shutdown/probe value.
pub const USBIN_ADAPTER_ALLOW_5V_TO_12V: u8 = 0x0C;
/// `QC_9V_BIT` in `QC_CHANGE_STATUS`.
pub const BIT_QC_9V: u8 = 1 << 1;
/// `QC_CONTINUOUS_BIT` in `QC_CHANGE_STATUS`.
///
/// **A name from the previous generation.** The bit is declared only in
/// `smb-reg.h:467` (`QC_CONTINUOUS_BIT`) and `smb-reg.h:466`
/// (`QC_5V_TO_9V_REASON_BIT`); in `smb5-reg.h` the `QC_CHANGE_STATUS` block
/// (`:229-233`) describes only `QC_12V BIT(2)`, `QC_9V BIT(1)`, `QC_5V BIT(0)` and
/// `QC_2P0_STATUS_MASK`, and the `0x130A` register is not there at all. The PM8150B
/// stamp follows `smb5-reg.h`, so bit 3 is undocumented for us. It stays a routing
/// input deliberately: the pulse path is safer for a doubtful result than the DCP
/// branch (pulses raise a real QC brick, while a DCP ignores them and stays at 5 V -
/// the live `SuQcPre`/`SuQcChgSt` measurement decides, not a guess). It must not be
/// read as "QC3 confirmed": only as "possibly QC".
pub const BIT_QC_CONTINUOUS: u8 = 1 << 3;
/// `QC_5V_TO_9V_REASON_BIT`. See [`BIT_QC_CONTINUOUS`] - same source of the names.
pub const BIT_QC_5V_TO_9V_REASON: u8 = 1 << 4;

/// Raw `USBIN_CURRENT_LIMIT_CFG` for 500 mA (step 50 mA → code 10).
pub const ICL_RAW_500MA: u8 = 10;
/// Raw ICL for LN8000 2:1 after Vin elevate (3 A, step 50 mA → code 60).
///
/// APSD keeps a 500 mA soft floor; once QC Vin is up the pump needs a real
/// USBIN budget or Iin stalls around ~0.65 A even with switching enabled.
pub const ICL_RAW_PUMP_3A: u8 = 60;
/// Nabu DCP high-current floor (1.8 A, step 50 mA → code 36).
///
/// Android nabu DCP / SDP high-current votes land in the 1.8–2.7 A band; the
/// 5 V bypass path must never sit below this floor once elevate fails.
pub const ICL_RAW_DCP_1P8A: u8 = 36;
/// Raw ICL for 5 V high-current bypass (NABU class B ≈ 2.7 A → code 54).
///
/// Used when APSD is plain DCP / QC2 FORCE stays at ~5 V (TA220 AFC brick
/// path: AFC protocol is **not** implemented on nabu Windows — max power is
/// bypass at this ICL). Paired with LN8000 `set_iin_limit(2_700_000)`.
pub const ICL_RAW_5V_2P7A: u8 = 54;
/// Vendor's DCP input-current vote: 2 A, 50 mA step → raw 40.
///
/// `DCP_CURRENT_UA = 2_000_000` (`smb5-lib.h:248`), written to
/// `USBIN_CURRENT_LIMIT_CFG`. Android never raises VBUS for a plain DCP — it
/// only votes this current — and the live `0x1370=0x0A` (500 mA, the SDP
/// number) is what makes a 5 V brick charge slowly on Windows.
pub const ICL_RAW_DCP_2A: u8 = 40;
/// Vendor's HVDCP2 (QC 2.0) input-current vote: 1.5 A, 50 mA step → raw 30.
///
/// `HVDCP2_CURRENT_UA = 1_500_000` (`smb5-lib.h:227`), voted next to the QC2
/// `FORCE_9V` write in the kernel's own APSD handler
/// (`smb5-lib.c:8373-8381`: `smblib_force_vbus_voltage(chg, FORCE_9V_BIT)` then
/// `vote(chg->usb_icl_votable, SW_ICL_MAX_VOTER, true, HVDCP2_CURRENT_UA)`).
///
/// The "we use our own qc2 method" early return at `smb5-lib.c:4143-4146` is the
/// **userspace** `DP_DM` entry point only — the kernel handler for nabu runs in
/// the `CONFIG_MACH_XIAOMI_VAYU || CONFIG_MACH_XIAOMI_NABU` arm, which does write
/// `FORCE_9V`. Without this vote the write happens with no input-current budget,
/// which is why a QC2 brick could stay at ~5 V with a 500 mA-class limit.
pub const ICL_RAW_HVDCP2_1P5A: u8 = 30;

const _: () = assert!(ICL_RAW_5V_2P7A >= ICL_RAW_DCP_1P8A);
const _: () = assert!(ICL_RAW_5V_2P7A <= ICL_RAW_PUMP_3A);
// Cold-plug policy in `ln8000::hvdcp_policy` must stay aligned with these codes.
const _: () = assert!(HvdcpError::UsbinUnavailable.code() == -10);
const _: () = assert!(HvdcpPhase::Idle as u32 == 0);
const _: () = assert!(HvdcpPhase::Done as u32 == 6);
const _: () = assert!(HvdcpPhase::Failed as u32 == 7);
const _: () = assert!(HvdcpPhase::FiveVBypass as u32 == 9);
/// Settle after FORCE_9V / last pulse before reading QC_CHANGE (ms).
pub const QC_STATUS_SETTLE_MS: u32 = 200;

/// Poll period inside the `FORCE_9V` wait (ms).
///
/// The timing policy (first window, one extension on a rising brick, hard cap)
/// is host-tested in `ln8000::hvdcp_policy`; only the poll cadence lives here.
pub const FORCE9V_POLL_MS: u32 = 50;

/// How many consecutive samples above the 2:1 gate count as a settled 9 V.
pub const FORCE9V_CONFIRM_N: u32 = 2;

/// QC3.0 step size (µV), `HVDCP3_STEP_UV`.
pub const QC3_STEP_UV: u32 = 200_000;
/// Baseline after APSD before pulses.
pub const MICRO_5V_UV: u32 = 5_000_000;
/// QC3 pulse cap for the CP path: Android `cp_qc30.h:81`
/// `MAX_PLUSE_COUNT_ALLOWED` = 23 (23 × 200 mV + 5 V = 9.6 V — the top of the
/// 9.5–10 V window the CP policy holds). The 30 in `smb5-lib.h` belongs to the
/// non-CP smb5 path and overshoots past the 2:1 transfer band.
/// Only INC is capped by this: `pulse_dec` walks back down uncapped.
pub const MAX_PULSE_CNT: u32 = 23;
/// Absolute safety ceiling for the bus (µV) — above ~10.5 V live silicon
/// latches `VIN_OV` and 2:1 stops being engageable at all.
///
/// Policy no longer aims anywhere near it: the operative bounds are the
/// `2*Vbat`-derived band in [`ln8000::encoding`]. This stays as the last-resort
/// guard for a path that would otherwise pulse past it.
pub const PUMP_VIN_TRIM_UV: i32 = 10_500_000;
/// Bus target when the pack voltage is unknown (µV).
///
/// Android `cp_qc30.c:848` raises the bus while `vbus <= 9500` — which is also
/// where the vendor's own CP policy stops. Only used as a fallback when no Vbat
/// reading exists; with a live pack the target is the band centre, because a
/// fixed 9.5 V sits *above* the band once the pack passes ~4.42 V (live 18.09:
/// bus held at 9.888 V against a 9.04–9.24 V band → mode 3 at the 39 mA floor).
pub const PUMP_VIN_TARGET_MIN_UV: i32 = 9_500_000;
/// Absolute floor for a derived target (µV) — mirrors
/// [`ln8000::encoding::SWITCHING_MIN_VIN_UV`], the gate below which 2:1 is never
/// requested. At a low pack this pins the target at 8.0 V, which for Vbat ≥
/// 3.2 V still lands inside or just above the band.
pub const PUMP_VIN_TARGET_ABS_MIN_UV: u32 = 8_000_000;
/// Ceiling for a derived target (µV) — the highest bus reachable within
/// [`MAX_PULSE_CNT`] INC pulses from the 5 V baseline ([`estimated_vbus_uv`]).
/// Every practical band top is below it: `2*4.45 + 0.4 = 9.3 V`.
pub const PUMP_VIN_TARGET_CEIL_UV: u32 = 9_600_000;
/// Max DEC pulses when trimming for the charge pump.
pub const MAX_TRIM_DEC: u32 = 20;
/// Max INC pulses when boosting a too-low elevated bus toward the pump floor.
/// Must never exceed [`MAX_PULSE_CNT`]: the soft counter saturates there, so a
/// larger budget would issue a pulse the counter can no longer record.
pub const MAX_BOOST_INC: u32 = 23;
/// Period floor between bus corrections from the telemetry tick (ms).
///
/// The 2:1 band rides up with the pack: at ~3.6 A the pack climbs ~1 % SOC per
/// 40 s, i.e. the band top moves ~60 mV in that time. Correcting every 10 s
/// keeps the bus inside without touching the SPMI bus on every 250 ms tick —
/// the same contention that made [`APSD_POLL_MS`] 200 ms.
pub const WINDOW_NUDGE_MS: u64 = 10_000;
/// IIN at or below which a mode-3 pump is judged to be carrying no power (µA).
///
/// The floor reading is 39 mA (8 × 4.89 mA, the ADC's zero), so this sits just
/// above it. A wider threshold is a trap: near end-of-charge the pack accepts
/// only 0.1–0.5 A, and a 150 mA gate read that as "dead" — live 18.09, the
/// correction then walked the bus down a step every 10 s and knocked the pump
/// out of 2:1 (mode 3↔1 flap, peaks 0.31/0.55 A between the drops). Callers
/// must also require the *window peak* to be at the floor, not just one sample.
pub const IIN_DEAD_FLOOR_UA: u32 = 60_000;

// QC3 window invariants: the target must pass the 2:1 admission gate
// (`2*Vbat + 250 mV`) across the whole reachable capacity of the pack and must be
// reachable within the allowed number of INC pulses.
const _: () = assert!(MAX_BOOST_INC <= MAX_PULSE_CNT);
const _: () = assert!(
    PUMP_VIN_TARGET_MIN_UV as u32 >= ln8000::encoding::min_vin_for_switching_uv(4_500_000)
);
const _: () = assert!(PUMP_VIN_TARGET_MIN_UV as u32 <= estimated_vbus_uv(MAX_PULSE_CNT));
const _: () = assert!(
    ln8000::encoding::window_target_uv(4_500_000)
        >= ln8000::encoding::min_vin_for_switching_uv(4_500_000)
);
const _: () = assert!(
    ln8000::encoding::window_target_uv(4_500_000) <= PUMP_VIN_TARGET_CEIL_UV
);
const _: () = assert!(
    PUMP_VIN_TARGET_CEIL_UV <= estimated_vbus_uv(MAX_PULSE_CNT)
);
/// Delay between QC3 pulses (ms), nabu `msleep(40)`.
pub const PULSE_GAP_MS: u32 = 40;
/// How long to wait for APSD_DONE after rerun (ms).
pub const APSD_WAIT_MS: u32 = 2_000;
/// Poll period while waiting for APSD_DONE (ms).
///
/// Each poll is a synchronous SPMI transaction on the SUPERUSER bus, and the
/// negotiate holds that bus for the whole wait. At 50 ms (40 polls in the
/// window) the bus was busy ~80 % of the time: a second, independent user-mode
/// reader could only open `\Device\Spmi\SUPERUSER` in 21 of 100 attempts
/// (measured 18.09 on the tablet, two runs, `.627` and `.628` alike), which
/// starves every other client — including the acceptance instrument that has to
/// read `0x1307`/`0x1506` for its own verdict. 200 ms still checks ten times
/// inside the window, which is far more often than APSD needs.
pub const APSD_POLL_MS: u32 = 200;
/// Settle after OPTIONS1 enable before APSD rerun (ms).
pub const ENABLE_SETTLE_MS: u32 = 50;

// QC3.5 authenticate windows / pulse data — host-tested in `ln8000::qc35_auth`.
pub use ln8000::{
    decide_qc35_auth_attempt, qc35_cap_window, qc35_detect_window, qc35_icl_raw,
    qc35_power_limit_w, Qc35AuthGate, Qc35AuthPulse, ICL_RAW_QC35_2A, ICL_RAW_QC35_40W,
    QC35_18W_HI_UV, QC35_27W_HI_UV, QC35_27W_LO_UV, QC35_40W_LO_UV, QC35_AUTH_PULSE_GAP_MS,
    QC35_CAP_HI_UV, QC35_CAP_LO_UV, QC35_CAP_TIMEOUT_MS, QC35_CONFIRM_PULSES, QC35_DETECT_HI_UV,
    QC35_DETECT_LO_UV, QC35_DETECT_TIMEOUT_MS, QC35_PREP_MAX_INC, QC35_SRC_CAP_PULSES,
    QC35_STEP_UV, QC35_VIN_POLL_MS,
};
const _: () = assert!(ICL_RAW_QC35_40W == ICL_RAW_PUMP_3A);
const _: () = assert!(QC35_DETECT_LO_UV == 5_500_000);
const _: () = assert!(QC35_DETECT_HI_UV == 6_400_000);
const _: () = assert!(QC35_CAP_LO_UV == 6_650_000);
const _: () = assert!(QC35_CAP_HI_UV == 9_800_000);
const _: () = assert!(QC35_18W_HI_UV == 7_350_000);
const _: () = assert!(QC35_27W_LO_UV == 7_600_000);
const _: () = assert!(QC35_27W_HI_UV == 8_400_000);
const _: () = assert!(QC35_40W_LO_UV == 8_550_000);
const _: () = assert!(ICL_RAW_QC35_2A == 40);

/// Vin still counts as “~5 V stayed” after FORCE / failed elevate (µV).
pub const FIVE_V_STAY_MAX_UV: i32 = 6_000_000;

/// Which SPMI path carried the negotiate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum HvdcpTransport {
    /// Not opened.
    None = 0,
    /// `\Device\Spmi\SUPERUSER`.
    Superuser = 1,
    /// Usbin Resource Hub connection.
    UsbinRh = 2,
}

impl HvdcpTransport {
    /// Numeric code for registry marks.
    #[must_use]
    pub const fn code(self) -> u32 {
        self as u32
    }
}

/// Soft runtime for the HVDCP machine (pulse count is software-only).
#[derive(Debug, Clone, Copy)]
pub struct HvdcpState {
    /// Soft QC3 pulse counter (`chg->pulse_cnt`).
    pub pulse_cnt: u32,
    /// Last `APSD_STATUS` byte.
    pub apsd_status: u8,
    /// Last `APSD_RESULT_STATUS` byte.
    pub apsd_result: u8,
    /// Last `USBIN_OPTIONS_1_CFG` value written/read.
    pub options1: u8,
    /// Machine phase (registry `HvdcpPhase`).
    pub phase: HvdcpPhase,
    /// Preferred SPMI address endianness after first successful read (Usbin RH).
    pub big_endian: bool,
    /// Whether endianness has been confirmed on this gate.
    pub endian_known: bool,
    /// Active transport for the last negotiate.
    pub transport: HvdcpTransport,
    /// `CMD_HVDCP_2` carries `FORCE_9V` and the brick holds a QC2 level.
    ///
    /// Vendor writes `FORCE_9V` **once** (`smb5-lib.c:8376`) and never touches
    /// the register again for that adapter: the level is held by the bit, not by
    /// pulses. QC2 has no continuous mode, so an INC/DEC here is both useless
    /// and - with a raw write - fatal to the latch. Live 19.09 on MDY-11-EP:
    /// peak 8.224 V, then the post-path pulse wrote `0x01`, the brick folded back
    /// to ~4.9 V and the landing classified it as a 5 V source.
    pub force9v_latched: bool,
}

impl HvdcpState {
    /// Idle state before any negotiation.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            pulse_cnt: 0,
            apsd_status: 0,
            apsd_result: 0,
            options1: 0,
            phase: HvdcpPhase::Idle,
            big_endian: true,
            endian_known: false,
            transport: HvdcpTransport::None,
            force9v_latched: false,
        }
    }
}

impl Default for HvdcpState {
    fn default() -> Self {
        Self::new()
    }
}

/// Coarse phase for registry / IOCTL reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum HvdcpPhase {
    /// Not started.
    Idle = 0,
    /// Writing HVDCP enable bits.
    Enable = 1,
    /// APSD rerun issued.
    ApsdRerun = 2,
    /// Waiting / reading APSD result.
    ApsdWait = 3,
    /// QC2 FORCE_9V path.
    Qc2Force9v = 4,
    /// QC3 pulse loop.
    Qc3Pulse = 5,
    /// Finished successfully.
    Done = 6,
    /// Failed (see registry `HvdcpErr`).
    Failed = 7,
    /// QC3.5 authenticate (`qc3p5_authenticate` pulse pattern).
    Qc35Auth = 8,
    /// 5 V high-current bypass (plain DCP / TA220 — AFC protocol unsupported).
    FiveVBypass = 9,
}

impl HvdcpPhase {
    /// Numeric code for registry / IOCTL.
    #[must_use]
    pub const fn code(self) -> u32 {
        self as u32
    }
}

/// Errors from the HVDCP path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HvdcpError {
    /// Neither SUPERUSER nor Usbin RH could be opened.
    UsbinUnavailable,
    /// A transport was selected but open/grant failed.
    BusOpenFailed,
    /// SPMI read/write failed.
    SpmiIo,
    /// APSD_DONE never asserted.
    ApsdTimeout,
    /// APSD finished but adapter is not DCP/QC2/QC3.
    NotQcAdapter,
}

impl HvdcpError {
    /// Stable negative code for IOCTL `error_code`.
    #[must_use]
    pub const fn code(self) -> i32 {
        match self {
            Self::UsbinUnavailable => -10,
            Self::BusOpenFailed => -11,
            Self::SpmiIo => -12,
            Self::ApsdTimeout => -13,
            Self::NotQcAdapter => -14,
        }
    }
}

/// Target VBUS for a 2:1 pump (µV) — the centre of the transfer band.
///
/// The band rides with the pack: the LN8000 moves power only while `Vin` sits
/// inside `[2*Vbat + 200, 2*Vbat + 400]` mV (`ln8000::encoding`), so a *fixed*
/// bus cannot serve a charging pack. Live 18.09 with the old fixed 9.5 V floor:
/// bus 9.888 V against a 9.04–9.24 V band → mode 3 (`SYS_STS = 0x04`) carrying
/// the 39 mA ADC floor, i.e. 0.38 W where 2:1 delivers 16–23 W.
///
/// Aims at the band centre because one QC3 step (200 mV) is as wide as the band
/// itself. Clamped up to [`PUMP_VIN_TARGET_ABS_MIN_UV`] so a low pack still
/// passes the 2:1 admission gate, and down to [`PUMP_VIN_TARGET_CEIL_UV`] so the
/// target is always reachable within [`MAX_PULSE_CNT`].
///
/// A zero `vbat_uv` (pack not read) falls back to the vendor 9.5 V floor.
#[must_use]
pub const fn target_vbus_uv(vbat_uv: u32) -> u32 {
    if vbat_uv == 0 {
        return PUMP_VIN_TARGET_MIN_UV as u32;
    }
    let target = ln8000::encoding::window_target_uv(vbat_uv);
    let target = if target < PUMP_VIN_TARGET_ABS_MIN_UV {
        PUMP_VIN_TARGET_ABS_MIN_UV
    } else {
        target
    };
    if target > PUMP_VIN_TARGET_CEIL_UV {
        PUMP_VIN_TARGET_CEIL_UV
    } else {
        target
    }
}

/// Top of the transfer band for `vbat_uv` (µV), or the absolute safety ceiling
/// when the pack is unknown — the bus is never driven past either.
#[must_use]
pub const fn trim_target_uv(vbat_uv: u32) -> i32 {
    if vbat_uv == 0 {
        return PUMP_VIN_TARGET_MIN_UV;
    }
    let top = ln8000::encoding::window_top_uv(vbat_uv);
    if top > PUMP_VIN_TRIM_UV as u32 {
        PUMP_VIN_TRIM_UV
    } else {
        top as i32
    }
}

/// Bottom of the transfer band for `vbat_uv` (µV) — trim never walks below it.
#[must_use]
pub const fn window_floor_uv(vbat_uv: u32) -> i32 {
    if vbat_uv == 0 {
        return PUMP_VIN_TARGET_MIN_UV;
    }
    let floor = ln8000::encoding::window_floor_uv(vbat_uv);
    if floor < PUMP_VIN_TARGET_ABS_MIN_UV {
        PUMP_VIN_TARGET_ABS_MIN_UV as i32
    } else {
        floor as i32
    }
}

/// Soft estimate of adapter VBUS from pulse count.
#[must_use]
pub const fn estimated_vbus_uv(pulse_cnt: u32) -> u32 {
    MICRO_5V_UV.saturating_add(QC3_STEP_UV.saturating_mul(pulse_cnt))
}

/// How many INC pulses are needed to reach [`target_vbus_uv`], capped at [`MAX_PULSE_CNT`].
#[must_use]
pub fn pulses_toward_target(vbat_uv: u32) -> u32 {
    let target = target_vbus_uv(vbat_uv);
    if target <= MICRO_5V_UV {
        return 0;
    }
    let delta = target - MICRO_5V_UV;
    let raw = delta.div_ceil(QC3_STEP_UV);
    raw.min(MAX_PULSE_CNT)
}

/// Merge QC2 max-voltage field to 9 V while preserving lower pulse-count bits.
#[must_use]
pub const fn with_qc2_max_9v(pulse_max_reg: u8) -> u8 {
    (pulse_max_reg & !MASK_HVDCP_PULSE_COUNT_MAX_QC2) | HVDCP_PULSE_COUNT_MAX_QC2_9V
}

/// Merge adapter-allow to Android nabu `5V_TO_12V` while preserving upper nibble.
#[must_use]
pub const fn with_adapter_allow_5v_to_12v(allow_reg: u8) -> u8 {
    (allow_reg & !MASK_USBIN_ADAPTER_ALLOW) | USBIN_ADAPTER_ALLOW_5V_TO_12V
}

/// True when `QC_CHANGE_STATUS` indicates a 9 V (or 5→9 reason) handshake.
#[must_use]
pub const fn qc_status_indicates_9v(qc_change: u8) -> bool {
    (qc_change & BIT_QC_9V) != 0 || (qc_change & BIT_QC_5V_TO_9V_REASON) != 0
}

/// True when Vin stayed near 5 V after FORCE/elevate (AFC/TA220 / plain DCP path).
#[must_use]
pub const fn vin_stayed_near_5v(vin_uv: i32) -> bool {
    vin_uv > 0 && vin_uv < FIVE_V_STAY_MAX_UV
}

// Cold-plug / SUPERUSER-retry + QC3.5 auth policy live in `ln8000` (host-tested).
pub use ln8000::{
    force9v_extended_deadline, force9v_step, input_present_from_vin,
    should_renegotiate_on_input_edge, should_schedule_superuser_retry, superuser_retry_due,
    Force9vWait, FORCE9V_HARD_CAP_MS, FORCE9V_RISE_UV, FORCE9V_SETTLE_MS, HVDCP_SUPERUSER_RETRY_MAX,
    HVDCP_SUPERUSER_RETRY_MS,
};
// APSD classification is host-tested in `ln8000::hvdcp_policy`.
use ln8000::{apsd_elevate_path, encoding::SWITCHING_MIN_VIN_UV, promote_qc_charger, ApsdElevate};

/// Sleep at PASSIVE_LEVEL for `ms` milliseconds.
pub(crate) fn sleep_ms(ms: u32) {
    let mut interval = wdk_sys::LARGE_INTEGER {
        QuadPart: -i64::from(ms).saturating_mul(10_000),
    };
    // SAFETY: PASSIVE_LEVEL; KernelMode (0), non-alertable.
    unsafe {
        let _ = wdk_sys::ntddk::KeDelayExecutionThread(0, 0, &raw mut interval);
    }
}

/// Write a diagnostic DWORD into Device Parameters.
fn mark(device: WDFDEVICE, name: &str, value: u32) {
    crate::mark_device_value(device, name, value);
}

/// Open Usbin SPMI Resource Hub (secondary path).
///
/// # Safety
///
/// PASSIVE_LEVEL; `device` is a live WDFDEVICE.
unsafe fn open_usbin(device: WDFDEVICE, usbin_id: u64) -> Result<SpbBus, HvdcpError> {
    // SAFETY: PASSIVE_LEVEL; connection id from `_CRS`.
    match unsafe { SpbBus::open(device, usbin_id, true) } {
        Ok(mut bus) => {
            bus.set_variant(0);
            Ok(bus)
        }
        Err(_) => Err(HvdcpError::BusOpenFailed),
    }
}

/// Active SPMI backend for one negotiate session.
enum Bus<'a> {
    Superuser(&'a mut SuperuserBus),
    Usbin(&'a mut SpbBus),
}

fn spmi_rw_usbin(
    bus: &mut SpbBus,
    addr: u16,
    value: Option<u8>,
    big_endian: bool,
) -> Result<u8, HvdcpError> {
    bus.transact_spmi16(addr, value, big_endian)
        .map_err(|_| HvdcpError::SpmiIo)
}

fn read_reg(bus: &mut Bus<'_>, state: &HvdcpState, addr: u16) -> Result<u8, HvdcpError> {
    match bus {
        Bus::Superuser(su) => su
            .read_u8(SPMI_SID_USBIN, addr)
            .map_err(|_| HvdcpError::SpmiIo),
        Bus::Usbin(rh) => spmi_rw_usbin(rh, addr, None, state.big_endian),
    }
}

fn write_reg(
    bus: &mut Bus<'_>,
    state: &HvdcpState,
    addr: u16,
    value: u8,
) -> Result<(), HvdcpError> {
    match bus {
        Bus::Superuser(su) => su
            .write_u8(SPMI_SID_USBIN, addr, value)
            .map_err(|_| HvdcpError::SpmiIo),
        Bus::Usbin(rh) => spmi_rw_usbin(rh, addr, Some(value), state.big_endian).map(|_| ()),
    }
}

/// Discover which address endianness the Usbin RH controller accepts (one-shot).
fn resolve_endian_usbin(bus: &mut SpbBus, state: &mut HvdcpState) -> Result<(), HvdcpError> {
    if state.endian_known {
        return Ok(());
    }
    if spmi_rw_usbin(bus, REG_APSD_STATUS, None, true).is_ok() {
        state.big_endian = true;
        state.endian_known = true;
        return Ok(());
    }
    if spmi_rw_usbin(bus, REG_APSD_STATUS, None, false).is_ok() {
        state.big_endian = false;
        state.endian_known = true;
        return Ok(());
    }
    Err(HvdcpError::SpmiIo)
}

/// Clear latched USBIN IRQs (safe no-op if HW ignores) before APSD/HVDCP.
fn clear_usbin_latched(bus: &mut Bus<'_>, state: &HvdcpState) {
    let _ = write_reg(bus, state, REG_INT_LATCHED_CLR, 0xFF);
}

/// Clear `BC1P2_START_ON_CC` so APSD is not gated on Type-C CC attach.
///
/// Android `qpnp-smb5.c` probe writes `TYPE_C_CFG_REG` with this bit cleared.
/// Live nabu Windows dump showed `0x1358=0xC0` (bit7 set) after enable — APSD
/// stayed at `0x1307=0x00` until timeout.
fn clear_apsd_cc_gate(bus: &mut Bus<'_>, state: &HvdcpState) -> Result<u8, HvdcpError> {
    let mut cfg = read_reg(bus, state, REG_TYPE_C_CFG)?;
    if cfg & BIT_BC1P2_START_ON_CC != 0 {
        cfg &= !BIT_BC1P2_START_ON_CC;
        write_reg(bus, state, REG_TYPE_C_CFG, cfg)?;
    }
    Ok(cfg)
}

/// Soft floor: raise USBIN ICL to 500 mA only when currently below that.
///
/// Never decreases ICL and never writes above [`ICL_RAW_500MA`]. Android votes
/// `SW_ICL_MAX=500mA` during early APSD / float paths. Live dump already had
/// `0x1370=0x0A` — this is then a no-op.
fn ensure_icl_floor_500ma(bus: &mut Bus<'_>, state: &HvdcpState) -> Result<u8, HvdcpError> {
    let current = read_reg(bus, state, REG_USBIN_ICL_CFG)?;
    if current >= ICL_RAW_500MA {
        return Ok(current);
    }
    write_reg(bus, state, REG_USBIN_ICL_CFG, ICL_RAW_500MA)?;
    let _ = write_reg(bus, state, REG_CMD_ICL_OVERRIDE, BIT_ICL_OVERRIDE);
    Ok(ICL_RAW_500MA)
}

/// Raise USBIN ICL for LN8000 2:1 after Vin is elevated.
///
/// Unlike the APSD 500 mA floor, this **may** increase an already-high ICL up
/// to [`ICL_RAW_PUMP_3A`] and always pulses `ICL_OVERRIDE`.
fn ensure_icl_for_pump(bus: &mut Bus<'_>, state: &HvdcpState) -> Result<u8, HvdcpError> {
    ensure_icl_at_least(bus, state, ICL_RAW_PUMP_3A)
}

/// Raise USBIN ICL to at least `target` (50 mA steps) and pulse override.
fn ensure_icl_at_least(
    bus: &mut Bus<'_>,
    state: &HvdcpState,
    target: u8,
) -> Result<u8, HvdcpError> {
    let current = read_reg(bus, state, REG_USBIN_ICL_CFG)?;
    let applied = if current >= target {
        current
    } else {
        write_reg(bus, state, REG_USBIN_ICL_CFG, target)?;
        target
    };
    let _ = write_reg(bus, state, REG_CMD_ICL_OVERRIDE, BIT_ICL_OVERRIDE);
    clear_usbin_suspend(bus, state);
    Ok(applied)
}

/// Best-effort clear USBIN suspend so APSD can draw detection current.
fn clear_usbin_suspend(bus: &mut Bus<'_>, state: &HvdcpState) {
    if let Ok(cmd_il) = read_reg(bus, state, REG_USBIN_CMD_IL) {
        if cmd_il & BIT_USBIN_SUSPEND != 0 {
            let _ = write_reg(bus, state, REG_USBIN_CMD_IL, cmd_il & !BIT_USBIN_SUSPEND);
        }
    }
}

/// Enable HVDCP + BC1.2 detect; clear autonomous mode (host drives pulses).
fn enable_hvdcp(bus: &mut Bus<'_>, state: &mut HvdcpState) -> Result<(), HvdcpError> {
    let mut options = read_reg(bus, state, REG_USBIN_OPTIONS_1)?;
    options |= BIT_HVDCP_EN | BIT_HVDCP_AUTH_ALG_EN | BIT_BC1P2_SRC_DETECT;
    options &= !BIT_HVDCP_AUTONOMOUS;
    write_reg(bus, state, REG_USBIN_OPTIONS_1, options)?;
    state.options1 = options;
    Ok(())
}

/// Unlock PMIC-side QC2 elevation gates (independent of PHY D+/D− ownership).
///
/// Android nabu writes `USBIN_ADAPTER_ALLOW_5V_TO_12V` and keeps QC2 pulse-max
/// at least 9 V. A 5 V-only allow / QC2-max field makes `FORCE_9V` a no-op even
/// when D+/D− are free.
fn ensure_hvdcp_voltage_gates(
    bus: &mut Bus<'_>,
    state: &HvdcpState,
) -> Result<(u8, u8), HvdcpError> {
    let allow = read_reg(bus, state, REG_USBIN_ADAPTER_ALLOW)?;
    let allow_new = with_adapter_allow_5v_to_12v(allow);
    if allow_new != allow {
        write_reg(bus, state, REG_USBIN_ADAPTER_ALLOW, allow_new)?;
    }

    let pulse_max = read_reg(bus, state, REG_HVDCP_PULSE_COUNT_MAX)?;
    let pulse_max_new = with_qc2_max_9v(pulse_max);
    if pulse_max_new != pulse_max {
        write_reg(bus, state, REG_HVDCP_PULSE_COUNT_MAX, pulse_max_new)?;
    }

    Ok((allow_new, pulse_max_new))
}

/// Sample HW QC voltage / pulse status after FORCE or pulse train.
fn sample_qc_status(bus: &mut Bus<'_>, state: &HvdcpState) -> Result<(u8, u8), HvdcpError> {
    sleep_ms(QC_STATUS_SETTLE_MS);
    let qc_change = read_reg(bus, state, REG_QC_CHANGE_STATUS)?;
    let qc_pulses = read_reg(bus, state, REG_QC_PULSE_COUNT)?;
    Ok((qc_change, qc_pulses))
}

/// Issue APSD rerun.
fn apsd_rerun(bus: &mut Bus<'_>, state: &HvdcpState) -> Result<(), HvdcpError> {
    write_reg(bus, state, REG_CMD_APSD, BIT_APSD_RERUN)
}

/// Poll until APSD_DONE or timeout.
fn wait_apsd_done(bus: &mut Bus<'_>, state: &mut HvdcpState) -> Result<(), HvdcpError> {
    let mut waited = 0_u32;
    loop {
        let status = read_reg(bus, state, REG_APSD_STATUS)?;
        state.apsd_status = status;
        if status & BIT_APSD_DONE != 0 {
            let result = read_reg(bus, state, REG_APSD_RESULT)?;
            state.apsd_result = result;
            return Ok(());
        }
        if waited >= APSD_WAIT_MS {
            return Err(HvdcpError::ApsdTimeout);
        }
        sleep_ms(APSD_POLL_MS);
        waited = waited.saturating_add(APSD_POLL_MS);
    }
}

/// Single INC pulse and soft counter update.
fn pulse_inc(bus: &mut Bus<'_>, state: &mut HvdcpState) -> Result<(), HvdcpError> {
    pulse_inc_gap(bus, state, PULSE_GAP_MS)
}

/// Single INC with an explicit inter-pulse gap (QC3.5 auth uses 5 ms).
fn pulse_inc_gap(
    bus: &mut Bus<'_>,
    state: &mut HvdcpState,
    gap_ms: u32,
) -> Result<(), HvdcpError> {
    if pulse_cmd_bit(bus, state, BIT_SINGLE_INC)? {
        state.pulse_cnt = state.pulse_cnt.saturating_add(1).min(MAX_PULSE_CNT);
    }
    sleep_ms(gap_ms);
    Ok(())
}

/// Single DEC pulse (soft counter floors at 0).
fn pulse_dec(bus: &mut Bus<'_>, state: &mut HvdcpState) -> Result<(), HvdcpError> {
    pulse_dec_gap(bus, state, PULSE_GAP_MS)
}

/// Single DEC with an explicit inter-pulse gap.
fn pulse_dec_gap(
    bus: &mut Bus<'_>,
    state: &mut HvdcpState,
    gap_ms: u32,
) -> Result<(), HvdcpError> {
    if pulse_cmd_bit(bus, state, BIT_SINGLE_DEC)? {
        state.pulse_cnt = state.pulse_cnt.saturating_sub(1);
    }
    sleep_ms(gap_ms);
    Ok(())
}

/// Sends one QC3 step bit the way the vendor does: masked, not absolute.
///
/// `smblib_dp_pulse` / `smblib_dm_pulse` (`smb5-lib.c:3893-3919`) both use
/// `smblib_masked_write(chg, CMD_HVDCP_2_REG, SINGLE_*, SINGLE_*)`, so every
/// other bit of the register survives the pulse. A raw write here cleared
/// `FORCE_9V`: one INC after the QC2 rise dropped the brick back to 5 V.
/// On a latched bus the pulse is skipped entirely — QC2 has no continuous mode.
/// Returns `true` if the pulse actually went out to the chip.
///
/// The caller needs this flag: under the latch the pulse counter must stand still
/// together with the pulse. Otherwise `estimated_vbus_uv` publishes a level that is
/// not on the bus (live measurement on 19.09 12:02: `SuVbusEst` 5.4 V against
/// measured 8.8 V - the counter counted pulses that `pulse_cmd_bit` had skipped).
fn pulse_cmd_bit(bus: &mut Bus<'_>, state: &HvdcpState, bit: u8) -> Result<bool, HvdcpError> {
    if state.force9v_latched {
        return Ok(false);
    }
    let cur = read_reg(bus, state, REG_CMD_HVDCP_2).unwrap_or(0);
    write_reg(
        bus,
        state,
        REG_CMD_HVDCP_2,
        (cur & !(BIT_SINGLE_INC | BIT_SINGLE_DEC)) | bit,
    )?;
    Ok(true)
}

/// QC2 force-9V path (`smblib_force_vbus_voltage(FORCE_9V_BIT)`).
///
/// One write, like the vendor's: the level is held by the bit until something
/// clears it, and from here on the bus is not trimmable (no QC3 continuous
/// mode), so [`pulse_cmd_bit`] refuses to pulse while the latch is up.
fn force_9v(bus: &mut Bus<'_>, state: &mut HvdcpState) -> Result<(), HvdcpError> {
    let existing = read_reg(bus, state, REG_CMD_HVDCP_2).unwrap_or(0);
    write_reg(bus, state, REG_CMD_HVDCP_2, existing | BIT_FORCE_9V)?;
    state.force9v_latched = true;
    state.pulse_cnt = 20;
    sleep_ms(100);
    Ok(())
}

/// Best-effort return to 5 V on failure after we already raised VBUS.
fn safe_force_5v(bus: &mut Bus<'_>, state: &mut HvdcpState) {
    let _ = write_reg(bus, state, REG_CMD_HVDCP_2, BIT_FORCE_5V);
    state.force9v_latched = false;
}

/// Publish step marks for remote diagnostics.
fn publish_marks(device: WDFDEVICE, state: &HvdcpState, err: Option<HvdcpError>) {
    mark(device, "HvdcpPhase", state.phase.code());
    mark(device, "HvdcpVia", state.transport.code());
    mark(device, "HvdcpEnSt", u32::from(state.options1));
    mark(device, "ApsdSt", u32::from(state.apsd_status));
    mark(device, "ApsdResult", u32::from(state.apsd_result));
    mark(device, "PulseCnt", state.pulse_cnt);
    mark(device, "HvdcpBe", u32::from(state.big_endian));
    mark(device, "SuPulseCnt", state.pulse_cnt);
    mark(device, "SuApsdResult", u32::from(state.apsd_result));
    mark(device, "SuVbusEst", estimated_vbus_uv(state.pulse_cnt));
    // The `FORCE_9V` latch is state that outlives the tick and suppresses the pulse
    // path. Without the mark, "the bus stands still" is indistinguishable from
    // "pulses go out for nothing".
    mark(device, "QcLatch", u32::from(state.force9v_latched));
    match err {
        Some(e) => mark(device, "HvdcpErr", e.code() as u32),
        None => mark(device, "HvdcpErr", 0),
    }
}

/// Poll VBUS while the QC signature is asserted, tolerating a slow brick ramp.
///
/// Returns `(last, peak)` in µV. The deadline moves out once when a sample shows
/// the brick answered (`>= FORCE9V_RISE_UV`) and never passes the host-tested
/// hard cap, so a source that ignores the signature cannot hold the SUPERUSER
/// bus for long.
///
/// The gate must be met on [`FORCE9V_CONFIRM_N`] consecutive samples: a QC3 brick
/// answers the legacy QC2 request with a spike and drops back, and a single
/// sample above the gate used to end the wait on a level that never existed
/// (live 19.09 11:07: `SuDcp9vVin` = 8.224 V, landing read ~4.8 V).
fn wait_force9v_rise(read_vin: &mut impl FnMut() -> i32) -> (i32, i32) {
    let mut waited = 0_u32;
    let mut deadline = FORCE9V_SETTLE_MS;
    let mut extended = false;
    let mut last = read_vin();
    let mut peak = last;
    let mut confirm = 0_u32;
    loop {
        if last > peak {
            peak = last;
        }
        if last >= SWITCHING_MIN_VIN_UV {
            confirm = confirm.saturating_add(1);
            if confirm >= FORCE9V_CONFIRM_N {
                break;
            }
        } else {
            confirm = 0;
            match force9v_step(last, waited, deadline, extended, SWITCHING_MIN_VIN_UV) {
                Force9vWait::Stop => break,
                Force9vWait::Extend => {
                    deadline = force9v_extended_deadline(deadline);
                    extended = true;
                }
                Force9vWait::Continue => {}
            }
        }
        if waited >= FORCE9V_HARD_CAP_MS {
            break;
        }
        sleep_ms(FORCE9V_POLL_MS);
        waited = waited.saturating_add(FORCE9V_POLL_MS);
        last = read_vin();
        if last > peak {
            peak = last;
        }
    }
    (last, peak)
}

/// Poll until `pred(vin)` or timeout; returns last Vin sample.
fn wait_vin_window<F, P>(
    read_vin: &mut F,
    timeout_ms: u32,
    mut pred: P,
) -> Option<i32>
where
    F: FnMut() -> i32,
    P: FnMut(i32) -> bool,
{
    let mut waited = 0_u32;
    loop {
        let vin = read_vin();
        if pred(vin) {
            return Some(vin);
        }
        if waited >= timeout_ms {
            return None;
        }
        sleep_ms(QC35_VIN_POLL_MS);
        waited = waited.saturating_add(QC35_VIN_POLL_MS);
    }
}

/// Apply one QC3.5 auth pulse from the shared sequence tables.
fn pulse_auth_step(
    bus: &mut Bus<'_>,
    state: &mut HvdcpState,
    pulse: Qc35AuthPulse,
) -> Result<(), ()> {
    match pulse {
        Qc35AuthPulse::Inc => pulse_inc_gap(bus, state, QC35_AUTH_PULSE_GAP_MS).map_err(|_| ()),
        Qc35AuthPulse::Dec => pulse_dec_gap(bus, state, QC35_AUTH_PULSE_GAP_MS).map_err(|_| ()),
    }
}

/// Soft prep: INC from ~5 V into the 5.5–6.4 V QC3.5 detect window.
fn prep_qc35_detect_window(
    bus: &mut Bus<'_>,
    state: &mut HvdcpState,
    read_vin: &mut impl FnMut() -> i32,
) -> Result<i32, ()> {
    let mut vin = read_vin();
    match decide_qc35_auth_attempt(vin) {
        Qc35AuthGate::SkipVinInvalid | Qc35AuthGate::SkipVinTooHigh => return Err(()),
        Qc35AuthGate::Attempt => {}
    }
    let mut prep = 0_u32;
    while !qc35_detect_window(vin) && prep < QC35_PREP_MAX_INC {
        if pulse_inc_gap(bus, state, QC35_AUTH_PULSE_GAP_MS).is_err() {
            return Err(());
        }
        prep = prep.saturating_add(1);
        vin = read_vin();
        if matches!(
            decide_qc35_auth_attempt(vin),
            Qc35AuthGate::SkipVinInvalid | Qc35AuthGate::SkipVinTooHigh
        ) {
            return Err(());
        }
    }
    wait_vin_window(read_vin, QC35_DETECT_TIMEOUT_MS, qc35_detect_window).ok_or(())
}

/// Android-like `qc3p5_authenticate`: +-+-+- then ++−−, classify SRC_CAP.
///
/// On failure returns `Err(())` — caller **must** keep the QC3 path (no APSD_RERUN).
/// On success returns power limit in watts (18 / 27 / 40) and leaves ICL voted.
fn attempt_qc35_authenticate(
    device: WDFDEVICE,
    bus: &mut Bus<'_>,
    state: &mut HvdcpState,
    read_vin: &mut impl FnMut() -> i32,
) -> Result<u32, ()> {
    mark(device, "SuQc35Auth", 0);
    mark(device, "SuQc35PowerW", 0);

    // Soft 500 mA floor during auth (Android QC3P5_VOTER).
    let _ = ensure_icl_floor_500ma(bus, state);

    let detect_vin = match prep_qc35_detect_window(bus, state, read_vin) {
        Ok(v) => v,
        Err(()) => {
            mark(device, "SuQc35Fail", 1); // detect / SkipVinTooHigh / prep
            return Err(());
        }
    };
    mark(device, "SuQc35DetVin", detect_vin.max(0) as u32);

    // Issue +-+-+- to request SRC CAP (data: `QC35_SRC_CAP_PULSES`).
    for &pulse in QC35_SRC_CAP_PULSES {
        if pulse_auth_step(bus, state, pulse).is_err() {
            mark(device, "SuQc35Fail", 2); // pulse train
            return Err(());
        }
    }

    let Some(cap_vin) = wait_vin_window(read_vin, QC35_CAP_TIMEOUT_MS, qc35_cap_window) else {
        // Live MDY-11: DetVin≈5.58 V then SRC_CAP window never hit → not QC3.5.
        mark(device, "SuQc35Fail", 3); // SRC_CAP timeout / miss
        mark(device, "SuQc35CapVin", read_vin().max(0) as u32);
        return Err(());
    };
    mark(device, "SuQc35CapVin", cap_vin.max(0) as u32);

    let Some(power_w) = qc35_power_limit_w(cap_vin) else {
        mark(device, "SuQc35Fail", 4); // gap between power bands
        return Err(());
    };

    // Issue ++−− to confirm transition to QC3.5 (`QC35_CONFIRM_PULSES`).
    for &pulse in QC35_CONFIRM_PULSES {
        if pulse_auth_step(bus, state, pulse).is_err() {
            mark(device, "SuQc35Fail", 5); // confirm pulse
            return Err(());
        }
    }

    let icl = qc35_icl_raw(power_w);
    let _ = ensure_icl_at_least(bus, state, icl);

    mark(device, "SuQc35Auth", 1);
    mark(device, "SuQc35Fail", 0);
    mark(device, "SuQc35PowerW", power_w);
    mark(device, "SuIclQc35", u32::from(icl));
    // Adapter-side fine step (20 mV); PMIC INC bit is unchanged vs QC3.
    mark(device, "SuQc35StepUv", QC35_STEP_UV);
    Ok(power_w)
}

/// Core negotiate over an already-opened bus.
///
/// `read_vin` supplies LN8000 Vin (µV) for QC3.5 VBUS windows; return `0` when
/// ADC is unavailable (auth then fails open into the QC3 path).
fn negotiate_on_bus(
    device: WDFDEVICE,
    bus: &mut Bus<'_>,
    vbat_uv: u32,
    state: &mut HvdcpState,
    read_vin: &mut impl FnMut() -> i32,
) -> Result<(), HvdcpError> {
    // AFC protocol is not present on nabu DT / Windows path — mark explicitly.
    mark(device, "SuAfcProto", 0);
    mark(device, "SuAfc5vPath", 0);
    mark(device, "SuQc35Auth", 0);
    mark(device, "SuQc35Fail", 0);
    mark(device, "SuQc35PowerW", 0);
    state.pulse_cnt = 0;

    clear_usbin_latched(bus, state);
    clear_usbin_suspend(bus, state);

    // APSD must not wait on CC: mirror qpnp-smb5 probe clear of BC1P2_START_ON_CC.
    match clear_apsd_cc_gate(bus, state) {
        Ok(cfg) => mark(device, "SuTypecCfg", u32::from(cfg)),
        Err(err) => {
            mark(device, "SuTypecCfg", 0xFFFF_FFFF);
            state.phase = HvdcpPhase::Failed;
            publish_marks(device, state, Some(err));
            return Err(err);
        }
    }

    match ensure_icl_floor_500ma(bus, state) {
        Ok(icl) => mark(device, "SuIclRaw", u32::from(icl)),
        Err(err) => {
            // Soft floor failure is non-fatal: continue with existing ICL.
            mark(device, "SuIclRaw", err.code() as u32);
        }
    }

    match ensure_hvdcp_voltage_gates(bus, state) {
        Ok((allow, pulse_max)) => {
            mark(device, "SuAdapterAllow", u32::from(allow));
            mark(device, "SuPulseMax", u32::from(pulse_max));
        }
        Err(err) => {
            mark(device, "SuAdapterAllow", err.code() as u32);
            mark(device, "SuPulseMax", err.code() as u32);
        }
    }

    // Live nabu: APSD_RERUN while QC3 continuous is latched demotes 0x48 -> plain
    // DCP 0x08 and FORCE_9V then no-ops. QC2 already present must also skip
    // rerun (FORCE_9V without demoting). Plain DCP after enable (cold-plug) can
    // take FORCE_9V without a destructive APSD_RERUN when APSD_DONE is already set.
    let pre_status = read_reg(bus, state, REG_APSD_STATUS).unwrap_or_default();
    let pre_result = read_reg(bus, state, REG_APSD_RESULT).unwrap_or_default();
    let pre_qc = read_reg(bus, state, REG_QC_CHANGE_STATUS).unwrap_or_default();
    mark(device, "SuApsdPre", u32::from(pre_status));
    mark(device, "SuApsdPreR", u32::from(pre_result));
    mark(device, "SuQcPre", u32::from(pre_qc));

    let already_qc3 = (pre_result & BIT_QC3) != 0 || (pre_qc & BIT_QC_CONTINUOUS) != 0;
    let already_qc2 = (pre_result & BIT_QC2) != 0
        || ((pre_status & BIT_QC_CHARGER) != 0
            && (pre_result & BIT_DCP) != 0
            && (pre_result & BIT_QC3) == 0
            && !already_qc3);
    let already_dcp = (pre_result & BIT_DCP) != 0 && !already_qc3 && !already_qc2;
    let already_done = (pre_status & BIT_APSD_DONE) != 0;
    // Skip APSD_RERUN only when QC2/QC3 is already latched — rerun demotes
    // QC3 continuous (0x48) to plain DCP (0x08) and FORCE_9V then no-ops.
    // Never skip plain DCP: MDY-11 and similar bricks often sit at 0x08 after a
    // prior demote; enable_hvdcp + APSD_RERUN is required to re-classify QC3.
    let skip_apsd_rerun = already_done && (already_qc3 || already_qc2);
    let _ = already_dcp; // retained for SuApsdSkipDcp telemetry below

    state.phase = HvdcpPhase::Enable;
    publish_marks(device, state, None);
    if let Err(err) = enable_hvdcp(bus, state) {
        state.phase = HvdcpPhase::Failed;
        mark(device, "HvdcpEnSt", 0xFFFF_FFFF);
        mark(device, "SuHvdcpEnSt", 0xFFFF_FFFF);
        publish_marks(device, state, Some(err));
        return Err(err);
    }
    mark(device, "HvdcpEnSt", 0);
    mark(device, "SuHvdcpEnSt", u32::from(state.options1));
    sleep_ms(ENABLE_SETTLE_MS);

    if skip_apsd_rerun {
        mark(device, "SuApsdSkip", 1);
        mark(device, "SuApsdSkipQc2", u32::from(already_qc2));
        mark(device, "SuApsdSkipDcp", 0);
        state.apsd_status = pre_status;
        state.apsd_result = pre_result;
        mark(device, "SuApsdRerunSt", 0);
        mark(device, "ApsdSt", 0);
        mark(device, "ApsdResult", u32::from(state.apsd_result));
        mark(device, "SuApsdResult", u32::from(state.apsd_result));
    } else {
        mark(device, "SuApsdSkip", 0);
        mark(device, "SuApsdSkipQc2", 0);
        // Telemetry: would have been a DCP-only skip under the old policy.
        mark(device, "SuApsdSkipDcp", u32::from(already_dcp));
        state.phase = HvdcpPhase::ApsdRerun;
        publish_marks(device, state, None);
        if let Err(err) = apsd_rerun(bus, state) {
            state.phase = HvdcpPhase::Failed;
            mark(device, "SuApsdRerunSt", err.code() as u32);
            publish_marks(device, state, Some(err));
            return Err(err);
        }
        mark(device, "SuApsdRerunSt", 0);

        state.phase = HvdcpPhase::ApsdWait;
        publish_marks(device, state, None);
        if let Err(err) = wait_apsd_done(bus, state) {
            state.phase = HvdcpPhase::Failed;
            publish_marks(device, state, Some(err));
            return Err(err);
        }
        mark(device, "ApsdSt", 0);
        mark(device, "ApsdResult", u32::from(state.apsd_result));
        mark(device, "SuApsdResult", u32::from(state.apsd_result));

        // If rerun demoted QC3 continuous to plain DCP, prefer the pre-rerun QC3
        // signature so we still take the pulse path (FORCE_9V is a no-op there).
        if (state.apsd_result & BIT_QC3) == 0
            && (pre_result & BIT_QC3) != 0
            && (state.apsd_result & BIT_DCP) != 0
        {
            mark(device, "SuApsdDemote", 1);
            state.apsd_result = pre_result;
            state.apsd_status = pre_status;
            mark(device, "ApsdResult", u32::from(state.apsd_result));
            mark(device, "SuApsdResult", u32::from(state.apsd_result));
        } else if (state.apsd_result & BIT_QC2) == 0
            && (pre_result & BIT_QC2) != 0
            && (state.apsd_result & BIT_DCP) != 0
        {
            // Same demote guard for QC2 → DCP; keep FORCE_9V eligible.
            mark(device, "SuApsdDemote", 1);
            state.apsd_result = pre_result;
            state.apsd_status = pre_status;
            mark(device, "ApsdResult", u32::from(state.apsd_result));
            mark(device, "SuApsdResult", u32::from(state.apsd_result));
        } else {
            mark(device, "SuApsdDemote", 0);
        }
    }

    let qc3_continuous = (pre_qc & BIT_QC_CONTINUOUS) != 0;
    // Vendor promotion before classification (`smb5-lib.c:610-630`): a result the
    // PMIC reports as DCP/unknown while `APSD_STATUS` carries `QC_CHARGER_BIT` is
    // HVDCP2 for Android, not a plain DCP. Without it a QC brick that does not
    // raise the QC_2P0/QC_3P0 result bits is classified as a DCP and — on a build
    // that does not elevate DCP — never sees 9 V. Widens only; QC3 is untouched.
    let promoted = promote_qc_charger(state.apsd_result, state.apsd_status);
    if promoted != state.apsd_result {
        mark(device, "ApsdPromote", 1);
        mark(device, "ApsdPromoted", u32::from(promoted));
        state.apsd_result = promoted;
        mark(device, "ApsdResult", u32::from(promoted));
        mark(device, "SuApsdResult", u32::from(promoted));
    } else {
        mark(device, "ApsdPromote", 0);
    }
    let result = state.apsd_result;
    // APSD → elevate path (`smb5-lib.c` APSD table). `0x28` (`DCP|QC_2P0`) is
    // HVDCP2 (QC2): `FORCE_9V`, never a "5 V AFC" classification. A brick that
    // stays at ~5 V after FORCE_9V is handled by the post-path retreat to the
    // 5 V bypass (`raise_icl_for_5v_bypass` / `vin_stayed_near_5v`).
    let elevate = apsd_elevate_path(result, qc3_continuous);
    // `SuApsdAfc5v` is a **retreat** flag: it is set only when the post-path
    // lands in the 5 V high-current bypass, not when the block is identified.
    mark(device, "SuApsdAfc5v", 0);

    match elevate {
        ApsdElevate::Qc3Pulse => {
            // QC3.5 auth first (Android hvdcp_3p0_auth_done → qc3p5_authenticate).
            // Failure falls through to QC3 elevate — never APSD_RERUN / demote.
            state.phase = HvdcpPhase::Qc35Auth;
            publish_marks(device, state, None);
            let _ = attempt_qc35_authenticate(device, bus, state, read_vin);

            state.phase = HvdcpPhase::Qc3Pulse;
            // The pulse counter is shared across the whole raise and is **not reset**
            // after the QC3.5 prep: the prep already lifted the adapter into the
            // 5.5-6.4 V window with its own INC, and starting the count over would
            // make the real pulse count exceed the `MAX_PULSE_CNT` ceiling (23 x 200 mV
            // from 5 V is 9.6 V), while `estimated_vbus_uv` would read too low.
            publish_marks(device, state, None);
            let want = pulses_toward_target(vbat_uv);
            mark(device, "HvdcpTarget", target_vbus_uv(vbat_uv));
            mark(device, "HvdcpWantPulses", want);
            // The raise is closed-loop on the pump ADC, the way the vendor raise-loop
            // reads the result at every step: pulse -> read Vin -> stop at the target.
            // A blind burst is dangerous precisely because after the prep the baseline
            // is no longer 5 V, and 23 pulses from it give ~11 V - above
            // `PUMP_VIN_TRIM_UV`, where live silicon latches `VIN_OV` and 2:1 cannot
            // be engaged afterwards (verified in the `trim_vin_for_pump` comment).
            // Any shortfall against the window floor is made up by
            // `boost_vin_for_pump` after the negotiate, so stopping early is safe.
            // The stop is on the `2*Vbat` target, not on a fixed 9.5 V: with a full
            // pack 9.5 V sits ABOVE the transfer band (live measurement on 18.09).
            let target_now = target_vbus_uv(vbat_uv) as i32;
            let ceiling_now = trim_target_uv(vbat_uv);
            let mut vin_now = read_vin();
            for _ in 0..want {
                if state.pulse_cnt >= MAX_PULSE_CNT {
                    break;
                }
                if vin_now >= target_now {
                    break;
                }
                if let Err(err) = pulse_inc(bus, state) {
                    state.phase = HvdcpPhase::Failed;
                    safe_force_5v(bus, state);
                    publish_marks(device, state, Some(err));
                    return Err(err);
                }
                mark(device, "PulseCnt", state.pulse_cnt);
                mark(device, "SuPulseCnt", state.pulse_cnt);
                vin_now = read_vin();
                if vin_now > ceiling_now {
                    // Safety ceiling: above the transfer band pulses only latch
                    // VIN_OV and charge nothing.
                    break;
                }
            }
            mark(device, "SuQcEndVin", u32::try_from(vin_now.max(0)).unwrap_or(0));
        }
        ApsdElevate::Force9v => {
            if (result & BIT_QC2) != 0 {
                // QC2 (0x28): the kernel does write `FORCE_9V` for nabu. The
                // "we use our own qc2 method" early return (`smb5-lib.c:4143-4146`)
                // belongs to the **userspace** `DP_DM` entry; the kernel's APSD
                // handler runs in the VAYU/NABU arm of the same file and does
                // `smblib_force_vbus_voltage(chg, FORCE_9V_BIT)` followed by an
                // ICL vote of `HVDCP2_CURRENT_UA` (`smb5-lib.c:8373-8381`).
                // The write alone never moved live Vin, and the missing vote is
                // the likely reason: elevation without an input budget leaves the
                // brick at its own default current class.
                state.phase = HvdcpPhase::Qc2Force9v;
                publish_marks(device, state, None);
                if let Err(err) = force_9v(bus, state) {
                    state.phase = HvdcpPhase::Failed;
                    safe_force_5v(bus, state);
                    publish_marks(device, state, Some(err));
                    return Err(err);
                }
                match ensure_icl_at_least(bus, state, ICL_RAW_HVDCP2_1P5A) {
                    Ok(icl) => mark(device, "SuQc2Icl", u32::from(icl)),
                    Err(err) => mark(device, "SuQc2Icl", err.code() as u32),
                }
            } else {
                // Plain DCP (0x08): Android only votes `DCP_CURRENT_UA` (2 A,
                // `smb5-lib.h:248`) and never raises VBUS — but a QC2/QC3 brick
                // *presents* DCP until the device drives the QC signature, so
                // "DCP" is not "5 V only". The 5 V route on this build is
                // thermally capped (1:1 at 2.7 A burns ~2 W in the die, so the
                // guard parks the charge in the bypass retreat), which is why
                // the elevation attempt stays in: one bounded `FORCE_9V` per
                // input session with the vendor's HVDCP2 current vote. No rise
                // inside the settle window → drop the signature
                // (`safe_force_5v`), keep the 2 A vote and let the caller's
                // post-path land in the 5 V high-current route.
                mark(device, "SuDcpNoElevate", 0);
                state.phase = HvdcpPhase::Qc2Force9v;
                publish_marks(device, state, None);
                if let Err(err) = force_9v(bus, state) {
                    state.phase = HvdcpPhase::Failed;
                    safe_force_5v(bus, state);
                    publish_marks(device, state, Some(err));
                    return Err(err);
                }
                match ensure_icl_at_least(bus, state, ICL_RAW_HVDCP2_1P5A) {
                    Ok(icl) => mark(device, "SuDcpIcl", u32::from(icl)),
                    Err(err) => mark(device, "SuDcpIcl", err.code() as u32),
                }
                let (vin_end, vin_peak) = wait_force9v_rise(read_vin);
                mark(device, "SuDcp9vVin", u32::try_from(vin_end.max(0)).unwrap_or(0));
                mark(
                    device,
                    "SuDcpPeakVin",
                    u32::try_from(vin_peak.max(0)).unwrap_or(0),
                );
                // The decision is on the HOLD, not on the peak: `wait_force9v_rise`
                // stops at the first sample above the gate, while the bus can swing
                // 4.6 <-> 8.2 V (live 19.09 11:07: `SuDcp9vVin` = 8.224 V, yet the
                // landing read ~4.8 V a tick later). The level counts as settled only
                // if 8.0 V hold for two consecutive reads.
                let mut vin_after = read_vin();
                let mut hold = 0_u32;
                for _ in 0..4 {
                    if vin_after >= SWITCHING_MIN_VIN_UV {
                        hold = hold.saturating_add(1);
                    } else {
                        hold = 0;
                    }
                    if hold >= 2 {
                        break;
                    }
                    sleep_ms(FORCE9V_POLL_MS);
                    vin_after = read_vin();
                }
                mark(device, "SuDcpHoldN", hold);
                // FORCE_9V is a **legacy QC2 command**. A QC3 brick answers it with a
                // spike and returns to its own continuous-mode step; the same MDY-11-EP
                // held 7.952 V for thirty seconds straight on QC3 pulses (measurement
                // 10:46). So an unsettled level is driven with the native protocol:
                // 5 V baseline (`smblib_force_vbus_voltage(FORCE_5V_BIT)` at the head
                // of the vendor `raise_qc3_vbus_work`), then INC steps with the result
                // read at every step.
                if hold < 2 {
                    mark(device, "SuDcpNoElevate", 1);
                    safe_force_5v(bus, state);
                    state.pulse_cnt = 0;
                    let target_now = target_vbus_uv(vbat_uv) as i32;
                    let mut inc = 0_u32;
                    let mut vin_now = vin_after;
                    while vin_now < target_now
                        && inc < MAX_PULSE_CNT
                        && state.pulse_cnt < MAX_PULSE_CNT
                    {
                        if pulse_inc(bus, state).is_err() {
                            break;
                        }
                        inc = inc.saturating_add(1);
                        vin_now = read_vin();
                    }
                    mark(device, "SuDcpPulseRaise", inc);
                    mark(
                        device,
                        "SuDcpPulseVin",
                        u32::try_from(vin_now.max(0)).unwrap_or(0),
                    );
                    if vin_now < FORCE9V_RISE_UV {
                        // The brick did not answer the pulses either: this is not a QC
                        // adapter, retreat to the 5 V bypass with the vendor's 2 A.
                        safe_force_5v(bus, state);
                        match ensure_icl_at_least(bus, state, ICL_RAW_DCP_2A) {
                            Ok(icl) => mark(device, "SuDcpIcl", u32::from(icl)),
                            Err(err) => mark(device, "SuDcpIcl", err.code() as u32),
                        }
                    } else {
                        match ensure_icl_at_least(bus, state, ICL_RAW_HVDCP2_1P5A) {
                            Ok(icl) => mark(device, "SuDcpIcl", u32::from(icl)),
                            Err(err) => mark(device, "SuDcpIcl", err.code() as u32),
                        }
                    }
                }
            }
        }
        ApsdElevate::Reject => {
            state.phase = HvdcpPhase::Failed;
            publish_marks(device, state, Some(HvdcpError::NotQcAdapter));
            return Err(HvdcpError::NotQcAdapter);
        }
    }

    match sample_qc_status(bus, state) {
        Ok((qc_change, qc_pulses)) => {
            mark(device, "SuQcChgSt", u32::from(qc_change));
            mark(device, "SuQcPulseHw", u32::from(qc_pulses));
            mark(
                device,
                "SuQc9vHint",
                u32::from(qc_status_indicates_9v(qc_change)),
            );
        }
        Err(err) => {
            mark(device, "SuQcChgSt", err.code() as u32);
            mark(device, "SuQcPulseHw", err.code() as u32);
            mark(device, "SuQc9vHint", 0);
        }
    }

    state.phase = HvdcpPhase::Done;
    mark(device, "HvdcpEstVbus", estimated_vbus_uv(state.pulse_cnt));
    mark(device, "SuVbusEst", estimated_vbus_uv(state.pulse_cnt));
    publish_marks(device, state, None);
    Ok(())
}

/// Full negotiate: SUPERUSER first, Usbin RH secondary.
///
/// `read_vin` supplies LN8000 Vin (µV) for QC3.5 windows; pass `|| 0` when
/// the pump ADC is unavailable (auth fails open into QC3).
///
/// # Errors
///
/// Returns [`HvdcpError::UsbinUnavailable`] only when **both** SUPERUSER and
/// Usbin RH are unavailable (slot full / no overlay).
///
/// # Safety
///
/// PASSIVE_LEVEL; `device` is valid for the lifetime of the call.
pub unsafe fn run_negotiate(
    device: WDFDEVICE,
    usbin_id: Option<u64>,
    vbat_uv: u32,
    state: &mut HvdcpState,
    read_vin: &mut impl FnMut() -> i32,
) -> Result<(), HvdcpError> {
    state.phase = HvdcpPhase::Enable;
    state.transport = HvdcpTransport::None;
    publish_marks(device, state, None);

    // --- Primary: SUPERUSER (open once → grant → R/W → Drop closes) ---
    // SAFETY: PASSIVE_LEVEL; device live.
    match unsafe { SuperuserBus::open(device) } {
        Ok(mut su) => {
            mark(device, "SuHvdcpOpen", 0);
            if let Err(st) = su.grant(SPMI_PERI_USBIN) {
                mark(device, "SuHvdcpOpen", st as u32);
                // Fall through to Usbin RH if grant failed.
            } else {
                state.transport = HvdcpTransport::Superuser;
                state.endian_known = true;
                state.big_endian = true;
                mark(device, "HvdcpVia", HvdcpTransport::Superuser.code());
                let mut bus = Bus::Superuser(&mut su);
                return negotiate_on_bus(device, &mut bus, vbat_uv, state, read_vin);
            }
        }
        Err(st) => {
            mark(device, "SuHvdcpOpen", st as u32);
        }
    }

    // --- Secondary: Usbin Resource Hub (ACPI overlay) ---
    let Some(id) = usbin_id else {
        state.phase = HvdcpPhase::Failed;
        state.transport = HvdcpTransport::None;
        publish_marks(device, state, Some(HvdcpError::UsbinUnavailable));
        return Err(HvdcpError::UsbinUnavailable);
    };

    // SAFETY: PASSIVE_LEVEL; connection id from `_CRS`.
    let mut rh = match unsafe { open_usbin(device, id) } {
        Ok(bus) => bus,
        Err(err) => {
            state.phase = HvdcpPhase::Failed;
            publish_marks(device, state, Some(err));
            return Err(err);
        }
    };
    if let Err(err) = resolve_endian_usbin(&mut rh, state) {
        state.phase = HvdcpPhase::Failed;
        publish_marks(device, state, Some(err));
        return Err(err);
    }
    state.transport = HvdcpTransport::UsbinRh;
    mark(device, "HvdcpVia", HvdcpTransport::UsbinRh.code());
    let mut bus = Bus::Usbin(&mut rh);
    negotiate_on_bus(device, &mut bus, vbat_uv, state, read_vin)
}

/// IOCTL / autostart entry: fills a status snapshot.
///
/// # Safety
///
/// PASSIVE_LEVEL; `device` is valid.
pub unsafe fn run_negotiate_report(
    device: WDFDEVICE,
    usbin_id: Option<u64>,
    vbat_uv: u32,
    state: &mut HvdcpState,
    read_vin: &mut impl FnMut() -> i32,
) -> (i32, u32) {
    match unsafe { run_negotiate(device, usbin_id, vbat_uv, state, read_vin) } {
        Ok(()) => (0, target_vbus_uv(vbat_uv)),
        Err(err) => (err.code(), target_vbus_uv(vbat_uv)),
    }
}

/// Lower QC3 VBUS with DEC pulses until `vin_uv` is at/under [`target_vbus_uv`].
///
/// Live nabu: 20× INC can overshoot to ~12 V and latch LN8000 `VIN_OV`, which
/// blocks 2:1. Trim before `set_charging`. Returns how many DEC pulses were sent.
///
/// The stop point is [`target_vbus_uv`] (band centre), not the band top: the
/// band is only 200 mV wide and one pulse is 200 mV, so stopping at the top
/// would land a single pulse *above* it — where the pump carries nothing.
///
/// # Safety
///
/// PASSIVE_LEVEL; `device` is valid.
pub unsafe fn trim_vin_for_pump(
    device: WDFDEVICE,
    usbin_id: Option<u64>,
    state: &mut HvdcpState,
    mut vin_uv: i32,
    vbat_uv: u32,
) -> u32 {
    let stop = target_vbus_uv(vbat_uv) as i32;
    let floor = window_floor_uv(vbat_uv);
    if vin_uv <= stop {
        mark(device, "TrimDec", 0);
        return 0;
    }
    let mut dec = 0_u32;
    // Prefer SUPERUSER (same as negotiate).
    if let Ok(mut su) = unsafe { SuperuserBus::open(device) } {
        state.transport = HvdcpTransport::Superuser;
        let mut bus = Bus::Superuser(&mut su);
        while vin_uv > stop && vin_uv > floor && dec < MAX_TRIM_DEC {
            if pulse_dec(&mut bus, state).is_err() {
                break;
            }
            dec = dec.saturating_add(1);
            // Approximate: one soft step; caller re-reads ADC between batches.
            vin_uv = vin_uv.saturating_sub(QC3_STEP_UV as i32);
        }
        mark(device, "TrimDec", dec);
        return dec;
    }
    if let Some(id) = usbin_id {
        if let Ok(mut rh) = unsafe { open_usbin(device, id) } {
            let mut bus = Bus::Usbin(&mut rh);
            while vin_uv > stop && vin_uv > floor && dec < MAX_TRIM_DEC {
                if pulse_dec(&mut bus, state).is_err() {
                    break;
                }
                dec = dec.saturating_add(1);
                vin_uv = vin_uv.saturating_sub(QC3_STEP_UV as i32);
            }
        }
    }
    mark(device, "TrimDec", dec);
    dec
}

/// Raise QC3 VBUS with INC pulses until `vin_uv` reaches [`target_vbus_uv`].
///
/// The target is the `2*Vbat`-derived band centre, so the pulse budget shrinks
/// with the pack: a full pack needs 21 steps (4.2 V) instead of the 23 that a
/// fixed 9.5 V floor always cost. The loop never pulses past the band top.
///
/// # Safety
///
/// PASSIVE_LEVEL; `device` is valid.
pub unsafe fn boost_vin_for_pump(
    device: WDFDEVICE,
    usbin_id: Option<u64>,
    state: &mut HvdcpState,
    mut vin_uv: i32,
    vbat_uv: u32,
) -> u32 {
    let stop = target_vbus_uv(vbat_uv) as i32;
    let top = trim_target_uv(vbat_uv);
    if vin_uv >= stop {
        mark(device, "BoostInc", 0);
        return 0;
    }
    let mut inc = 0_u32;
    if let Ok(mut su) = unsafe { SuperuserBus::open(device) } {
        state.transport = HvdcpTransport::Superuser;
        let mut bus = Bus::Superuser(&mut su);
        while vin_uv < stop && inc < MAX_BOOST_INC {
            if pulse_inc(&mut bus, state).is_err() {
                break;
            }
            inc = inc.saturating_add(1);
            vin_uv = vin_uv.saturating_add(QC3_STEP_UV as i32);
            if vin_uv > top {
                break;
            }
        }
        mark(device, "BoostInc", inc);
        return inc;
    }
    if let Some(id) = usbin_id {
        if let Ok(mut rh) = unsafe { open_usbin(device, id) } {
            let mut bus = Bus::Usbin(&mut rh);
            while vin_uv < stop && inc < MAX_BOOST_INC {
                if pulse_inc(&mut bus, state).is_err() {
                    break;
                }
                inc = inc.saturating_add(1);
                vin_uv = vin_uv.saturating_add(QC3_STEP_UV as i32);
                if vin_uv > top {
                    break;
                }
            }
        }
    }
    mark(device, "BoostInc", inc);
    inc
}

/// One closed-loop bus correction toward the transfer band (µV moved, ±).
///
/// Three cases, in order:
///
/// * `Vin` **above** the band → DEC back to the centre ([`trim_vin_for_pump`]).
/// * `Vin` **below** the centre → INC, but only when the bus is below the band
///   floor or the pump is carrying nothing: from *inside* the band a pulse of
///   200 mV would leave it upward, which is the one direction that stops the
///   transfer.
/// * **Inside** the band yet `dead_current` — the pump reports mode 3 on the
///   39 mA ADC floor (live 18.09: bus 9.744 V, window 8.94–9.09 V, all three
///   loop configurations at exactly 39 mA) → one DEC step down, because the
///   band the policy computed is not the band silicon is using.
///
/// Returns the number of pulses sent, and marks `NudgeInc` / `NudgeDec` for the
/// post-mortem.
///
/// # Safety
///
/// PASSIVE_LEVEL; `device` is valid.
pub unsafe fn nudge_vin_into_window(
    device: WDFDEVICE,
    usbin_id: Option<u64>,
    state: &mut HvdcpState,
    vin_uv: i32,
    vbat_uv: u32,
    dead_current: bool,
) -> u32 {
    if vin_uv <= 0 || vbat_uv == 0 {
        return 0;
    }
    // A latched `FORCE_9V` runs outside the band: QC2 gives a fixed level and has
    // no continuous mode, and a pulse would drop the latch (see `pulse_cmd_bit`).
    // The caller decides the mode from the live ADC.
    if state.force9v_latched {
        return 0;
    }
    let top = trim_target_uv(vbat_uv);
    let floor = window_floor_uv(vbat_uv);
    let target = target_vbus_uv(vbat_uv) as i32;
    if vin_uv > top {
        let dec = unsafe { trim_vin_for_pump(device, usbin_id, state, vin_uv, vbat_uv) };
        mark(device, "NudgeDec", dec);
        return dec;
    }
    if vin_uv < target && (dead_current || vin_uv < floor) {
        let inc = unsafe { boost_vin_for_pump(device, usbin_id, state, vin_uv, vbat_uv) };
        mark(device, "NudgeInc", inc);
        return inc;
    }
    if dead_current && vin_uv > floor {
        let dec = unsafe { one_dec_pulse(device, usbin_id, state) };
        mark(device, "NudgeDec", dec);
        return dec;
    }
    0
}

/// Single DEC pulse on whichever transport opens (`1` sent, `0` otherwise).
///
/// # Safety
///
/// PASSIVE_LEVEL; `device` is valid.
unsafe fn one_dec_pulse(
    device: WDFDEVICE,
    usbin_id: Option<u64>,
    state: &mut HvdcpState,
) -> u32 {
    if let Ok(mut su) = unsafe { SuperuserBus::open(device) } {
        state.transport = HvdcpTransport::Superuser;
        let mut bus = Bus::Superuser(&mut su);
        return u32::from(pulse_dec(&mut bus, state).is_ok());
    }
    if let Some(id) = usbin_id {
        if let Ok(mut rh) = unsafe { open_usbin(device, id) } {
            let mut bus = Bus::Usbin(&mut rh);
            return u32::from(pulse_dec(&mut bus, state).is_ok());
        }
    }
    0
}

/// Raise USBIN ICL to the pump budget ([`ICL_RAW_PUMP_3A`]) after Vin elevate.
///
/// # Safety
///
/// PASSIVE_LEVEL; `device` is valid.
pub unsafe fn raise_icl_for_pump(
    device: WDFDEVICE,
    usbin_id: Option<u64>,
    state: &mut HvdcpState,
) -> u8 {
    if let Ok(mut su) = unsafe { SuperuserBus::open(device) } {
        state.transport = HvdcpTransport::Superuser;
        let mut bus = Bus::Superuser(&mut su);
        match ensure_icl_for_pump(&mut bus, state) {
            Ok(icl) => {
                mark(device, "SuIclPump", u32::from(icl));
                return icl;
            }
            Err(err) => mark(device, "SuIclPump", err.code() as u32),
        }
    }
    if let Some(id) = usbin_id {
        if let Ok(mut rh) = unsafe { open_usbin(device, id) } {
            let mut bus = Bus::Usbin(&mut rh);
            if let Ok(icl) = ensure_icl_for_pump(&mut bus, state) {
                mark(device, "SuIclPump", u32::from(icl));
                return icl;
            }
        }
    }
    0
}

/// Raise USBIN ICL for 5 V high-current bypass (NABU ≈ 2.7 A).
///
/// Retreat path: `FORCE_9V` / QC pulses left Vin near 5 V (plain DCP / QC2 brick
/// without elevate). Marks `SuApsdAfc5v=1` (legacy name; means "landed in the
/// 5 V bypass as a retreat") and `SuAfc5vPath=1`; AFC protocol itself remains
/// unsupported (`SuAfcProto=0`).
///
/// # Safety
///
/// PASSIVE_LEVEL; `device` is valid.
pub unsafe fn raise_icl_for_5v_bypass(
    device: WDFDEVICE,
    usbin_id: Option<u64>,
    state: &mut HvdcpState,
) -> u8 {
    mark(device, "SuAfcProto", 0);
    mark(device, "SuAfc5vPath", 1);
    mark(device, "SuApsdAfc5v", 1);
    state.phase = HvdcpPhase::FiveVBypass;
    mark(device, "HvdcpPhase", state.phase.code());

    if let Ok(mut su) = unsafe { SuperuserBus::open(device) } {
        state.transport = HvdcpTransport::Superuser;
        let mut bus = Bus::Superuser(&mut su);
        match ensure_icl_at_least(&mut bus, state, ICL_RAW_5V_2P7A) {
            Ok(icl) => {
                mark(device, "SuIcl5v", u32::from(icl));
                return icl;
            }
            Err(err) => mark(device, "SuIcl5v", err.code() as u32),
        }
    }
    if let Some(id) = usbin_id {
        if let Ok(mut rh) = unsafe { open_usbin(device, id) } {
            let mut bus = Bus::Usbin(&mut rh);
            if let Ok(icl) = ensure_icl_at_least(&mut bus, state, ICL_RAW_5V_2P7A) {
                mark(device, "SuIcl5v", u32::from(icl));
                return icl;
            }
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use ln8000::encoding::{charge_mode, OpMode};

    #[test]
    fn target_tracks_the_live_transfer_band() {
        // The target is the centre of the transfer band `[2*Vbat+200, 2*Vbat+400]` mV,
        // and it rides with the pack instead of the fixed 9.5 V, which with a full
        // pack sits above the band (live measurement on 18.09: 39 mA at 9.744 and 9.888 V).
        assert_eq!(target_vbus_uv(4_000_000), 8_300_000);
        assert_eq!(target_vbus_uv(4_420_000), 9_140_000);
        assert_eq!(target_vbus_uv(4_500_000), 9_300_000);
        // Low pack: the target is raised to the absolute 2:1 admission floor.
        assert_eq!(target_vbus_uv(3_000_000), PUMP_VIN_TARGET_ABS_MIN_UV);
        // Pack not read - the vendor's 9.5 V floor.
        assert_eq!(target_vbus_uv(0), PUMP_VIN_TARGET_MIN_UV as u32);
    }

    #[test]
    fn target_always_admits_switching() {
        // The invariant the old `2*Vbat + 200 mV` form violated: the target must pass
        // the 2:1 admission gate (`2*Vbat + 250 mV`).
        for vbat in (3_000_000..=4_500_000).step_by(50_000) {
            let vin = i32::try_from(target_vbus_uv(vbat)).unwrap_or(i32::MIN);
            assert_eq!(
                charge_mode(vin, vbat),
                Some(OpMode::Switching),
                "target {vin} must admit 2:1 at Vbat {vbat}"
            );
        }
    }

    #[test]
    fn inc_cap_matches_boost_budget() {
        // Boost never issues more INC pulses than the soft counter can record,
        // and the CP cap still reaches the window floor from the 5 V baseline.
        assert!(MAX_BOOST_INC <= MAX_PULSE_CNT);
        assert_eq!(MAX_PULSE_CNT, 23); // cp_qc30.h:81 MAX_PLUSE_COUNT_ALLOWED
        assert!(estimated_vbus_uv(MAX_PULSE_CNT) >= PUMP_VIN_TARGET_MIN_UV as u32);
        // The pulse budget now depends on the pack: a full pack is cheaper.
        assert_eq!(pulses_toward_target(4_000_000), 17);
        assert!(pulses_toward_target(4_500_000) <= MAX_PULSE_CNT);
    }

    #[test]
    fn estimated_vbus_from_pulses() {
        assert_eq!(estimated_vbus_uv(0), MICRO_5V_UV);
        assert_eq!(estimated_vbus_uv(20), 9_000_000);
    }

    #[test]
    fn usbin_unavailable_code_is_stable() {
        assert_eq!(HvdcpError::UsbinUnavailable.code(), -10);
    }

    #[test]
    fn transport_codes() {
        assert_eq!(HvdcpTransport::Superuser.code(), 1);
        assert_eq!(HvdcpTransport::UsbinRh.code(), 2);
    }

    #[test]
    fn force_5v_matches_smb5_bit3() {
        assert_eq!(BIT_FORCE_5V, 1 << 3);
        assert_ne!(BIT_FORCE_5V, 1 << 2); // IDLE_BIT — must not confuse
    }

    #[test]
    fn icl_500ma_raw_is_ten_steps() {
        assert_eq!(u32::from(ICL_RAW_500MA) * 50_000, 500_000);
    }

    #[test]
    fn icl_pump_3a_raw_is_sixty_steps() {
        assert_eq!(u32::from(ICL_RAW_PUMP_3A) * 50_000, 3_000_000);
    }

    #[test]
    fn icl_5v_2p7a_raw_is_fifty_four_steps() {
        assert_eq!(u32::from(ICL_RAW_5V_2P7A) * 50_000, 2_700_000);
    }

    #[test]
    fn icl_5v_bypass_matches_nabu_dcp_band() {
        assert_eq!(u32::from(ICL_RAW_DCP_1P8A) * 50_000, 1_800_000);
        assert!(
            ICL_RAW_5V_2P7A >= ICL_RAW_DCP_1P8A,
            "5 V bypass ICL must be >= nabu DCP 1.8 A floor"
        );
        assert!(
            ICL_RAW_5V_2P7A <= ICL_RAW_PUMP_3A,
            "5 V bypass ICL must stay within LN8000 class-B bus budget"
        );
    }

    #[test]
    fn pump_vin_window_is_ordered() {
        assert!(PUMP_VIN_TARGET_ABS_MIN_UV <= PUMP_VIN_TARGET_CEIL_UV);
        assert!(PUMP_VIN_TARGET_CEIL_UV < PUMP_VIN_TRIM_UV as u32);
        assert!(window_floor_uv(4_420_000) < trim_target_uv(4_420_000));
        assert!(trim_target_uv(4_420_000) as u32 <= PUMP_VIN_TRIM_UV as u32);
        // Without a pack reading the bounds do not collapse to zero.
        assert_eq!(window_floor_uv(0), PUMP_VIN_TARGET_MIN_UV);
        assert_eq!(trim_target_uv(0), PUMP_VIN_TARGET_MIN_UV);
    }

    #[test]
    fn qc2_max_9v_merges_without_clobbering_low_bits() {
        assert_eq!(with_qc2_max_9v(0x00), 0x40);
        assert_eq!(with_qc2_max_9v(0x1F), 0x5F);
        assert_eq!(with_qc2_max_9v(0xC0), 0x40); // was 12V field → 9V
    }

    #[test]
    fn adapter_allow_5v_to_12v_merges_low_nibble() {
        assert_eq!(with_adapter_allow_5v_to_12v(0x00), 0x0C);
        assert_eq!(with_adapter_allow_5v_to_12v(0xF0), 0xFC);
    }

    #[test]
    fn qc_status_9v_hint_bits() {
        assert!(!qc_status_indicates_9v(0x01)); // QC_5V only
        assert!(qc_status_indicates_9v(BIT_QC_9V));
        assert!(qc_status_indicates_9v(BIT_QC_5V_TO_9V_REASON));
    }

    #[test]
    fn qc35_power_windows_match_android() {
        assert_eq!(qc35_power_limit_w(7_000_000), Some(18));
        assert_eq!(qc35_power_limit_w(8_000_000), Some(27));
        assert_eq!(qc35_power_limit_w(9_000_000), Some(40));
        assert_eq!(qc35_power_limit_w(6_000_000), None);
        assert_eq!(qc35_power_limit_w(7_500_000), None); // gap between 18W and 27W
    }

    #[test]
    fn qc35_detect_and_cap_windows() {
        assert!(qc35_detect_window(6_000_000));
        assert!(!qc35_detect_window(5_000_000));
        assert!(qc35_cap_window(7_000_000));
        assert!(!qc35_cap_window(6_000_000));
    }

    #[test]
    fn qc35_icl_by_class() {
        assert_eq!(qc35_icl_raw(18), ICL_RAW_QC35_2A);
        assert_eq!(qc35_icl_raw(27), ICL_RAW_QC35_2A);
        assert_eq!(qc35_icl_raw(40), ICL_RAW_QC35_40W);
        assert_eq!(ICL_RAW_QC35_40W, ICL_RAW_PUMP_3A);
    }

    #[test]
    fn five_v_stay_helper() {
        assert!(vin_stayed_near_5v(5_100_000));
        assert!(!vin_stayed_near_5v(9_600_000));
        assert!(!vin_stayed_near_5v(0));
    }

    #[test]
    fn qc35_step_is_one_tenth_of_qc3() {
        assert_eq!(QC35_STEP_UV * 10, QC3_STEP_UV);
    }

    #[test]
    fn qc35_phase_codes() {
        assert_eq!(HvdcpPhase::Qc35Auth.code(), 8);
        assert_eq!(HvdcpPhase::FiveVBypass.code(), 9);
    }

    #[test]
    fn ta200_apsd_0x28_takes_qc2_force9v() {
        // 0x28 = DCP|QC_2P0 = HVDCP2 in the reference APSD table: FORCE_9V,
        // never the 5 V bypass (that is only the post-FORCE retreat).
        assert_eq!(apsd_elevate_path(0x28, false), ApsdElevate::Force9v);
        assert_eq!(
            apsd_elevate_path(BIT_QC2 | BIT_DCP, false),
            ApsdElevate::Force9v
        );
        // Pure QC2 / DCP must still take the elevate path.
        assert_eq!(apsd_elevate_path(BIT_QC2, false), ApsdElevate::Force9v);
        assert_eq!(apsd_elevate_path(BIT_DCP, false), ApsdElevate::Force9v);
        // QC3 / continuous keep the pulse path.
        assert_eq!(
            apsd_elevate_path(BIT_QC3 | BIT_DCP, false),
            ApsdElevate::Qc3Pulse
        );
        assert_eq!(apsd_elevate_path(BIT_DCP, true), ApsdElevate::Qc3Pulse);
        // SDP / CDP / unknown must be rejected.
        assert_eq!(apsd_elevate_path(0, false), ApsdElevate::Reject);
        assert_eq!(apsd_elevate_path(1 << 1, false), ApsdElevate::Reject);
    }
}
