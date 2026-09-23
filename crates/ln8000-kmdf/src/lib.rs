//! KMDF driver for the LN8000 charge pump (Xiaomi Pad 5, `nabu`), Windows on ARM64.
//!
//! # What the driver does
//!
//! 1. Attaches to the existing ACPI node `PEIC` (`_HID = QCOM057E`) - that is the
//!    external LN8000 IC on the I²C bus at address 0x51 (see
//!    `09-acpi-nabu/ln8000-acpi-uefi.md`).
//! 2. In `EvtDevicePrepareHardware` parses `_CRS`, takes the connection
//!    identifier and opens the bus through the Resource Hub: path
//!    `\Device\RESOURCE_HUB\<16 hex>` (rule from `reshub.h`).
//! 3. Checks `DEVICE_ID`, sets up thresholds and protection, enables 2:1 mode.
//! 4. The timer samples telemetry periodically (voltages, current, temperature),
//!    keeps a journal of charge sessions and applies temperature and current
//!    protection.
//! 5. Exposes state and journal to user mode through IOCTL.
//!
//! # Why ARM64
//!
//! The target device is a tablet with a Snapdragon 860; the build only targets
//! `aarch64-pc-windows-msvc` (see `.cargo/config.toml` next to this file).
//!
//! # Verification status
//!
//! The core logic ([`ln8000`]) is covered by host tests. The WDF glue is built
//! with `cargo wdk build`; on-tablet verification is described in
//! `docs/DEPLOY-LN8000.md`.

#![no_std]
#![deny(missing_docs)]
#![allow(clippy::missing_safety_doc)]

mod battery;
mod hvdcp;
mod ioctl;
mod sddl;
mod spb;
mod spb_abi;
mod sysreq;

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Stop code `MANUALLY_INITIATED_CRASH`: "the driver itself stopped the system".
///
/// We do not define our own value: this code has free-form parameters, and its
/// number reads unambiguously, unlike a home-made one.
const BUGCHECK_MANUALLY_INITIATED_CRASH: u32 = 0x0000_00E2;

/// Panic magic (`LN80`) - distinguishes our call from any other `0xE2`.
const PANIC_MAGIC: usize = 0x4C4E_3830;

unsafe extern "C" {
    /// Stops the system with a diagnosable code (`ntoskrnl`).
    fn KeBugCheckEx(
        bugcheck_code: u32,
        parameter1: usize,
        parameter2: usize,
        parameter3: usize,
        parameter4: usize,
    ) -> !;
}

/// FNV-1a of the file name: the stop parameters fit a number, not a string.
const fn file_hash(path: &str) -> usize {
    let bytes = path.as_bytes();
    let mut hash: u32 = 0x811C_9DC5;
    let mut index = 0;
    while index < bytes.len() {
        hash ^= bytes[index] as u32;
        hash = hash.wrapping_mul(0x0100_0193);
        index += 1;
    }
    hash as usize
}

/// Panic handler: an emergency stop instead of an infinite loop.
///
/// The stock `wdk-panic 0.4.1` spins in `loop {}` (its source even says so:
/// `FIXME: Should this trigger Bugcheck via KeBugCheckEx?`). A loop at the panic
/// site means a hung processor with no single clue and with locks held; the
/// 18.09 dumps had to be taken apart by RVA by hand. A stop leaves both the
/// module and the exact site: `Arg1` - the `LN80` magic, `Arg2` - line,
/// `Arg3` - column, `Arg4` - FNV-1a of the file name.
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    let (line, column, file_hash_value) = match info.location() {
        Some(location) => (
            location.line() as usize,
            location.column() as usize,
            file_hash(location.file()),
        ),
        None => (0, 0, 0),
    };
    // SAFETY: `KeBugCheckEx` may be called at any IRQL and does not return;
    // we do not print - at the panic site it may not go through.
    unsafe {
        KeBugCheckEx(
            BUGCHECK_MANUALLY_INITIATED_CRASH,
            PANIC_MAGIC,
            line,
            column,
            file_hash_value,
        )
    }
}

use ioctl::{
    Ln8000ChargeRequest, Ln8000HvdcpRequest, Ln8000LimitsRequest, Ln8000ModeRequest, Ln8000RegRequest,
    Ln8000Sample, Ln8000SamplesRequest, Ln8000Sessions, Ln8000Status, LN8000_STATUS_MAGIC,
    LN8000_STATUS_VERSION,
};
use ln8000::battery_policy;
use ln8000::encoding::decode_iin_limit;
use ln8000::{
    bypass_allowed_by_vin, bypass_strikes_expired, charge_mode, evaluate, regs, resolve_bypass,
    AdcChannel, AdcMode, BypassResolution, GuardAction, GuardLimits, OpMode, Pump, PumpConfig,
    PumpError, PumpState, Telemetry, TelemetrySample, SWITCHING_MIN_VIN_UV,
};
use spb::SpbBus;
use wdk::println;
use wdk_sys::{
    _WDF_EXECUTION_LEVEL::WdfExecutionLevelPassive,
    _WDF_IO_QUEUE_DISPATCH_TYPE::WdfIoQueueDispatchSequential,
    _WDF_REQUEST_SEND_OPTIONS_FLAGS::{
        WDF_REQUEST_SEND_OPTION_SYNCHRONOUS, WDF_REQUEST_SEND_OPTION_TIMEOUT,
    },
    _WDF_SYNCHRONIZATION_SCOPE::WdfSynchronizationScopeNone, _WDF_TRI_STATE::WdfTrue,
    call_unsafe_wdf_function_binding, CmResourceTypeConnection, NTSTATUS, PCUNICODE_STRING,
    PLUGPLAY_REGKEY_DEVICE, PWDFDEVICE_INIT, ULONG, UNICODE_STRING, WDFCMRESLIST, WDFDEVICE, WDFDRIVER,
    WDFIOTARGET, WDFKEY, WDFMEMORY, WDFQUEUE, WDFREQUEST, WDFTIMER, WDF_DRIVER_CONFIG,
    WDF_IO_QUEUE_CONFIG, WDF_NO_HANDLE, WDF_NO_OBJECT_ATTRIBUTES, WDF_OBJECT_ATTRIBUTES,
    WDF_PNPPOWER_EVENT_CALLBACKS, WDF_REQUEST_SEND_OPTIONS, WDF_TIMER_CONFIG,
};

/// Device interface for user mode.
///
/// `{B0A1C3D2-4E5F-4A6B-8C7D-9E0F1A2B3C4D}`
pub const GUID_DEVINTERFACE_LN8000: wdk_sys::GUID = wdk_sys::GUID {
    Data1: 0xB0A1_C3D2,
    Data2: 0x4E5F,
    Data3: 0x4A6B,
    Data4: [0x8C, 0x7D, 0x9E, 0x0F, 0x1A, 0x2B, 0x3C, 0x4D],
};

/// Lower bound of the telemetry period, ms (registry `TelemetryMs`).
const TELEMETRY_MS_MIN: u32 = 100;

/// Retry period for charge autostart, ms.
const CHARGE_RETRY_MS: u64 = 30_000;

/// Input change that triggers an immediate retry, µV.
const CHARGE_RETRY_DELTA_UV: u32 = 300_000;

/// Floor between "input changed" retries, ms.
///
/// [`CHARGE_RETRY_DELTA_UV`] alone does not limit the rate: on a five-volt
/// input `recover_5v_bypass` does a POR, the POR restarts input detection, and
/// the bus runs like a sawtooth in half a volt. Every tooth is "input changed"
/// and a new attempt. Live measurement 19.09 14:45 (MDY-08-EI after losing the
/// QC level): Vin 4,27↔4,83 V, `ChargeAttemptN` 198→346 in 3,5 min (1,6
/// attempts/s), `FAULT1 = 0x21`, current at the ADC floor. A five-second floor
/// keeps a fast retry meaningful (QC raised the bus - we re-decide at once),
/// but stops the storm: a POR is heavy, and the VFAULT latch returns after it
/// anyway until the input is raised.
const CHARGE_RETRY_FAST_MS: u64 = 5_000;

/// First delay of the HVDCP re-elevation, ms.
///
/// Negotiation only happens on autostart and on the input edge
/// ([`hvdcp::should_renegotiate_on_input_edge`]), so a lost QC level does not
/// come back by itself: the brick fell back to 5 V, the 1:1 bypass is closed by
/// the latch - and the bus sits at five volts until the cable is re-plugged.
/// Live measurement 19.09 14:45: `ac=1`, `chg=0`, mode 1, 39 mA,
/// `LastEnableErr = -4`, `FAULT1 = 0x21`. Here we repeat what re-plugging does,
/// without waiting for the operator.
const RE_ELEVATE_FIRST_MS: u64 = 60_000;

/// Limit of the re-elevation delay, ms.
///
/// On an honest five-volt source (DCP charger, port without QC) elevation is
/// useless, and it costs SPMI transactions and an input detection reset, so
/// every failure doubles the delay up to this limit.
const RE_ELEVATE_BACKOFF_MAX_MS: u64 = 300_000;

/// Cell level above which re-elevation is not needed, µV.
///
/// At the top of the charge the current is already limited by the taper, and an
/// extra input reset on a full cell only hinders top-off charging.
const RE_ELEVATE_MAX_VBAT_UV: u32 = 4_350_000;

/// Step of the FCC field of the SMB5 PM8150B buck, µA per bit.
///
/// `.step_u` of the PM8150B parameter in the vendor header
/// (`drivers_power_supply_qcom_qpnp-smb5.c:128-134`: `min_u = 0`,
/// `max_u = 8 000 000`, `step_u = 50 000`); the register is there too:
/// `CHGR_FAST_CHARGE_CURRENT_CFG_REG`. A live measurement on 19.09 gave
/// `ChgrFccRaw = 30`, that is 1,50 A, - and that is the factory buck limit on
/// our tablet.
const FCC_STEP_UA: u32 = 50_000;

/// Name of the registry parameter that enables the buck FCC policy: the raw
/// value of the FCC field of register `0x1061` (50 mA per bit) that the driver
/// sets while the pump is carrying current.
///
/// No parameter or a zero value means no policy at all: the register is never
/// touched during a boot. This is the default and at the same time the
/// behaviour of build `.652`, from which the baseline measurement was taken.
/// A value `N > 0` enables raising up to the raw `N`; the ceiling
/// [`FCC_WRITE_MAX_UA`] is applied by [`raw_for`], and there is no other way
/// around it.
///
/// This knob appeared not out of caution but from a live measurement on 19.09
/// that overturned the original assumption. Build `.655` raised the buck limit
/// to 2,5 A (`FccRaw = 50`) on the grounds that the factory 1,5 A (`ChgrFccRaw =
/// 30`, `FgIbatUa` ≈ 2,9 A, `SysStsRaw = 0x04`) choke the transfer, - and the
/// cell charge rate fell by half: 0,409 %/min on `.652` without the raise
/// against 0,249 %/min on `.655`, and the share of ticks in 2:1 mode - from
/// 85 % (34 samples out of 40) to 39 % (13 out of 33). The mechanism is visible
/// in the same measurement: the transfer divides the adapter's eighteen watts
/// between the pump and the buck, and the 2,5 A allowed to the buck drain that
/// budget - the adapter loses the raised QC3 level (`FAULT1 = 0x21` latches,
/// `SuMode` drops to 1, bus 4,5 V), after which the pump falls back to the
/// five-volt path, where the cell gets about 0,7 A. The limit itself works:
/// while the pump holds 2:1, the current into the cell reaches −2,5…−3,2 A,
/// that is it is not the limit that hurts but the exhausted budget of a
/// particular brick, and on another adapter the answer may be the opposite.
/// This is only verified by A/B on hardware, and that must not cost a rebuild:
/// so the value became a parameter. The upper bound of the tablet's vendor DT
/// is 5,9 A (`qcom,fcc-max-ua = <5900000>`,
/// `arch/arm64/boot/dts/qcom/xiaomi/overlay/nabu/nabu-sm8150.dtsi:68`), but the
/// driver has no trustworthy cell temperature channel, so the write ceiling is
/// left lower - at [`FCC_WRITE_MAX_UA`].
const FCC_POLICY_VALUE_NAME: &str = "FccRaw";

/// Setpoint of the one-shot buck FCC write probe, µA.
///
/// The probe is needed because the CHGR write path has never been executed even
/// once: the live measurement on 19.09 - 92 % cell, 4,375 V, `ChgrFccRaw = 30`
/// - lies above the policy gate, and the pump is not carrying current, so not a
/// single raise condition holds and the question "does the CHGR peripheral
/// accept a write at all" stays unanswered. It cannot be resolved from the
/// outside: `\Device\Spmi\SUPERUSER` is a kernel-level name, it has no user
/// symbolic link, and opening it from user mode returns
/// `STATUS_OBJECT_PATH_NOT_FOUND`; the driver is the only path to the bus, so
/// the probe lives in the driver too.
///
/// 1,00 A (raw 20) is strictly below the factory value on this board (30, that
/// is 1,50 A), and this is not caution but a requirement on the probe: an error
/// downwards is safe (such a setpoint cannot raise the current into the cell),
/// upwards it is not. At 92 % the buck is driven by the taper and delivers far
/// less than an ampere, so for the fractions of a second while the setpoint
/// stands it does not limit the transfer; the factory value returns in the same
/// tick.
const FCC_PROBE_UA: u32 = 1_000_000;

/// Grace before the FCC returns to its original value after the pump stops, ms.
///
/// The pump goes out on QC3 loop ticks (bus step, taper, mode failure), and an
/// instant reaction to every such tick would mean an SPMI write several times a
/// minute. Two minutes hold the raised limit through short transfer gaps and
/// still return the original sooner than an operator could take the journal.
const FCC_RESTORE_GRACE_MS: u64 = 120_000;

/// FCC write ceiling: above it the driver does not write under any conditions, µA.
///
/// Below the vendor `qcom,fcc-max-ua` (5,9 A) with margin. 3 A is sixty steps
/// of the 8-bit field; beyond that begins the region where the cost of a mistake
/// in the step is higher than the gain from a lifted ceiling.
const FCC_WRITE_MAX_UA: u32 = 3_000_000;

/// Raw FCC value for a setpoint in µA.
///
/// The only place that knows the encoding of register `0x1061` - both for the
/// raise and for the return: the original byte is converted back to microamps
/// and passes through this function again, so the ceiling [`FCC_WRITE_MAX_UA`]
/// cannot be bypassed another way. The division saturates: a setpoint above the
/// field must hit the ceiling rather than wrap modulo 256 - a wrapped value
/// would write **less** current into the chip than intended.
const fn raw_for(ua: u32) -> u8 {
    let raw = ua / FCC_STEP_UA;
    let cap = FCC_WRITE_MAX_UA / FCC_STEP_UA;
    if raw > cap {
        cap as u8
    } else {
        raw as u8
    }
}

/// Minimum bus deviation from the target counted as progress, µV.
///
/// Below the QC3 step (200 mV) with margin: 50 mV separates a real shift from
/// ADC jitter, but does not require guessing the real pulse slope.
const WINDOW_PROGRESS_UV: u32 = 50_000;

/// How many corrections without progress the window loop tolerates before a rare repeat.
const WINDOW_STALL_MAX: u32 = 4;

/// Bus correction interval during a stall, ms.
///
/// A stall means the QC3 pulses do not change the input (non-QC3 adapter) or
/// the brick step is finer than expected. A rare repeat keeps the attempt but
/// removes the constant load on SPMI.
const WINDOW_STALL_MS: u64 = 60_000;

/// Observation window of the input current peak for the `MaxIinUa` mark, ms.
const IIN_WINDOW_MS: u64 = 5_000;
/// Poll period of the PM8150B fuel counter, ms.
///
/// The counter is a slow quantity: one raw step is ~0,4 % of capacity, and one
/// SUPERUSER transaction on this platform takes about two seconds. Polling every
/// 250 ms (build `.627`) occupied the bus almost constantly: a third-party
/// user-mode reader got `ERROR_GEN_FAILURE` in 79 attempts out of 100
/// (measurement 18.09), and acceptance was left without `0x1307`/`0x1506`.
/// Once every 30 s the bus is free ~94 % of the time, and the charge indicator
/// is more than satisfied with that.
const GAUGE_POLL_MS: u32 = 30_000;

// Short numeric codes of the `EngageState` mark - the state "is there input and mode".
/// No input (`Vin` below the presence threshold).
const ENGAGE_NO_INPUT: u32 = 0;
/// Input present, no charge/mode (standby).
const ENGAGE_STANDBY: u32 = 1;
/// 1:1 bypass enabled.
const ENGAGE_BYPASS: u32 = 2;
/// 2:1 mode enabled.
const ENGAGE_SWITCHING: u32 = 3;
/// Input is elevated, but 2:1 physically cannot carry it
/// (`Vin < 2*Vbat + 250 mV`), and bypass at such a voltage is forbidden.
const ENGAGE_NO_HEADROOM: u32 = 4;

/// How many consecutive failures count as a latched chip state.
///
/// The chip latches a failure if a mode is requested with an invalid input, and
/// after that it fails even with a normal input. The stock way out is a software
/// reset and reconfiguration, as the reference driver does on loss of
/// communication.
const CHARGE_FAILS_BEFORE_RESET: u32 = 3;

/// `ProfSel` mark value when there is no `ProtectionProfile` parameter in the
/// registry.
const PROFILE_NOT_SET: u32 = u32::MAX;

/// Driver state: the single device instance.
///
/// # Invariants
///
/// The fields are touched either by the sequential control-request queue or by
/// the telemetry timer; WDF serializes them against each other, so there is no
/// concurrent access. That is exactly why `Sync` is declared by hand.
struct DriverState {
    pump: Option<Pump<SpbBus>>,
    telemetry: Telemetry,
    limits: GuardLimits,
    writes: u32,
    reads: u32,
    last_error: i32,
    actions: u32,
    /// Telemetry period from the registry, ms.
    telemetry_ms: u32,
    /// Time of the last charge autostart attempt, ms.
    last_charge_attempt_ms: u64,
    /// Input voltage at the last attempt, µV.
    last_attempt_vbus_uv: u32,
    /// How many times autostart enabled the charge.
    auto_starts: u32,
    /// Consecutive charge failures: counted to detect a latched state.
    failed_attempts: u32,
    /// Total number of attempts to enable the charge (`ChargeAttemptN`).
    charge_attempts: u32,
    /// Monotonic time of the last attempt to enable the charge (`LastEnableMs`).
    last_enable_ms: u64,
    /// Input current peak over the current observation window, µA (`MaxIinUa`).
    max_iin_ua: u32,
    /// Start of the current current-peak observation window, ms.
    iin_window_start_ms: u64,
    /// How many ticks in a row protection asks for 1:1 while Vin does not allow it.
    ///
    /// Needed so that at elevated Vin standby is not spun every tick: the first
    /// tick reduces the current, from the next one
    /// (`BYPASS_DENIED_STRIKES_BEFORE_STOP`) - stop.
    bypass_denied_strikes: u32,
    /// Usbin SPMI connection id from `_CRS` (`None` on stock ACPI; HVDCP uses SUPERUSER).
    usbin_id: Option<u64>,
    /// Soft HVDCP / QC pulse state (software `pulse_cnt`).
    hvdcp: hvdcp::HvdcpState,
    /// SUPERUSER open failed at PrepareHardware - retry from telemetry.
    hvdcp_retry_pending: bool,
    /// How many SUPERUSER retries have been attempted.
    hvdcp_retry_attempts: u32,
    /// Monotonic deadline (ms) for the next SUPERUSER retry.
    hvdcp_retry_next_ms: u64,
    /// Last cable-present sample (Vin > unplug floor) for re-plug edge detect.
    last_input_present: bool,
    /// The tick has taken the ADC out of `AutoHibernate` ([`AdcMode::Normal`]).
    ///
    /// Set when a tick saw unusable samples while the hardware said VBUS was
    /// present ([`battery_policy::adc_wake_needed`]), cleared when the comparator
    /// said VBUS was gone and the chip was put back to sleep. Without it the tick
    /// would write `ADC_CTRL` on every such tick, and the mode it left behind
    /// would be invisible in a dump.
    adc_awake: bool,
    /// How many times the ADC was woken by the comparator, ever.
    ///
    /// Evidence for the field: a sleeping ADC is the reason an insertion can be
    /// invisible, so a dump has to show whether the wake path ever ran. Zero here
    /// with `AdcValid` bit 0 set in every dump means the comparator never told the
    /// tick that a source was there.
    adc_wake_n: u32,
    /// Edge detect armed after PrepareHardware autostart (avoids double-negotiate).
    hvdcp_edge_armed: bool,
    /// Monotonic time of the last bus correction towards the 2:1 window, ms.
    ///
    /// The window follows the cell (band `[2*Vbat+200, 2*Vbat+400]` mV), so
    /// without periodic correction the bus stays where negotiation left it: as
    /// Vbat rises it goes above the band, mode 3 is retained, and the transfer
    /// drops to 39 mA. The interval is [`hvdcp::WINDOW_NUDGE_MS`], so as not to
    /// load SPMI on every tick.
    last_window_nudge_ms: u64,
    /// Best (smallest) bus deviation from the target in the current episode, µV.
    ///
    /// `u32::MAX` - episode not started (bus inside the band). Needed to tell a
    /// correction that works from a stall: with a PD adapter the QC3 pulses
    /// change nothing, and the loop must switch to a rare repeat instead of
    /// pouring pulses into SPMI forever.
    window_best_err_uv: u32,
    /// Consecutive corrections without progress in the current episode.
    window_stall_n: u32,
    /// How many times HVDCP elevation was repeated without re-plugging.
    re_elevate_attempts: u32,
    /// Monotonic deadline of the next re-elevation, ms (0 - not scheduled).
    re_elevate_next_ms: u64,
    /// Current re-elevation delay, ms.
    re_elevate_backoff_ms: u64,
    /// Monotonic time of the last publication of the PM8150B registers (`Chgr*`), ms.
    ///
    /// Zero - the snapshot has not been published yet. The mark is also set when
    /// the read fails (`ChgrErr=1`), otherwise an unavailable peripheral would
    /// pull the bus on every telemetry tick. The period is [`GAUGE_POLL_MS`]: the
    /// snapshot goes over the same SPMI bus and through the same SUPERUSER slot
    /// as the counter poll.
    last_chgr_mark_ms: u64,
    /// Raw FCC from the [`FCC_POLICY_VALUE_NAME`] parameter - what the policy
    /// raises the buck limit to while the pump is carrying current. Zero - the
    /// policy is off, and then nobody touches register `0x1061`.
    ///
    /// Read once at device start ([`read_parameters`]), because this is an A/B
    /// knob between boots, not a setpoint for on-the-fly changes; the mark
    /// `FccCfgRaw` publishes the applied value so that the mode is visible from
    /// the tablet side. The capture of [`Self::fcc_boot_raw`] does not depend on
    /// this knob and always runs - it is needed on its own too, as diagnostics
    /// (`ChgrFccRaw`). Hence the well-known pitfall: if the previous boot left
    /// the register raised, the next one captures the raised value as
    /// "factory", and there is nowhere to return the buck to. This can only be
    /// seen from the pair of marks - `FccCfgRaw` (what was asked) and
    /// `ChgrFccRaw` (what is really there) - and that is why both are always
    /// published, including zero.
    fcc_cfg_raw: u32,
    /// Buck FCC that stood in `0x1061` before our first write, 50 mA per bit.
    ///
    /// Zero - the value has not been captured yet: a live measurement on 19.09
    /// gave 30 (1,50 A), so a real zero in the register cannot play this role,
    /// but a zero in this role means "there is nothing to return" - and then the
    /// limit must not be raised (there would be nothing to return the buck to),
    /// and certainly a zero must not be returned, which would forbid charging
    /// the buck altogether.
    fcc_boot_raw: u8,
    /// Whether the buck FCC has been raised by the policy (`FccRaw`): only then
    /// does [`Self::fcc_boot_raw`] return through [`FCC_RESTORE_GRACE_MS`].
    fcc_raised: bool,
    /// Monotonic deadline for the FCC to return to [`Self::fcc_boot_raw`], ms.
    ///
    /// Zero - no return is scheduled. The deadline is set on the tick when the
    /// transfer disappeared and cleared if the pump picks up the current again:
    /// short transfer gaps must not pull the register back and forth.
    fcc_restore_at_ms: u64,
    /// Whether the one-shot FCC write probe has been performed ([`FCC_PROBE_UA`]).
    ///
    /// Once per driver boot: the probe checks whether the CHGR peripheral accepts
    /// a write, and that answer does not change during a boot. The flag is set
    /// on any outcome - both on a write failure and when the factory value is
    /// already not above the probe: there is no point repeating a silent failure
    /// every tick, and the outcome stays in the journal (`ChgrProbeOk`). It is
    /// only not set when the probe could not run at all: a broken SPMI snapshot,
    /// a critical chip failure, or a snapshot that never succeeded
    /// (`fcc_boot_raw == 0`).
    fcc_probe_done: bool,
}

// SAFETY: see the invariants above - access is serialized by WDF.
unsafe impl Sync for DriverState {}

impl DriverState {
    const fn new() -> Self {
        Self {
            pump: None,
            telemetry: Telemetry::new(),
            limits: GuardLimits::standard(),
            writes: 0,
            reads: 0,
            last_error: 0,
            actions: 0,
            telemetry_ms: 250,
            last_charge_attempt_ms: 0,
            last_attempt_vbus_uv: 0,
            auto_starts: 0,
            failed_attempts: 0,
            charge_attempts: 0,
            last_enable_ms: 0,
            max_iin_ua: 0,
            iin_window_start_ms: 0,
            bypass_denied_strikes: 0,
            usbin_id: None,
            hvdcp: hvdcp::HvdcpState::new(),
            hvdcp_retry_pending: false,
            hvdcp_retry_attempts: 0,
            hvdcp_retry_next_ms: 0,
            last_input_present: false,
            adc_awake: false,
            adc_wake_n: 0,
            hvdcp_edge_armed: false,
            last_window_nudge_ms: 0,
            window_best_err_uv: u32::MAX,
            window_stall_n: 0,
            re_elevate_attempts: 0,
            re_elevate_next_ms: 0,
            re_elevate_backoff_ms: 0,
            last_chgr_mark_ms: 0,
            fcc_cfg_raw: 0,
            fcc_boot_raw: 0,
            fcc_raised: false,
            fcc_restore_at_ms: 0,
            fcc_probe_done: false,
        }
    }
}

/// The single instance of the driver state.
static mut STATE: DriverState = DriverState::new();

/// Access to the driver state.
///
/// # Safety
///
/// Called only while the driver holds [`lock_state`]: the queue serializes the
/// IOCTLs, while the telemetry timer and `EvtDevicePrepareHardware` run in their
/// own contexts.
unsafe fn state() -> &'static mut DriverState {
    // SAFETY: see the invariants of `DriverState`.
    unsafe { &mut *core::ptr::addr_of_mut!(STATE) }
}

/// State mutex: the bus and `STATE` have one user at a time.
///
/// A KMUTEX, not a WDFWAITLOCK: the device callback execution level is not set
/// (`WdfDeviceInitSetExecutionLevel` is absent from the bindings), and a KMUTEX
/// is documented for both `PASSIVE_LEVEL` and `APC_LEVEL` - the driver calls are
/// certainly not higher, they do a synchronous WDF send.
static mut STATE_LOCK: wdk_sys::KMUTEX = unsafe { core::mem::zeroed() };

/// The mutex has been initialized. It must not be taken before initialization: a
/// zeroed dispatcher structure is not yet a waitable object.
static STATE_LOCK_READY: AtomicBool = AtomicBool::new(false);

/// Monotonic time of the last fuel counter poll, ms (0 - never).
///
/// A separate atomic rather than a [`DriverState`] field: the counter is read
/// **before** taking [`STATE_LOCK`] - a SUPERUSER transaction takes about two
/// seconds, and holding the mutex that long would slow down both charge control
/// and the `IOCTL_STATUS` the acceptance protocol uses. The compare-and-swap
/// also keeps two timer ticks from reading the bus at the same time.
static GAUGE_LAST_MS: AtomicU64 = AtomicU64::new(0);

/// The hardware is prepared: the counter may be polled.
///
/// `read_gauge_if_due` is called before taking [`STATE_LOCK`] and is therefore
/// not protected by the mutex against a race with `evt_release_hardware`; this
/// flag is that protection.
static GAUGE_READY: AtomicBool = AtomicBool::new(false);

/// What a counter poll tick yielded - three distinct outcomes that must not be mixed.
enum GaugePoll {
    /// The tick was skipped by the schedule: too early to read.
    Skipped,
    /// The raw counter value.
    Raw(u8),
    /// The read failed: bus busy, counter did not answer, copies did not match.
    Failed,
}

/// Reads the fuel counter no more often than [`GAUGE_POLL_MS`] and from a single tick.
///
/// # Safety
///
/// Passive level; called before taking [`STATE_LOCK`], does not touch the state.
unsafe fn read_gauge_if_due() -> GaugePoll {
    if !GAUGE_READY.load(Ordering::Acquire) {
        return GaugePoll::Skipped;
    }
    let device = unsafe { DEVICE };
    if device.is_null() {
        return GaugePoll::Skipped;
    }
    let now = monotonic_ms();
    let last = GAUGE_LAST_MS.load(Ordering::Acquire);
    if last != 0 && now.saturating_sub(last) < u64::from(GAUGE_POLL_MS) {
        return GaugePoll::Skipped;
    }
    if GAUGE_LAST_MS
        .compare_exchange(last, now, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        // Another tick has already taken this window.
        return GaugePoll::Skipped;
    }
    // SAFETY: `device` is live (checked above), the level is passive.
    match unsafe { crate::spb::read_batt_soc_raw(device) } {
        Some(raw) => GaugePoll::Raw(raw),
        None => GaugePoll::Failed,
    }
}

/// Initializes the state mutex. Once, from `evt_device_add`.
unsafe fn init_state_lock() {
    if STATE_LOCK_READY.load(Ordering::Acquire) {
        return;
    }
    // SAFETY: `Level` is reserved and must be zero.
    unsafe { wdk_sys::ntddk::KeInitializeMutex(core::ptr::addr_of_mut!(STATE_LOCK), 0) };
    STATE_LOCK_READY.store(true, Ordering::Release);
}

/// Holds the state mutex and releases it when the scope ends.
struct StateGuard {
    /// The mutex is held by this thread.
    held: bool,
}

impl Drop for StateGuard {
    fn drop(&mut self) {
        if self.held {
            // SAFETY: the mutex was taken by this very thread in `lock_state`.
            unsafe {
                let _ = wdk_sys::ntddk::KeReleaseMutex(core::ptr::addr_of_mut!(STATE_LOCK), 0);
            }
        }
    }
}

/// The mutex was abandoned by an exited thread. Ownership still passes to us,
/// and it must be released the same way as a normal acquisition.
const STATUS_ABANDONED: i32 = 0x0000_0080;

/// Takes the state mutex, waiting for another context.
///
/// Without it the telemetry timer, `EvtDevicePrepareHardware` and the IOCTL
/// handlers reach the same cached `WDFREQUEST` inside `SpbBus`, and WDF forbids
/// sending a request twice: on 18.09 this produced three
/// `WDF_VIOLATION (0x10D)` dumps with `Arg2 = 3` ("the request has already been
/// sent to the I/O target").
///
/// The wait return value must not be read as "success is anything not negative".
/// A zero relative timeout (a pointer to `QuadPart = 0`) does not mean "wait
/// forever" but "poll and return at once": with the mutex busy the call returns
/// `STATUS_TIMEOUT` (`0x102`) - a positive code, but with no ownership. That is
/// how the driver crashed on 18.09 at 17:08: `KeReleaseMutex` on a mutex owned by
/// someone else raises `STATUS_MUTANT_NOT_OWNED` (`0xC0000046`), which in the
/// `powershell.exe` context produced `SYSTEM_SERVICE_EXCEPTION (0x3B)` with the
/// stack `nt!KeReleaseMutantEx` ← `nt!KeReleaseMutex` ← `ln8000_kmdf+0xb774`.
/// Therefore the timeout is `NULL` (wait until the holder releases; the holder
/// always releases on its own, and a bus transfer is bounded by a second), and
/// only `STATUS_SUCCESS` or `STATUS_ABANDONED` count as acquisition.
fn lock_state() -> StateGuard {
    if !STATE_LOCK_READY.load(Ordering::Acquire) {
        return StateGuard { held: false };
    }
    // SAFETY: the mutex is initialized; kernel mode, no APCs, passive level;
    // `NULL` instead of a timeout - wait without a limit.
    let status = unsafe {
        wdk_sys::ntddk::KeWaitForSingleObject(
            core::ptr::addr_of_mut!(STATE_LOCK).cast(),
            wdk_sys::_KWAIT_REASON::Executive,
            // `KernelMode` from `_MODE` is an `i32`, while `KPROCESSOR_MODE` is a `CCHAR`.
            wdk_sys::_MODE::KernelMode as core::ffi::c_char,
            0,
            core::ptr::null_mut(),
        )
    };
    StateGuard {
        held: status == wdk_sys::STATUS_SUCCESS || status == STATUS_ABANDONED,
    }
}

/// Symbolic link for user mode: `\\.\nabu_ln8000`.
///
/// The buffer width was 20 characters for a 22-character name - the name was
/// silently truncated to `\DosDevices\nabu_ln8`, and the CLI could not open the
/// device.
const SYMLINK_CHARS: usize = 24;
const SYMLINK_NAME: &str = "/DosDevices/nabu_ln8000";
const SYMLINK: [u16; SYMLINK_CHARS] = utf16_lit(SYMLINK_NAME);

/// Length of the symlink name in bytes: without the terminating zero.
const SYMLINK_BYTES: u16 = (SYMLINK_NAME.len() as u16) * 2;

/// The name must fit the buffer - otherwise it is truncated and the device will
/// not open.
const _: () = assert!(
    SYMLINK_NAME.len() <= SYMLINK_CHARS,
    "symlink name longer than the buffer"
);

/// Device-add stages: written to the registry as a mark so that a start failure
/// is visible remotely, not only in the driver's debug output.
const STAGE_ENTER: u32 = 10;const STAGE_DEVICE: u32 = 11;
const STAGE_INTERFACE: u32 = 12;
const STAGE_SYMLINK: u32 = 13;
const STAGE_QUEUE: u32 = 14;
const STAGE_TIMER: u32 = 15;
const STAGE_DONE: u32 = 0xFF;
/// Hardware prepare stages: shows exactly where the bus or the chip fell over.
const STAGE_PREPARE: u32 = 30;
const STAGE_PREPARE_BUS: u32 = 31;
const STAGE_PREPARE_CHIP: u32 = 32;
const STAGE_PREPARE_CONFIG: u32 = 33;
const STAGE_READY: u32 = 0xFE;

/// Builds UTF-16 without a terminating zero: `'/'` characters become `'\\'`.
const fn utf16_lit(ascii: &str) -> [u16; SYMLINK_CHARS] {
    let bytes = ascii.as_bytes();
    let mut out = [0_u16; SYMLINK_CHARS];
    let mut index = 0;
    while index < bytes.len() && index < SYMLINK_CHARS {
        let byte = bytes[index];
        out[index] = if byte == b'/' { b'\\' as u16 } else { byte as u16 };
        index += 1;
    }
    out
}

/// Read rights for a registry key (`KEY_READ`).
const KEY_READ: ULONG = 0x0002_0019;

/// Parameter names and their values from the registry: what the driver reads.
struct DriverParams {
    config: PumpConfig,
    limits: GuardLimits,
    telemetry_ms: u32,
    /// Raw FCC value from the [`FCC_POLICY_VALUE_NAME`] parameter (0 - the policy
    /// is off: both when the parameter is absent and when it is zero).
    fcc_raw: u32,
}

/// Fills the buffer with the characters of a string and returns the length.
fn utf16_into(buffer: &mut [u16], text: &str) -> usize {
    let mut len = 0_usize;
    for byte in text.as_bytes() {
        if let Some(slot) = buffer.get_mut(len) {
            *slot = u16::from(*byte);
            len = len.saturating_add(1);
        }
    }
    len
}

/// Reads a single `REG_DWORD` parameter from an open key.
///
/// # Safety
///
/// `key` is a valid key handle opened for reading.
unsafe fn query_ulong(key: WDFKEY, name: &str) -> Option<u32> {
    let mut buffer = [0_u16; 32];
    let len = utf16_into(&mut buffer, name);
    if len == 0 || len > buffer.len() {
        return None;
    }
    let length = u16::try_from(len.saturating_mul(2)).unwrap_or(0);
    let value_name = UNICODE_STRING {
        Length: length,
        MaximumLength: length,
        Buffer: buffer.as_mut_ptr(),
    };
    let mut value: u32 = 0;
    // SAFETY: the key and the name buffer live until the end of the call; the
    // value is a local variable.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(WdfRegistryQueryULong, key, &raw const value_name, &raw mut value)
    };
    if status < 0 { None } else { Some(value) }
}

/// Write rights for a registry key (`KEY_SET_VALUE`).
const KEY_SET_VALUE: ULONG = 0x0002;

/// Writes a single numeric value into an already open key.
fn write_one(key: WDFKEY, name: &str, value: u32) {
    let mut buffer = [0_u16; 32];
    let len = utf16_into(&mut buffer, name);
    if len == 0 {
        return;
    }
    let length = u16::try_from(len.saturating_mul(2)).unwrap_or(0);
    let value_name = UNICODE_STRING {
        Length: length,
        MaximumLength: length,
        Buffer: buffer.as_mut_ptr(),
    };
    // SAFETY: the key is opened for writing, the name is a local buffer.
    unsafe {
        let _ = call_unsafe_wdf_function_binding!(
            WdfRegistryAssignULong,
            key,
            &raw const value_name,
            value,
        );
    }
}

/// Writes a pair of numeric marks into an already open key.
fn write_marker(key: WDFKEY, stage_name: &str, status_name: &str, stage: u32, status: NTSTATUS) {
    write_one(key, stage_name, stage);
    write_one(key, status_name, status as u32);
}

/// Device-add stage mark - into the device key.
///
/// The driver's debug output is unreachable without a debugger, so the progress
/// of device add is visible in the node's `Device Parameters`: `AddStage` -
/// where we stopped, `AddStatus` - with which code. Success is `AddStage = 0xFF`.
fn mark_stage(device: WDFDEVICE, stage: u32, status: NTSTATUS) {
    let mut key: WDFKEY = WDF_NO_HANDLE.cast();
    // SAFETY: the device was created by WDF; the key is opened for writing.
    let opened = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceOpenRegistryKey,
            device,
            PLUGPLAY_REGKEY_DEVICE,
            KEY_SET_VALUE,
            WDF_NO_OBJECT_ATTRIBUTES,
            &raw mut key,
        )
    };
    if opened < 0 {
        return;
    }
    write_marker(key, "AddStage", "AddStatus", stage, status);
    // SAFETY: the key was opened above and is no longer needed.
    unsafe {
        call_unsafe_wdf_function_binding!(WdfRegistryClose, key);
    }
}

/// Driver load mark - into the driver service `Parameters` key.
///
/// `DriverStage = 1` means `DriverEntry` reached the end and called
/// `WdfDriverCreate`. This distinguishes "the driver did not load" from "it
/// crashed during device add": in the second case the mark is there but the
/// device is not.
fn mark_driver(driver: WDFDRIVER, stage: u32, status: NTSTATUS) {
    let mut key: WDFKEY = WDF_NO_HANDLE.cast();
    // SAFETY: the driver handle was created by WDF.
    let opened = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDriverOpenParametersRegistryKey,
            driver,
            KEY_SET_VALUE,
            WDF_NO_OBJECT_ATTRIBUTES,
            &raw mut key,
        )
    };
    if opened < 0 {
        return;
    }
    write_marker(key, "DriverStage", "DriverStatus", stage, status);
    // SAFETY: the key was opened above and is no longer needed.
    unsafe {
        call_unsafe_wdf_function_binding!(WdfRegistryClose, key);
    }
}

/// Writes a single value into the driver `Parameters` key - for one-off diagnostics.
fn mark_driver_value(driver: WDFDRIVER, name: &str, value: u32) {
    let mut key: WDFKEY = WDF_NO_HANDLE.cast();
    // SAFETY: the driver handle was created by WDF.
    let opened = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDriverOpenParametersRegistryKey,
            driver,
            KEY_SET_VALUE,
            WDF_NO_OBJECT_ATTRIBUTES,
            &raw mut key,
        )
    };
    if opened < 0 {
        return;
    }
    write_one(key, name, value);
    // SAFETY: the key was opened above and is no longer needed.
    unsafe {
        call_unsafe_wdf_function_binding!(WdfRegistryClose, key);
    }
}

/// Writes a single value into the device key - for one-off diagnostics.
pub(crate) fn mark_device_value(device: WDFDEVICE, name: &str, value: u32) {
    let mut key: WDFKEY = WDF_NO_HANDLE.cast();
    // SAFETY: the device was created by WDF; the key is opened for writing.
    let opened = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceOpenRegistryKey,
            device,
            PLUGPLAY_REGKEY_DEVICE,
            KEY_SET_VALUE,
            WDF_NO_OBJECT_ATTRIBUTES,
            &raw mut key,
        )
    };
    if opened < 0 {
        return;
    }
    write_one(key, name, value);
    // SAFETY: the key was opened above and is no longer needed.
    unsafe {
        call_unsafe_wdf_function_binding!(WdfRegistryClose, key);
    }
}

/// Length of the connection request input (eight bytes, as in the reference client).
const ATTACH_INPUT_LEN: usize = 8;

/// Sends the connection request (`0x32C004`) to the device's parent target.
///
/// The reference client does this step before touching the registers, and sends
/// it not to the Resource Hub node but to its own device target. Returns the
/// status; the first words of the response are written to the registry as proof.
unsafe fn parent_attach_probe(device: WDFDEVICE) -> i32 {
    // SAFETY: the device was created; the target belongs to WDF.
    let target: WDFIOTARGET = unsafe { call_unsafe_wdf_function_binding!(WdfDeviceGetIoTarget, device) };
    if target.is_null() {
        return -3;
    }

    let mut request: WDFREQUEST = WDF_NO_HANDLE.cast();
    // SAFETY: the target is valid; the handles are local out-variables.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfRequestCreate,
            WDF_NO_OBJECT_ATTRIBUTES,
            target,
            &raw mut request,
        )
    };
    if status < 0 {
        return status;
    }

    let mut in_ptr: *mut core::ffi::c_void = core::ptr::null_mut();
    let mut in_mem: WDFMEMORY = WDF_NO_HANDLE.cast();
    // SAFETY: the connection input is eight bytes in non-paged pool.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfMemoryCreate,
            WDF_NO_OBJECT_ATTRIBUTES,
            wdk_sys::_POOL_TYPE::NonPagedPool,
            0,
            ATTACH_INPUT_LEN,
            &raw mut in_mem,
            &raw mut in_ptr,
        )
    };
    if status < 0 {
        return status;
    }
    let mut out_ptr: *mut core::ffi::c_void = core::ptr::null_mut();
    let mut out_mem: WDFMEMORY = WDF_NO_HANDLE.cast();
    // SAFETY: the connection response is 1024 bytes in non-paged pool.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfMemoryCreate,
            WDF_NO_OBJECT_ATTRIBUTES,
            wdk_sys::_POOL_TYPE::NonPagedPool,
            0,
            spb_abi::ATTACH_REPLY_LEN,
            &raw mut out_mem,
            &raw mut out_ptr,
        )
    };
    if status < 0 {
        return status;
    }

    // The reference client puts a magic and a four-byte value into the input.
    // SAFETY: the input is eight bytes, the write stays within it.
    unsafe {
        core::ptr::write_volatile(in_ptr.cast::<u32>(), spb_abi::ATTACH_MAGIC);
        core::ptr::write_volatile(in_ptr.add(4).cast::<u32>(), 1);
    }

    // SAFETY: the target, the request and the buffers are valid.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfIoTargetFormatRequestForIoctl,
            target,
            request,
            spb_abi::IOCTL_ATTACH,
            in_mem,
            core::ptr::null_mut(),
            out_mem,
            core::ptr::null_mut(),
        )
    };
    if status < 0 {
        return status;
    }

    let mut options: WDF_REQUEST_SEND_OPTIONS = unsafe { core::mem::zeroed() };
    options.Size = size_of_ulong::<WDF_REQUEST_SEND_OPTIONS>();
    options.Flags =
        (WDF_REQUEST_SEND_OPTION_SYNCHRONOUS | WDF_REQUEST_SEND_OPTION_TIMEOUT) as ULONG;
    options.Timeout = -10_000_000_i64;
    // SAFETY: synchronous send at passive level.
    let sent = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfRequestSend,
            request,
            target,
            &raw mut options,
        )
    };
    if sent == 0 {
        return -1;
    }
    // SAFETY: the request has completed, we read the status and the first words
    // of the response.
    let status = unsafe { call_unsafe_wdf_function_binding!(WdfRequestGetStatus, request) };
    if status >= 0 {
        for (index, name) in [(0_usize, "Par0"), (1, "Par1"), (2, "Par2"), (3, "Par3")] {
            // SAFETY: the response is 1024 bytes; we read the first four words.
            let word = unsafe { core::ptr::read_volatile(out_ptr.add(index * 4).cast::<u32>()) };
            mark_device_value(device, name, word);
        }
    }
    // SAFETY: the objects were created here and are no longer needed.
    unsafe {
        call_unsafe_wdf_function_binding!(WdfObjectDelete, request.cast());
        call_unsafe_wdf_function_binding!(WdfObjectDelete, in_mem.cast());
        call_unsafe_wdf_function_binding!(WdfObjectDelete, out_mem.cast());
    }
    status
}

/// Connection resource class and types from the WDM header.
const CONNECTION_CLASS_SERIAL: u32 = 0x02;
const CONNECTION_TYPE_SERIAL_I2C: u32 = 0x01;
const CONNECTION_TYPE_SERIAL_SPI: u32 = 0x02;
/// Vendor SerialBusType SPMI in raw ACPI (`0xC1`); after the RH translation it
/// often survives as the connection Type.
const CONNECTION_TYPE_SERIAL_SPMI_VENDOR: u32 = 0xC1;

/// Address of `APSD_STATUS` on the PM8150B USBIN peripheral (SID 2).
const USBIN_APSD_STATUS: u16 = 0x1307;

/// Writes connection resource details into the registry (on-hardware analysis).
fn mark_connection(device: WDFDEVICE, index: usize, id: u64, class: u32, kind: u32) {
    let (class_name, type_name, id_name) = match index {
        0 => ("C0Class", "C0Type", "C0Low"),
        1 => ("C1Class", "C1Type", "C1Low"),
        2 => ("C2Class", "C2Type", "C2Low"),
        _ => ("C3Class", "C3Type", "C3Low"),
    };
    mark_device_value(device, class_name, class);
    mark_device_value(device, type_name, kind);
    mark_device_value(device, id_name, id as u32);
}

/// Collects all connection resources from `_CRS` in declaration order.
///
/// # Safety
///
/// The resource list is valid; `out` is the caller's local buffer.
unsafe fn collect_connections(resources: WDFCMRESLIST, out: &mut [(u64, u32, u32); 4]) -> usize {
    let mut index: ULONG = 0;
    let mut found = 0_usize;
    loop {
        // SAFETY: the resource list is immutable during the iteration.
        let descriptor = unsafe {
            call_unsafe_wdf_function_binding!(WdfCmResourceListGetDescriptor, resources, index)
        };
        if descriptor.is_null() {
            break;
        }
        // SAFETY: the descriptor came from the resource list.
        let kind = unsafe { (*descriptor).Type };
        if u32::from(kind) == CmResourceTypeConnection {
            // SAFETY: for the Connection type the `Connection` field is filled in.
            let class = unsafe { (*descriptor).u.Connection.Class };
            let connection_kind = unsafe { (*descriptor).u.Connection.Type };
            let low = unsafe { (*descriptor).u.Connection.IdLowPart };
            let high = unsafe { (*descriptor).u.Connection.IdHighPart };
            if found < out.len() {
                out[found] = (
                    (u64::from(high) << 32) | u64::from(low),
                    u32::from(class),
                    u32::from(connection_kind),
                );
                found = found.saturating_add(1);
            }
        }
        index = index.saturating_add(1);
    }
    found
}

/// Selects the I²C connection identifier for the LN8000.
///
/// The classes and types are from `wdm.h`: `CLASS_SERIAL` = 0x02,
/// `TYPE_SERIAL_I2C` = 0x01. We look for I²C first; if there is none, any serial
/// (SPI), so as not to break diagnostics on non-standard overlays.
fn select_i2c_connection(connections: &[(u64, u32, u32); 4], count: usize) -> Option<u64> {
    for (id, class, kind) in connections.iter().take(count) {
        if *class == CONNECTION_CLASS_SERIAL && *kind == CONNECTION_TYPE_SERIAL_I2C {
            return Some(*id);
        }
    }
    for (id, class, kind) in connections.iter().take(count) {
        if *class == CONNECTION_CLASS_SERIAL && *kind == CONNECTION_TYPE_SERIAL_SPI {
            return Some(*id);
        }
    }
    None
}

/// Selects the second serial connection - a USBIN SPMI candidate after the ACPI
/// overlay.
///
/// In the stock DSDT PEIC has only I²C → returns `None`. After the SSDT with the
/// SID=2 / periph `0x13` descriptor a second connection appears (often
/// Type=`0xC1`).
fn select_usbin_connection(
    connections: &[(u64, u32, u32); 4],
    count: usize,
    i2c_id: u64,
) -> Option<u64> {
    for (id, class, kind) in connections.iter().take(count) {
        if *id == i2c_id || *class != CONNECTION_CLASS_SERIAL {
            continue;
        }
        if *kind == CONNECTION_TYPE_SERIAL_SPMI_VENDOR || *kind != CONNECTION_TYPE_SERIAL_I2C {
            return Some(*id);
        }
    }
    for (id, class, _) in connections.iter().take(count) {
        if *class == CONNECTION_CLASS_SERIAL && *id != i2c_id {
            return Some(*id);
        }
    }
    None
}

/// Tries an SPMI read of `APSD_STATUS` (0x1307) through the second PEIC connection.
///
/// Without the ACPI overlay `usbin_id` = `None` → only the
/// `UsbinOpen=0xFFFFFFFF` mark.
///
/// # Safety
///
/// Passive level; the device has been created.
unsafe fn probe_usbin_spmi(device: WDFDEVICE, usbin_id: Option<u64>) {
    let Some(id) = usbin_id else {
        mark_device_value(device, "UsbinOpen", 0xFFFF_FFFF);
        return;
    };
    // SAFETY: passive level, the device has been created.
    let mut bus = match unsafe { SpbBus::open(device, id, true) } {
        Ok(bus) => {
            mark_device_value(device, "UsbinOpen", 1);
            bus
        }
        Err(_) => {
            mark_device_value(device, "UsbinOpen", 0);
            return;
        }
    };
    bus.set_variant(0);
    for (big_endian, st_name, val_name) in [
        (true, "UsbinBeSt", "UsbinBeVal"),
        (false, "UsbinLeSt", "UsbinLeVal"),
    ] {
        match bus.transact_spmi16(USBIN_APSD_STATUS, None, big_endian) {
            Ok(value) => {
                mark_device_value(device, st_name, 0);
                mark_device_value(device, val_name, u32::from(value));
            }
            Err(_) => {
                mark_device_value(device, st_name, bus.last_status() as u32);
                mark_device_value(device, val_name, 0xFFFF_FFFF);
            }
        }
    }
}

/// Tries to read a chip register by sending an SPB sequence to the device's
/// parent target (the controller stack) rather than to the Resource Hub node.
///
/// The reference client keeps the target handle in a global variable and builds
/// the transfer list the same way we do; the question was exactly where the
/// request goes.
unsafe fn parent_sequence_probe(device: WDFDEVICE, address: u8) -> i32 {
    // SAFETY: the device was created; the target belongs to WDF.
    let target: WDFIOTARGET = unsafe { call_unsafe_wdf_function_binding!(WdfDeviceGetIoTarget, device) };
    if target.is_null() {
        return -3;
    }

    let mut request: WDFREQUEST = WDF_NO_HANDLE.cast();
    // SAFETY: the target is valid; the handle is a local out-variable.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfRequestCreate,
            WDF_NO_OBJECT_ATTRIBUTES,
            target,
            &raw mut request,
        )
    };
    if status < 0 {
        return status;
    }

    // Transfer list: exactly as many entries as there are transfers - as in the reference.
    let list_len = spb_abi::SpbTransferList::area_size(2);
    let mut list_ptr: *mut core::ffi::c_void = core::ptr::null_mut();
    let mut list_mem: WDFMEMORY = WDF_NO_HANDLE.cast();
    // SAFETY: we allocate a non-paged buffer for the transfer list.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfMemoryCreate,
            WDF_NO_OBJECT_ATTRIBUTES,
            wdk_sys::_POOL_TYPE::NonPagedPool,
            0,
            list_len,
            &raw mut list_mem,
            &raw mut list_ptr,
        )
    };
    if status < 0 {
        return status;
    }

    let mut data_ptr: *mut core::ffi::c_void = core::ptr::null_mut();
    let mut data_mem: WDFMEMORY = WDF_NO_HANDLE.cast();
    // SAFETY: the data buffer is the register plus the response byte.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfMemoryCreate,
            WDF_NO_OBJECT_ATTRIBUTES,
            wdk_sys::_POOL_TYPE::NonPagedPool,
            0,
            8,
            &raw mut data_mem,
            &raw mut data_ptr,
        )
    };
    if status < 0 {
        return status;
    }

    // SAFETY: both buffers were allocated above and are large enough.
    unsafe {
        core::ptr::write_volatile(data_ptr.cast::<u8>(), address);
        let list = list_ptr.cast::<spb_abi::SpbTransferList>();
        (*list).size = u32::try_from(spb_abi::SpbTransferList::header_size()).unwrap_or(0);
        (*list).reserved = 0;
        (*list).transfer_count = 2;
        (*list).transfers[0] = spb_abi::entry_init(
            spb_abi::SPB_DIRECTION_TO_DEVICE,
            data_ptr,
            1,
        );
        let second = core::ptr::addr_of_mut!((*list).transfers[0]).add(1);
        *second = spb_abi::entry_init(
            spb_abi::SPB_DIRECTION_FROM_DEVICE,
            data_ptr.add(1),
            1,
        );
    }

    // SAFETY: the target, the request and the buffers are valid.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfIoTargetFormatRequestForIoctl,
            target,
            request,
            spb_abi::IOCTL_SPB_EXECUTE_SEQUENCE,
            list_mem,
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            core::ptr::null_mut(),
        )
    };
    if status < 0 {
        return status;
    }

    let mut options: WDF_REQUEST_SEND_OPTIONS = unsafe { core::mem::zeroed() };
    options.Size = size_of_ulong::<WDF_REQUEST_SEND_OPTIONS>();
    options.Flags =
        (WDF_REQUEST_SEND_OPTION_SYNCHRONOUS | WDF_REQUEST_SEND_OPTION_TIMEOUT) as ULONG;
    options.Timeout = -10_000_000_i64;
    // SAFETY: synchronous send at passive level.
    let sent = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfRequestSend,
            request,
            target,
            &raw mut options,
        )
    };
    if sent == 0 {
        return -1;
    }
    // SAFETY: the request has completed; we read the status and the byte read.
    let status = unsafe { call_unsafe_wdf_function_binding!(WdfRequestGetStatus, request) };
    if status >= 0 {
        // SAFETY: the data buffer is 8 bytes; the response lies second.
        let value = unsafe { core::ptr::read_volatile(data_ptr.add(1).cast::<u8>()) };
        mark_device_value(device, "ParSeqValue", u32::from(value));
    }
    // SAFETY: the objects were created here and are no longer needed.
    unsafe {
        call_unsafe_wdf_function_binding!(WdfObjectDelete, request.cast());
        call_unsafe_wdf_function_binding!(WdfObjectDelete, list_mem.cast());
        call_unsafe_wdf_function_binding!(WdfObjectDelete, data_mem.cast());
    }
    status
}

/// Reads profile parameters from the device registry (`HKR, Parameters, ...`).
///
/// This is how whoever installs the driver can change the thresholds without
/// rebuilding it: the INF sets the defaults, and here they are applied with
/// bounds checking. An invalid value does not spoil the profile - it is rejected
/// and the previous one stays.
///
/// # Safety
///
/// Passive IRQL, the device has already been created.
unsafe fn read_parameters(device: WDFDEVICE) -> DriverParams {
    let mut config = PumpConfig::for_qc35_class_b();
    let mut limits = GuardLimits::standard();
    // Profile current limit for the return after the fold-back band: a snapshot
    // of `config`, because `set_iin_limit` overwrites `config.iin_limit_ua` with
    // every setpoint (protection, session ICL) and the "profile" value is lost.
    limits.iin_profile_ua = config.iin_limit_ua;
    let mut telemetry_ms = 250_u32;
    // Applied `FccRaw` value: `0` - the parameter is absent or zero, that is the
    // FCC policy is off and register `0x1061` is not touched during the boot.
    let mut fcc_raw = 0_u32;
    // Applied `ProtectionProfile` value: `None` - there is no parameter in the
    // registry and the code profile stays (`for_qc35_class_b`, loops enabled).
    let mut protection_profile: Option<u32> = None;

    let mut device_key: WDFKEY = WDF_NO_HANDLE.cast();
    // SAFETY: the device was created; the key is only opened for reading.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceOpenRegistryKey,
            device,
            PLUGPLAY_REGKEY_DEVICE,
            KEY_READ,
            WDF_NO_OBJECT_ATTRIBUTES,
            &raw mut device_key,
        )
    };
    if status < 0 {
        println!("ln8000-kmdf: device key did not open ({status:#010X}); using the default profile");
        // There is no FCC policy in this outcome by construction (the parameter
        // was not read), but the mark is published here too: otherwise the
        // absence of `FccCfgRaw` in a dump would be indistinguishable from "the
        // driver never got to these marks".
        mark_device_value(device, "FccCfgRaw", 0);
        mark_profile(device, &config, protection_profile);
        return DriverParams {
            config,
            limits,
            telemetry_ms,
            fcc_raw,
        };
    }

    let mut params_key: WDFKEY = WDF_NO_HANDLE.cast();
    let mut buffer = [0_u16; 32];
    let len = utf16_into(&mut buffer, "Parameters");
    let length = u16::try_from(len.saturating_mul(2)).unwrap_or(0);
    let subkey_name = UNICODE_STRING {
        Length: length,
        MaximumLength: length,
        Buffer: buffer.as_mut_ptr(),
    };
    // SAFETY: the device key is valid; the name is a local buffer.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfRegistryOpenKey,
            device_key,
            &raw const subkey_name,
            KEY_READ,
            WDF_NO_OBJECT_ATTRIBUTES,
            &raw mut params_key,
        )
    };
    if status < 0 {
        println!("ln8000-kmdf: Parameters subkey did not open ({status:#010X}); using defaults");
        // SAFETY: the key was opened above and is no longer needed.
        unsafe {
            call_unsafe_wdf_function_binding!(WdfRegistryClose, device_key);
        }
        mark_device_value(device, "FccCfgRaw", 0);
        mark_profile(device, &config, protection_profile);
        return DriverParams {
            config,
            limits,
            telemetry_ms,
            fcc_raw,
        };
    }

    for name in [
        // The protection profile is applied first: it defines the whole
        // template, and without this explicit setpoints (current, voltage,
        // threshold) would be overwritten by the template values - verified on
        // the device.
        "ProtectionProfile",
        "IinLimitUa",
        "VbatFloatUv",
        "VacOvpUv",
        "NtcAlarmCfg",
        "WatchdogEnabled",
    ] {
        // SAFETY: the Parameters key is open for reading.
        if let Some(value) = unsafe { query_ulong(params_key, name) } {
            if config.apply_parameter(name, value) {
                if name == "ProtectionProfile" {
                    protection_profile = Some(value);
                }
            } else {
                println!("ln8000-kmdf: parameter {name} = {value} rejected, the default stays");
            }
        }
    }
    // `IinLimitUa` from the registry is the profile limit itself: protection
    // returns the current to it after the voltage and temperature leave the band.
    limits.iin_profile_ua = config.iin_limit_ua;
    // SAFETY: the Parameters key is open for reading.
    if let Some(ms) = unsafe { query_ulong(params_key, "TelemetryMs") } {
        if (100..=60_000).contains(&ms) {
            telemetry_ms = ms;
        } else {
            println!("ln8000-kmdf: telemetry period {ms} ms outside the 100..60000 bounds, keeping {telemetry_ms}");
        }
    }

    // Buck FCC policy: `FccRaw` is the raw value of the FCC field of register
    // `0x1061` that the driver sets while the pump is carrying current. On why
    // this is a parameter rather than a constant, and what the live measurement
    // showed, see [`FCC_POLICY_VALUE_NAME`]. The value deliberately has no
    // bounds: any `N > 0` means "raise up to N bits", the ceiling
    // [`FCC_WRITE_MAX_UA`] is applied by `raw_for`, so an inflated N hits the
    // ceiling instead of wrapping modulo the field.
    // SAFETY: the Parameters key is open for reading.
    if let Some(raw) = unsafe { query_ulong(params_key, FCC_POLICY_VALUE_NAME) } {
        fcc_raw = raw;
    }
    // The mark is published as a zero too: it shows in a dump which policy branch
    // is active, and it is the mark to look at next to `ChgrFccRaw` when the
    // question of the factory register value comes up (see
    // `DriverState::fcc_cfg_raw`).
    mark_device_value(device, "FccCfgRaw", fcc_raw);
    if fcc_raw == 0 {
        println!("ln8000-kmdf: buck FCC policy off (FccRaw not set or zero)");
    } else {
        println!(
            "ln8000-kmdf: buck FCC policy on: FccRaw = {fcc_raw} ({} mA, ceiling {} mA)",
            fcc_raw.saturating_mul(FCC_STEP_UA / 1000),
            FCC_WRITE_MAX_UA / 1000
        );
    }

    // Protection thresholds arrive as a set, and the order of the thresholds is
    // checked **once at the end**: `apply_parameter` checks consistency after
    // every value, so a set that changes several thresholds at once was never
    // applied - the first value was compared against neighbours not yet updated.
    // Live case 19.09: `TempReduceDc=600` was rejected against the old
    // `TempBypassDc=480` in any order, and protection stayed at 43,0/48,0 °C,
    // that is below the crystal idle temperature (46,1 °C).
    //
    // The `before` snapshot is needed so that a rejected set does not leave the
    // thresholds partially updated: either the whole set or the previous one.
    let before = limits;
    for name in [
        "TempReduceDc",
        "TempBypassDc",
        "TempStopDc",
        "IinMaxUa",
        "IinTargetUa",
        "IinFloorUa",
        "VbatReduceUv",
    ] {
        // SAFETY: the Parameters key is open for reading.
        if let Some(value) = unsafe { query_ulong(params_key, name) } {
            if !limits.apply_parameter_lenient(name, value) {
                println!("ln8000-kmdf: threshold {name} = {value} rejected (out of range)");
            }
        }
    }
    if !limits.validate() {
        println!(
            "ln8000-kmdf: threshold set inconsistent ({} / {} / {}); keeping the previous one",
            limits.temp_reduce_dc, limits.temp_bypass_dc, limits.temp_stop_dc
        );
        limits = before;
    }

    println!(
        "ln8000-kmdf: protection thresholds: {} / {} / {} (0.1 °C), current {} µA, target {} µA",
        limits.temp_reduce_dc, limits.temp_bypass_dc, limits.temp_stop_dc, limits.iin_max_ua, limits.iin_target_ua
    );

    println!(
        "ln8000-kmdf: profile from the registry: current {} µA, voltage {} µV, pump protection loops {}",
        config.iin_limit_ua,
        config.vbat_float_uv,
        if config.tdie_prot_disabled {
            "off (as in the Device Tree)"
        } else {
            "on"
        }
    );

    // SAFETY: both keys were opened above and are no longer needed.
    unsafe {
        call_unsafe_wdf_function_binding!(WdfRegistryClose, params_key);
        call_unsafe_wdf_function_binding!(WdfRegistryClose, device_key);
    }

    mark_profile(device, &config, protection_profile);

    DriverParams {
        config,
        limits,
        telemetry_ms,
        fcc_raw,
    }
}

/// Driver entry point.
///
/// # Safety
///
/// Called by the kernel; the arguments are valid per the WDF contract.
#[unsafe(no_mangle)]
#[unsafe(link_section = "INIT")]
pub unsafe extern "system" fn DriverEntry(
    driver: &mut wdk_sys::DRIVER_OBJECT,
    registry_path: PCUNICODE_STRING,
) -> NTSTATUS {
    println!("ln8000-kmdf: DriverEntry");

    // BattC WMI QueryWmiRegInfo needs a durable registry path (simbatt).
    // SAFETY: `registry_path` is valid for the duration of DriverEntry; we copy.
    unsafe { battery::set_registry_path(registry_path) };

    let mut config = WDF_DRIVER_CONFIG {
        Size: size_of_ulong::<WDF_DRIVER_CONFIG>(),
        EvtDriverDeviceAdd: Some(evt_device_add),
        ..unsafe { core::mem::zeroed() }
    };
    config.Size = size_of_ulong::<WDF_DRIVER_CONFIG>();

    let mut driver_handle: WDFDRIVER = WDF_NO_HANDLE.cast();
    // SAFETY: the configuration is filled in, the handle is a local variable.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDriverCreate,
            core::ptr::from_mut(driver).cast::<wdk_sys::DRIVER_OBJECT>(),
            registry_path,
            WDF_NO_OBJECT_ATTRIBUTES,
            &raw mut config,
            &raw mut driver_handle,
        )
    };
    if status < 0 {
        println!("ln8000-kmdf: WdfDriverCreate failed: {status:#010X}");
    } else {
        // The state field holds seconds since system boot: they show whether the
        // mark refers to the current run or was left over from the previous one.
        let mut stamp: u64 = 0;
        // SAFETY: KeQueryInterruptTimePrecise is a documented kernel function.
        let ticks = unsafe { wdk_sys::ntddk::KeQueryInterruptTimePrecise(&raw mut stamp) };
        mark_driver(driver_handle, 1, (ticks / 10_000_000) as i32);
    }
    status
}

/// Creates the device, the interface and the control-request queue.
///
/// # Safety
///
/// Called by WDF at passive level.
unsafe extern "C" fn evt_device_add(
    driver: WDFDRIVER,
    mut device_init: PWDFDEVICE_INIT,
) -> NTSTATUS {
    // SAFETY: the very first driver callback: the mutex is needed before the
    // telemetry timer and the IOCTL queue start working.
    unsafe { init_state_lock() };
    // The first mark goes into the driver key: it is available even before the
    // device is created, so it shows even a failure of the very first call.
    mark_driver(driver, STAGE_ENTER, 0);
    // Probe: the size of the attributes structure the driver builds. The value is
    // needed to see exactly what our structure failed KMDF on.
    mark_driver_value(driver, "AttrSize", size_of_ulong::<WDF_OBJECT_ATTRIBUTES>());

    // PnP handlers: hardware preparation and release.
    let mut pnp = WDF_PNPPOWER_EVENT_CALLBACKS {
        Size: size_of_ulong::<WDF_PNPPOWER_EVENT_CALLBACKS>(),
        EvtDevicePrepareHardware: Some(evt_prepare_hardware),
        EvtDeviceReleaseHardware: Some(evt_release_hardware),
        ..unsafe { core::mem::zeroed() }
    };
    pnp.Size = size_of_ulong::<WDF_PNPPOWER_EVENT_CALLBACKS>();
    // SAFETY: `device_init` is provided by WDF before the device is created.
    unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceInitSetPnpPowerEventCallbacks,
            device_init,
            &raw mut pnp,
        );
    }

    // BattC must see DEVICE_CONTROL + SYSTEM_CONTROL before the WDF queue (simbatt).
    // SAFETY: `device_init` still owned by the driver here.
    let status = unsafe { battery::assign_ioctl_preprocess(device_init) };
    if status < 0 {
        println!("ln8000-kmdf: battery preprocess failed: {status:#010X}");
        mark_driver(driver, STAGE_DEVICE, status);
        return status;
    }

    // Access policy. The control codes are `FILE_ANY_ACCESS` and the driver makes
    // no requestor check, so the descriptor set here is the only thing standing
    // between the pump and any process on the tablet. A failure here is not
    // survivable - `WdfDeviceCreate` below rejects the same initialisation
    // structure with the same status and the device fails to start - so the code
    // is recorded as a mark and the failure is printed. See `sddl`.
    // SAFETY: `device_init` is still owned by the driver; the device is created below.
    let sddl_status = unsafe { sddl::assign(device_init) };
    if sddl_status < 0 {
        println!("ln8000-kmdf: device access policy not applied: {sddl_status:#010X}");
    }
    mark_driver_value(driver, "SddlSt", sddl_status as u32);

    let mut device: WDFDEVICE = WDF_NO_HANDLE.cast();
    // We do not pass attributes: NULL is the stock WDF variant. KMDF rejects our
    // structure here with STATUS_WDF_OBJECT_ATTRIBUTES_INVALID (0xC0200209),
    // verified on the tablet; the device needs neither a context nor callbacks.
    // SAFETY: `device_init` is valid.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceCreate,
            &raw mut device_init,
            WDF_NO_OBJECT_ATTRIBUTES,
            &raw mut device,
        )
    };
    if status < 0 {
        println!("ln8000-kmdf: WdfDeviceCreate failed: {status:#010X}");
        mark_driver(driver, STAGE_DEVICE, status);
        return status;
    }
    mark_driver(driver, STAGE_DEVICE, 0);

    // The interface through which user mode finds the driver.
    // SAFETY: the device was created; the GUID is a static constant.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceCreateDeviceInterface,
            device,
            &GUID_DEVINTERFACE_LN8000,
            core::ptr::null_mut(),
        )
    };
    if status < 0 {
        println!("ln8000-kmdf: failed to create the interface: {status:#010X}");
        mark_driver(driver, STAGE_INTERFACE, status);
        return status;
    }

    // Symbolic link: for the diagnostic CLI it is enough to open
    // \\.\nabu_ln8000 - without enumerating interfaces through SetupAPI.
    let mut link = UNICODE_STRING {
        Length: SYMLINK_BYTES,
        MaximumLength: SYMLINK_BYTES,
        Buffer: SYMLINK.as_ptr().cast_mut(),
    };
    // SAFETY: the name is a static UTF-16 buffer, the device has been created.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceCreateSymbolicLink,
            device,
            &raw mut link,
        )
    };
    if status < 0 {
        // The symlink is a convenience, not a condition of operation: the driver
        // lives without it too.
        println!("ln8000-kmdf: symbolic link not created: {status:#010X}");
        mark_driver(driver, STAGE_SYMLINK, status);
        // A separate name: the main mark is overwritten by the following stages.
        mark_driver_value(driver, "LinkStatus", status as u32);
    }

    // Control-request queue and telemetry timer.
    let mut queue_config = WDF_IO_QUEUE_CONFIG {
        Size: size_of_ulong::<WDF_IO_QUEUE_CONFIG>(),
        // Default queue: the client's control requests land here. Without this
        // flag they go to the default WDF queue and complete with the "unknown
        // function" status (the client sees error code 1).
        DefaultQueue: 1,
        DispatchType: WdfIoQueueDispatchSequential,
        PowerManaged: WdfTrue,
        EvtIoDeviceControl: Some(evt_io_device_control),
        ..unsafe { core::mem::zeroed() }
    };
    queue_config.Size = size_of_ulong::<WDF_IO_QUEUE_CONFIG>();

    let mut queue: WDFQUEUE = WDF_NO_HANDLE.cast();
    // SAFETY: the configuration is filled in, the device has been created.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfIoQueueCreate,
            device,
            &raw mut queue_config,
            WDF_NO_OBJECT_ATTRIBUTES,
            &raw mut queue,
        )
    };
    if status < 0 {
        println!("ln8000-kmdf: WdfIoQueueCreate failed: {status:#010X}");
        mark_driver(driver, STAGE_QUEUE, status);
        return status;
    }

    // One-shot timer: Period is fixed at Create and cannot track registry
    // `TelemetryMs`. Re-arm each tick from `st.telemetry_ms` (was: Period=1000
    // while DueTime used 250 → UI saw plug/unplug only ~once per second).
    let mut timer_config = WDF_TIMER_CONFIG {
        Size: size_of_ulong::<WDF_TIMER_CONFIG>(),
        Period: 0,
        EvtTimerFunc: Some(evt_telemetry_timer),
        ..unsafe { core::mem::zeroed() }
    };
    timer_config.Size = size_of_ulong::<WDF_TIMER_CONFIG>();
    // Automatic serialization: the callback does not overlap with the queue.
    timer_config.AutomaticSerialization = 1;

    let mut timer_attributes = WDF_OBJECT_ATTRIBUTES {
        Size: size_of_ulong::<WDF_OBJECT_ATTRIBUTES>(),
        ParentObject: device.cast(),
        ExecutionLevel: WdfExecutionLevelPassive,
        SynchronizationScope: WdfSynchronizationScopeNone,
        ..unsafe { core::mem::zeroed() }
    };
    timer_attributes.Size = size_of_ulong::<WDF_OBJECT_ATTRIBUTES>();
    // A parent is mandatory: without ParentObject WDF fails with
    // STATUS_WDF_PARENT_NOT_SPECIFIED (0xC0200212) - verified on the tablet.

    let mut timer: WDFTIMER = WDF_NO_HANDLE.cast();
    // SAFETY: the configuration and the attributes are filled in.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfTimerCreate,
            &raw mut timer_config,
            &raw mut timer_attributes,
            &raw mut timer,
        )
    };
    if status < 0 {
        // The timer is not a condition of operation: without it control and
        // diagnostics are alive, polling happens on request. The reason for the
        // failure is in the wdftimer.h header: automatic serialization requires
        // compatibility with DISPATCH, while a bus transfer needs PASSIVE. The
        // right variant is a DISPATCH-level timer with work deferred to PASSIVE;
        // that is a separate piece of work.
        println!("ln8000-kmdf: WdfTimerCreate failed: {status:#010X}");
        mark_driver(driver, STAGE_TIMER, status);
    }

    // Remember the timer in a static: prepare_hardware starts it when the bus is ready.
    // SAFETY: a single device instance, access is serialized.
    unsafe {
        TIMER = timer;
        DEVICE = device;
    }

    println!("ln8000-kmdf: device ready");
    mark_driver(driver, STAGE_DONE, 0);
    mark_stage(device, STAGE_DONE, 0);
    wdk_sys::STATUS_SUCCESS
}

/// Telemetry timer: a single instance per driver.
static mut TIMER: WDFTIMER = core::ptr::null_mut();
/// Device for the timer callback (timer parent = device; stored explicitly).
static mut DEVICE: WDFDEVICE = core::ptr::null_mut();

/// Parses `_CRS`, opens the bus and configures the LN8000.
///
/// # Safety
///
/// Called by WDF at passive level; the resource lists are valid.
unsafe extern "C" fn evt_prepare_hardware(
    device: WDFDEVICE,
    _resources_raw: WDFCMRESLIST,
    resources_translated: WDFCMRESLIST,
) -> NTSTATUS {
    // We hold the state and the bus until prepare finishes: the telemetry timer
    // has already been created and may tick in parallel.
    let _state = lock_state();
    // 1. Look for the connection resource (I²C) and take the identifier.
    mark_stage(device, STAGE_PREPARE, 0);
    let mut connections = [(0_u64, 0_u32, 0_u32); 4];
    // SAFETY: the resource list is valid, the buffer is local.
    let connection_count = unsafe { collect_connections(resources_translated, &mut connections) };
    mark_device_value(device, "ConnCount", u32::try_from(connection_count).unwrap_or(0));
    for (index, (id, class, kind)) in connections.iter().take(connection_count).enumerate() {
        mark_connection(device, index, *id, *class, *kind);
    }
    let peripheral_id = match select_i2c_connection(&connections, connection_count) {
        Some(id) => id,
        None => {
            println!("ln8000-kmdf: no serial connection (I2C/SPI) in _CRS");
            mark_stage(device, STAGE_PREPARE_BUS, wdk_sys::STATUS_DEVICE_NOT_READY);
            return wdk_sys::STATUS_DEVICE_NOT_READY;
        }
    };
    let usbin_id = select_usbin_connection(&connections, connection_count, peripheral_id);
    match usbin_id {
        Some(id) => {
            mark_device_value(device, "UsbinConn", 1);
            mark_device_value(device, "UsbinLow", id as u32);
            mark_device_value(device, "UsbinHigh", (id >> 32) as u32);
        }
        None => mark_device_value(device, "UsbinConn", 0),
    }
    println!("ln8000-kmdf: connection {peripheral_id:#018X}");
    // SAFETY: passive level, the device has been created.
    let mut bus = match unsafe { SpbBus::open(device, peripheral_id, false) } {
        Ok(bus) => {
            let path = bus.path_string();
            let text = core::str::from_utf8(&path).unwrap_or("?");
            println!("ln8000-kmdf: bus {text}");
            bus
        }
        Err(err) => {
            println!("ln8000-kmdf: bus unavailable: {err}");
            let st = unsafe { state() };
            st.last_error = -1;
            mark_stage(device, STAGE_PREPARE_BUS, wdk_sys::STATUS_DEVICE_NOT_READY);
            return wdk_sys::STATUS_DEVICE_NOT_READY;
        }
    };

    // 3. Identify the chip and configure it.
    // The profile is taken from the device registry: `HKR, Parameters, ...` from
    // the INF. The values are checked against bounds; an invalid value does not
    // change the profile.
    // SAFETY: passive level, the device has been created.
    let params = unsafe { read_parameters(device) };
    let config = params.config;
    let guard_limits = params.limits;

    // Chip probe before identification: a raw exchange with the DEVICE_ID
    // register (0x00). We iterate over request layouts for the bus node and write
    // the result of each into the registry: without a debugger this is the only
    // way to understand exactly what the node rejects - and at the same time to
    // find a working variant.
    mark_device_value(device, "ConnLow", peripheral_id as u32);
    mark_device_value(device, "ConnHigh", (peripheral_id >> 32) as u32);
    mark_device_value(device, "RegAddr", u32::from(ln8000::regs::DEVICE_ID));
    // Connecting to the peripheral through the device's parent target: this is
    // exactly how the Qualcomm reference client does it, and it is exactly there
    // (and not to the node) that the 0x32C004 request goes.
    let parent_attach = unsafe { parent_attach_probe(device) };
    mark_device_value(device, "ParAttachStatus", parent_attach as u32);
    mark_device_value(device, "ParAttachOk", if parent_attach >= 0 { 1 } else { 0 });
    // A sequence to the parent target: the same operation, but a different destination.
    let parent_sequence = unsafe { parent_sequence_probe(device, ln8000::regs::DEVICE_ID) };
    mark_device_value(device, "ParSeqStatus", parent_sequence as u32);
    mark_device_value(device, "ParSeqOk", if parent_sequence >= 0 { 1 } else { 0 });
    let attach = bus.attach();
    mark_device_value(device, "AttachStatus", attach as u32);    for (index, name) in [(0_usize, "Att0"), (1, "Att1"), (2, "Att2"), (3, "Att3")] {
        mark_device_value(device, name, bus.attach_word(index));
    }
    let mut working = None;
    for (variant, names) in [
        (0_u8, ("Ok0", "St0")),
        (1_u8, ("Ok1", "St1")),
        (2_u8, ("Ok2", "St2")),
        (3_u8, ("Ok3", "St3")),
        (4_u8, ("Ok4", "St4")),
        (5_u8, ("Ok5", "St5")),
    ] {
        bus.set_variant(variant);
        let result = bus.transact(ln8000::regs::DEVICE_ID, None);
        let status = bus.last_status() as u32;
        match result {
            Ok(value) => {
                mark_device_value(device, names.0, 1);
                mark_device_value(device, "ProbeValue", u32::from(value));
                if value == ln8000::regs::DEVICE_ID_VALUE && working.is_none() {
                    working = Some(variant);
                }
            }
            Err(_) => mark_device_value(device, names.0, 0),
        }
        mark_device_value(device, names.1, status);
    }
    if let Some(variant) = working {
        bus.set_variant(variant);
        mark_device_value(device, "Variant", u32::from(variant));
    }

    // Check whether holding the connection matters: the same sequence, but now on
    // a busy connection. If holding breaks the exchange, we will see the difference.
    let lock = bus.lock_connection();
    mark_device_value(device, "LockStatus", lock as u32);
    mark_device_value(device, "LockOk", if lock >= 0 { 1 } else { 0 });
    bus.set_variant(0);
    let locked = bus.transact(ln8000::regs::DEVICE_ID, None);
    match locked {
        Ok(value) => {
            mark_device_value(device, "LockedOk", 1);
            mark_device_value(device, "LockedValue", u32::from(value));
        }
        Err(_) => mark_device_value(device, "LockedOk", 0),
    }
    mark_device_value(device, "LockedStatus", bus.last_status() as u32);

    // Read a few registers directly: if the chip answers, we will see its
    // identification code (we expect 0x42 in register 0x00).
    for (address, name) in [(0x00_u8, "Reg00"), (0x03, "Reg03"), (0x31, "Reg31")] {
        bus.set_variant(0);
        match bus.transact(address, None) {
            Ok(value) => mark_device_value(device, name, u32::from(value)),
            Err(_) => mark_device_value(device, name, 0xFFFF_FFFF),
        }
    }

    // Node capability matrix: which SPB control requests it accepts. This
    // separates "the node cannot do sequences" from "it is unhappy with the request".
    for (name, code) in [
        ("Io0", spb_abi::IOCTL_SPB_LOCK_CONTROLLER),
        ("Io1", spb_abi::IOCTL_SPB_UNLOCK_CONTROLLER),
        ("Io2", spb_abi::IOCTL_SPB_EXECUTE_SEQUENCE),
        ("Io3", spb_abi::IOCTL_SPB_LOCK_CONNECTION),
        ("Io4", spb_abi::IOCTL_SPB_UNLOCK_CONNECTION),
        ("Io5", spb_abi::IOCTL_SPB_FULL_DUPLEX),
        ("Io6", spb_abi::IOCTL_SPB_MULTI_SPI_TRANSFER),
    ] {
        let status = bus.probe_ioctl(code);
        mark_device_value(device, name, status as u32);
    }

    // The same matrix on the Resource Hub node with the stock driver's access
    // mask: if the node now accepts sequences, that is the working path.
    let mut hub_bus = unsafe { SpbBus::open(device, peripheral_id, true) }.ok();
    let mut hub_working: Option<u8> = None;
    if let Some(hub) = hub_bus.as_mut() {
        for (variant, names) in [
            (0_u8, ("Hub0", "HubS0")),
            (1_u8, ("Hub1", "HubS1")),
            (2_u8, ("Hub2", "HubS2")),
            (3_u8, ("Hub3", "HubS3")),
            (4_u8, ("Hub4", "HubS4")),
            (5_u8, ("Hub5", "HubS5")),
        ] {
            hub.set_variant(variant);
            let result = hub.transact(ln8000::regs::DEVICE_ID, None);
            let status = hub.last_status() as u32;
            match result {
                Ok(value) => {
                    mark_device_value(device, names.0, 1);
                    mark_device_value(device, "HubValue", u32::from(value));
                    if value == ln8000::regs::DEVICE_ID_VALUE && hub_working.is_none() {
                        hub_working = Some(variant);
                    }
                }
                Err(_) => mark_device_value(device, names.0, 0),
            }
            mark_device_value(device, names.1, status);
        }
    } else {
        mark_device_value(device, "Hub0", 0xFFFF_FFFF);
    }

    // Iterating over resource-node identifiers: which connections the hub exposes
    // at all. Our node got identifier 1 - we check the neighbouring ones and the
    // ADC peripheral addresses from ACPI (0x131/0x135 = VADC/ADC_TM on SID2
    // PM8150B) to understand whether any of them carries a connection to the
    // charger registers.
    // We write the result of every step into the registry: otherwise it cannot be
    // seen from the device, and without an answer there is nothing to move on with.
    if hub_bus.is_some() {
        const CANDIDATES: [(u64, (&str, &str, &str)); 13] = [
            (2, ("Sc2Open", "Sc2St", "Sc2Val")),
            (3, ("Sc3Open", "Sc3St", "Sc3Val")),
            (4, ("Sc4Open", "Sc4St", "Sc4Val")),
            (5, ("Sc5Open", "Sc5St", "Sc5Val")),
            (6, ("Sc6Open", "Sc6St", "Sc6Val")),
            (8, ("Sc8Open", "Sc8St", "Sc8Val")),
            (16, ("Sc16Open", "Sc16St", "Sc16Val")),
            (56, ("Sc56Open", "Sc56St", "Sc56Val")),
            (0x13, ("Sc13Open", "Sc13St", "Sc13Val")),
            (0x31, ("Sc31Open", "Sc31St", "Sc31Val")),
            (0x131, ("Sc131Open", "Sc131St", "Sc131Val")),
            (0x135, ("Sc135Open", "Sc135St", "Sc135Val")),
            (0x213, ("Sc213Open", "Sc213St", "Sc213Val")),
        ];
        // 0x13/0x131/0x135 are peripheral IDs from ACPI, not RH ConnectionIds.
        for (candidate, names) in CANDIDATES {
            // SAFETY: passive level, the device has been created.
            match unsafe { SpbBus::open(device, candidate, true) } {
                Ok(mut probe) => {
                    mark_device_value(device, names.0, 1);
                    probe.set_variant(0);
                    let result = probe.transact(ln8000::regs::DEVICE_ID, None);
                    mark_device_value(device, names.1, probe.last_status() as u32);
                    match result {
                        Ok(value) => mark_device_value(device, names.2, u32::from(value)),
                        Err(_) => mark_device_value(device, names.2, 0xFFFF_FFFF),
                    }
                }
                Err(_) => {
                    mark_device_value(device, names.0, 0);
                    mark_device_value(device, names.1, 0xFFFF_FFFF);
                    mark_device_value(device, names.2, 0xFFFF_FFFF);
                }
            }
        }
    }

    // The working route to the pump is through the resource node: only it carries
    // the device address (0x51). The parent route accepts requests but does not
    // return data, so the pump is not identified on it. If the node is
    // unavailable, we stay on the previous route so as not to lose diagnostics.
    // The resource node is already open above; we do not open it again - a second
    // open does not succeed. We take the ready target, otherwise we stay on the
    // previous route so as not to lose diagnostics.
    let pump_bus = match hub_bus {
        Some(hub) => {
            mark_device_value(device, "PumpBusOpen", 1);
            hub
        }
        None => {
            mark_device_value(device, "PumpBusOpen", 0);
            bus
        }
    };
    // The probe iterated over request layouts and stopped on the last one.
    // Before working with the pump we restore the working variant: otherwise
    // identification goes out with a request known to be unsupported and fails.
    // SPMI bus access probe: the only unexplored road to the PMIC registers.
    // Such objects are not visible from user mode - we check from the kernel.
    // The codes are stock ones: 0 - opened, 0xC0000034 - no such object,
    // 0xC0000022 - access denied, 0xC0000001 - other failure.
    for (mark, name) in [
        ("SpmiProbeSuperuser", "\\Device\\Spmi\\SUPERUSER"),
        ("SpmiProbeSpmi", "\\Device\\Spmi"),
        ("SpmiProbeUpperName", "\\Device\\SPMI"),
        ("SpmiProbeLowerName", "\\Device\\spmi"),
        ("SpmiProbeArb", "\\Device\\SpmiArb"),
    ] {
        // SAFETY: the device has been created, the level is passive.
        let status = unsafe { crate::spb::probe_named_target(device, name, 0x001F_01FF) };
        mark_device_value(device, mark, status as u32);
    }

    // The SPMI bus object for the PMIC peripheral: it is opened by the
    // qcpmic8150, qcpmicext8150 and qcpmicgpio8150 drivers, so it exists in the
    // system. The previous probe returned a failure - we check whether the access
    // mask is to blame.
    for (mark, access) in [
        ("SpmiSuAcc0", 0x0000_0000_u32),
        ("SpmiSuAcc1", 0x0000_0001_u32),
        ("SpmiSuAcc2", 0x0000_0002_u32),
        ("SpmiSuAcc3", 0x0000_0003_u32),
        ("SpmiSuAcc4", 0x8000_0000_u32),
        ("SpmiSuAcc5", 0x4000_0000_u32),
        ("SpmiSuAcc6", 0xC000_0000_u32),
        ("SpmiSuAcc7", 0x0001_0000_u32),
    ] {
        // SAFETY: the device has been created, the level is passive.
        let status = unsafe {
            crate::spb::probe_named_target(device, "\\Device\\Spmi\\SUPERUSER", access)
        };
        mark_device_value(device, mark, status as u32);
    }

    // The next QC lever: SUPERUSER has already been refused with ShareAccess=0.
    // The stock qcpmic/qcADC hold their objects - we try FILE_SHARE_* and the
    // public symbolic names through which access to the PMIC/ADC works on this
    // platform. If any open returns 0, that is a channel to SID2 /
    // CMD_HVDCP_2 (0x1343).
    for (mark, name) in [
        ("PmicOpenQcompmic", "\\DosDevices\\Global\\QCOMPMIC"),
        ("PmicOpenBattmgr", "\\DosDevices\\Global\\QCOMBATTMGR"),
        ("PmicOpenPmictcc", "\\DosDevices\\Global\\QCOMPMICTCC"),
        ("PmicOpenPmicapps", "\\DosDevices\\Global\\QCOMPMICAPPS"),
        ("PmicOpenMiceic", "\\DosDevices\\Global\\QCOMPMICEIC"),
        ("PmicOpenBattmini", "\\DosDevices\\Global\\QCBatteryMiniclass"),
        ("AdcOpenQcomAdc", "\\??\\QCOM_ADC"),
        ("AdcOpenQcomAdc2", "\\??\\QCOM_ADC2"),
        ("AdcOpenQcomAdc3", "\\??\\QCOM_ADC3"),
        ("HubOpenBare", "\\Device\\RESOURCE_HUB"),
    ] {
        // SAFETY: the device has been created, the level is passive.
        let status = unsafe {
            crate::spb::probe_named_target_ex(
                device,
                name,
                0x001F_01FF,
                crate::spb::PROBE_SHARE_ALL,
            )
        };
        mark_device_value(device, mark, status as u32);
    }
    // SUPERUSER once more - with shared access (in case qcpmic already holds the object).
    // SAFETY: the device has been created, the level is passive.
    let su_share = unsafe {
        crate::spb::probe_named_target_ex(
            device,
            "\\Device\\Spmi\\SUPERUSER",
            0x001F_01FF,
            crate::spb::PROBE_SHARE_ALL,
        )
    };
    mark_device_value(device, "SpmiSuShare", su_share as u32);

    // If the SUPERUSER slot is free (<3 holders) - we read APSD_STATUS (0x1307).
    // SAFETY: passive level; the device has been created.
    let apsd = unsafe { crate::spb::probe_superuser_apsd(device) };
    mark_device_value(device, "SuOpen", apsd.open_status as u32);
    mark_device_value(device, "SuGrant", apsd.grant_status as u32);
    mark_device_value(device, "SuApsdSt", apsd.read_status as u32);
    mark_device_value(device, "SuApsdVal", u32::from(apsd.value));

    // If the ACPI overlay added SPMI USBIN (SID2 / 0x13) to PEIC - we try to read
    // APSD_STATUS (0x1307). Success (UsbinBeSt/UsbinLeSt = 0) = a path to
    // CMD_HVDCP_2. Without the overlay UsbinConn=0 and this block only writes
    // UsbinOpen=0xFFFFFFFF.
    // SAFETY: passive level; the device has been created.
    unsafe { probe_usbin_spmi(device, usbin_id) };
    // Gate for WS-C HVDCP: stock ACPI keeps `None`; negotiate prefers SUPERUSER.
    // SAFETY: prepare is serialized with IOCTL/timer by WDF.
    unsafe { state().usbin_id = usbin_id };

    let mut pump_bus = pump_bus;
    let pump_variant = hub_working.unwrap_or(0);
    pump_bus.set_variant(pump_variant);
    mark_device_value(device, "PumpVariant", u32::from(pump_variant));
    let mut pump = match Pump::open(pump_bus, config) {
        Ok(pump) => pump,
        Err(err) => {
            println!("ln8000-kmdf: LN8000 not identified: {err}");
            let st = unsafe { state() };
            st.last_error = -2;
            mark_device_value(device, "PumpOpen", 0);
            mark_device_value(device, "PumpFailStatus", 0xC000_0001);
            mark_stage(device, STAGE_PREPARE_CHIP, 0xC000_0001u32 as i32);
            return wdk_sys::STATUS_SUCCESS;
        }
    };
    if let Err(err) = pump.configure() {
        println!("ln8000-kmdf: configuration failed: {err}");
        let st = unsafe { state() };
        st.last_error = -3;
        mark_stage(device, STAGE_PREPARE_CONFIG, 0);
        return wdk_sys::STATUS_DEVICE_NOT_READY;
    }

    // 4. Do not force a charge mode here: Vin is usually still ~5 V before
    //    HVDCP, and a failed 2:1 attempt can latch VIN_OV. Mode is chosen after
    //    HVDCP / by the telemetry timer via Vin-aware `set_charging`.
    let _ = pump.standby();

    let st = unsafe { state() };
    mark_device_value(device, "PumpOpen", 1);
    st.pump = Some(pump);
    st.telemetry_ms = params.telemetry_ms;
    st.fcc_cfg_raw = params.fcc_raw;
    st.limits = guard_limits;

    // 4b. Autostart HVDCP via SUPERUSER (Usbin RH secondary if overlay present).
    try_autostart_hvdcp(device);

    // 4c. Publish GUID_DEVICE_BATTERY via BattC (tray / Settings SoC).
    // Xiaomi qcbattminiclass never enables the interface; we estimate SoC from VBAT.
    // SAFETY: FDO exists; PASSIVE_LEVEL.
    let batt_st = unsafe { battery::initialize(device) };
    if batt_st >= 0 {
        // Re-borrow after HVDCP (it also touches `state()`).
        let st = unsafe { state() };
        let mut vbat = 0i32;
        let mut vbus = 0i32;
        let mut iin = 0i32;
        let mut vac_unplug = false;
        // Suitability of the input samples for the power decision (see the
        // telemetry tick and `battery_policy::online_raw_with_evidence`).
        let mut input_readings_usable = false;
        // First ADC right after HVDCP can return 0; retry - a zero sample must
        // not pin tray SoC at 0% (see battery::update_from_telemetry).
        for _ in 0..3 {
            if let Some(pump) = st.pump.as_mut() {
                let vbat_read = pump.read_adc(AdcChannel::Vbat);
                let vbus_read = pump.read_adc(AdcChannel::Vin);
                vbat = vbat_read.unwrap_or_default();
                vbus = vbus_read.unwrap_or_default();
                iin = pump.read_adc(AdcChannel::Iin).unwrap_or_default();
                // A zero channel means "no sample", not "0 V": a bus failure
                // gives a zero, and a sleeping ADC reads successfully but also
                // returns zero.
                input_readings_usable =
                    vbat_read.is_ok() && vbus_read.is_ok() && vbat > 0 && vbus > 0;
                // The same hardware flag as in the telemetry tick: without it the
                // reflected `2 · VBAT` after the brick is unplugged would again
                // pass for a live input on this publication path.
                vac_unplug = pump
                    .read_register(ln8000::regs::FAULT1_STS)
                    .is_ok_and(|f1| f1 & ln8000::regs::FAULT1_VAC_UNPLUG != 0);
            }
            if vbat > 0 {
                break;
            }
        }
        unsafe {
            battery::update_from_telemetry(
                u32::try_from(vbat.max(0)).unwrap_or(0),
                u32::try_from(vbus.max(0)).unwrap_or(0),
                u32::try_from(iin.max(0)).unwrap_or(0),
                // The peak window has not started yet: the single sample is the peak.
                st.max_iin_ua
                    .max(u32::try_from(iin.max(0)).unwrap_or(0)),
                vac_unplug,
                input_readings_usable,
                battery::soc_rising(),
                monotonic_ms(),
            );
        }
        mark_device_value(device, "BattPct", battery::last_percent());
        mark_device_value(device, "BattVbat", u32::try_from(vbat.max(0)).unwrap_or(0) / 1000);
        mark_device_value(device, "BattPwr", battery::last_power_state());
    }

    // 4d. Hold the system in S0 while the device works: the screen turns off on
    //     its own timeout, but Connected Standby does not set in (see `sysreq`).
    //     Without this the tablet died on idle after 4-12 minutes
    //     (`Kernel-Power 41`, `BugcheckCode=0`, `ConnectedStandbyInProgress=true`).
    // SAFETY: prepare-hardware runs at PASSIVE_LEVEL, the FDO exists.
    let req_st = unsafe { sysreq::acquire(device) };
    mark_device_value(device, "SysReqSt", req_st as u32);
    mark_device_value(device, "SysReqOk", u32::from(req_st >= 0));

    // 5. Start telemetry with the period from the registry (one-shot + re-arm).
    arm_telemetry_timer();
    // The hardware is ready: from this tick the counter may be polled. The flag is
    // set after the bus and before the first tick - `read_gauge_if_due` runs
    // **before** the mutex, that is it is not protected against a race with
    // `evt_release_hardware`.
    GAUGE_READY.store(true, Ordering::Release);

    mark_stage(device, STAGE_READY, 0);
    wdk_sys::STATUS_SUCCESS
}

/// Start / re-arm the telemetry one-shot from `st.telemetry_ms`.
///
/// WDF locks `Period` at `WdfTimerCreate`; a non-zero Period of 1000 ms used to
/// override registry `TelemetryMs=250` after the first tick.
fn arm_telemetry_timer() {
    let timer = unsafe { TIMER };
    if timer.is_null() {
        return;
    }
    let st = unsafe { state() };
    let period = i64::from(st.telemetry_ms.max(TELEMETRY_MS_MIN));
    unsafe {
        let _ = call_unsafe_wdf_function_binding!(WdfTimerStart, timer, -10_000_i64 * period);
    }
}

/// Short state code "is there input and mode" for the `EngageState` mark.
///
/// 0 - no input, 1 - standby, 2 - bypass, 3 - switching,
/// 4 - input is elevated but 2:1 does not pass physically (`Vin < 2*Vbat + 250 mV`),
/// and the 1:1 bypass is forbidden at such an input.
fn engage_state(input_present: bool, vin_uv: i32, vbat_uv: u32, mode: OpMode) -> u32 {
    if !input_present {
        return ENGAGE_NO_INPUT;
    }
    if vin_uv >= SWITCHING_MIN_VIN_UV && charge_mode(vin_uv, vbat_uv).is_none() {
        return ENGAGE_NO_HEADROOM;
    }
    match mode {
        OpMode::Switching => ENGAGE_SWITCHING,
        OpMode::Bypass => ENGAGE_BYPASS,
        _ => ENGAGE_STANDBY,
    }
}

/// Writes the marks of a single charge-enable attempt: `ChargeAttemptN`,
/// `LastEnableErr` (0 = success) and `LastEnableMs` (monotonic milliseconds).
fn mark_charge_attempt(
    device: WDFDEVICE,
    attempts: u32,
    now_ms: u64,
    result: &Result<OpMode, PumpError>,
) {
    let err = match result {
        Ok(_) => 0,
        Err(err) => pump_error_code(*err),
    };
    mark_device_value(device, "ChargeAttemptN", attempts);
    mark_device_value(device, "LastEnableErr", err as u32);
    mark_device_value(
        device,
        "LastEnableMs",
        u32::try_from(now_ms).unwrap_or(u32::MAX),
    );
}

/// Writes the marks of the effective protection profile: `ProfLoops` and `ProfSel`.
///
/// `ProfLoops` is 1 if the pump regulation loops are enabled in the resulting
/// profile (both `V_FLOAT` and `IIN`); otherwise 0: then the battery voltage is
/// limited by nothing but the hardware `VBAT_OV`, and that must be visible in the
/// post-mortem. `ProfSel` is the applied `ProtectionProfile` value from the
/// registry (`PROFILE_NOT_SET` - there was no parameter, the code profile stayed).
fn mark_profile(device: WDFDEVICE, config: &PumpConfig, selected: Option<u32>) {
    let loops = u32::from(!config.vbat_reg_disabled && !config.iin_reg_disabled);
    mark_device_value(device, "ProfLoops", loops);
    mark_device_value(device, "ProfSel", selected.unwrap_or(PROFILE_NOT_SET));
}

/// Outcome of [`recover_ln_shutdown`].
enum ShutdownRecovery {
    /// The chip was not in shutdown - there is nothing to recover.
    NotInShutdown,
    /// The chip was rebuilt (`soft_reset` + `configure`) and immediately got a
    /// `set_charging(true)` attempt; it holds that attempt's result.
    Recovered(Result<OpMode, PumpError>),
}

/// POR pause after `soft_reset`.
///
/// The reset starts a POR, and before [`regs::SOFT_RESET_DELAY_MS`] any I²C
/// exchange hangs the chip (a live hang was caught on this). The pause is passed
/// into `set_charging` so that the five-volt recovery path (`soft_reset` →
/// `configure`) does not touch the bus ahead of time.
fn por_delay() {
    let delay = u32::try_from(regs::SOFT_RESET_DELAY_MS)
        .unwrap_or(10)
        .saturating_mul(2);
    hvdcp::sleep_ms(delay);
}

/// Exit LN8000 hardware SHUTDOWN (`SYS_STS` bit0).
///
/// Soft-reset triggers POR: do not touch I²C until
/// [`regs::SOFT_RESET_DELAY_MS`] (live hang was verify-read during POR).
///
/// `configure()` leaves the chip in standby, so the charge attempt is made right
/// here: otherwise the caller would burn a whole `CHARGE_RETRY_MS` cooldown
/// before the next tick could set a mode.
fn recover_ln_shutdown(pump: &mut Pump<SpbBus>) -> ShutdownRecovery {
    let sys = match pump.read_register(regs::SYS_STS) {
        Ok(v) => v,
        Err(_) => return ShutdownRecovery::NotInShutdown,
    };
    if sys & regs::SYS_STS_SHUTDOWN == 0 {
        return ShutdownRecovery::NotInShutdown;
    }
    println!("ln8000-kmdf: SYS_STS=0x{sys:02X} shutdown - soft_reset + reconfigure");
    let _ = pump.soft_reset();
    por_delay();
    let _ = pump.configure();
    ShutdownRecovery::Recovered(pump.set_charging(true, &mut por_delay))
}

/// Stops telemetry and puts the device into a safe state.
///
/// # Safety
///
/// Called by WDF when the device is removed.
unsafe extern "C" fn evt_release_hardware(
    _device: WDFDEVICE,
    _resources_translated: WDFCMRESLIST,
) -> NTSTATUS {
    // We no longer poll the counter: `read_gauge_if_due` runs before the mutex and
    // would not see that the bus is about to go away from under it. The flag is
    // cleared before stopping the timer so that an already started read stays the
    // only one.
    GAUGE_READY.store(false, Ordering::Release);
    // We take the mutex before stopping the timer: `WdfTimerStop` with a zero does
    // not wait for the current call, so it could work with `STATE` at the same
    // time as we do.
    let _state = lock_state();
    // SAFETY: `STATE` and the bus are protected by the state mutex.
    let timer = unsafe { TIMER };
    if !timer.is_null() {
        unsafe {
            let _ = call_unsafe_wdf_function_binding!(WdfTimerStop, timer, 0);
        }
    }
    // SAFETY: detach BattC before tearing down the FDO path.
    unsafe { battery::unload() };
    // SAFETY: the power request is ours and has not been released yet; we release
    // it together with the device.
    unsafe { sysreq::release() };
    // SAFETY: see the invariants of `DriverState`.
    let st = unsafe { state() };
    if let Some(pump) = st.pump.as_mut() {
        if let Err(err) = pump.standby() {
            println!("ln8000-kmdf: failed to go to standby: {err}");
        }
        pump.close();
    }
    st.pump = None;
    st.hvdcp_retry_pending = false;
    st.hvdcp_edge_armed = false;
    st.last_input_present = false;
    // SAFETY: single-device lifetime ends with release.
    unsafe {
        DEVICE = core::ptr::null_mut();
    }
    wdk_sys::STATUS_SUCCESS
}

/// Writes the SMB5 buck FCC and publishes the write outcome; `true` - the
/// requested value really ended up in the register.
///
/// A separate function because there are two write paths (raise and return) but
/// one check: comparison with the read-back. A write without a cross-check would
/// repeat the `BypassPorSpent` story - a silent failure would look like success.
/// `ChgrFccErr` is cleared with a zero on success, otherwise the previous failure
/// would hang in the journal until a reboot.
///
/// # Safety
///
/// `PASSIVE_LEVEL`; `device` is alive.
unsafe fn write_fcc_and_publish(device: WDFDEVICE, want: u8) -> bool {
    // SAFETY: passive level; `device` is alive (checked by the caller).
    match unsafe { crate::spb::write_fcc_raw(device, want) } {
        Some(written) => {
            mark_device_value(device, "ChgrFccSet", u32::from(written));
            if written == want {
                mark_device_value(device, "ChgrFccErr", 0);
                println!(
                    "ln8000-kmdf: buck FCC = {} x 50 mA = {} mA",
                    written,
                    u32::from(written).saturating_mul(50)
                );
                true
            } else {
                // The chip accepted the write but kept its own value: the setpoint
                // is above its current ceiling (taper, JEITA) or the peripheral is
                // not exposed.
                mark_device_value(device, "ChgrFccErr", 1);
                println!(
                    "ln8000-kmdf: buck FCC did not take: asked for {want}, register holds {written}"
                );
                false
            }
        }
        None => {
            mark_device_value(device, "ChgrFccErr", 1);
            println!("ln8000-kmdf: buck FCC write (0x1061 = {want}) failed");
            false
        }
    }
}

/// Periodic telemetry collection, journalling and protection.
///
/// # Safety
///
/// Called by WDF from the timer at passive level.
unsafe extern "C" fn evt_telemetry_timer(_timer: WDFTIMER) {
    // The fuel counter is read before the mutex: one SUPERUSER transaction takes
    // about two seconds, and both charge control and `IOCTL_STATUS` sit under the
    // mutex. While polling ran under the mutex every 250 ms, a third-party
    // SUPERUSER reader could not open the bus in 79 attempts out of 100.
    // SAFETY: passive level, we do not touch the state.
    let gauge = unsafe { read_gauge_if_due() };
    // The timer runs in its own context, the WDF queue does not serialize it: the
    // bus and `STATE` are protected by the state mutex.
    let _state = lock_state();
    // Pump borrow must end before HVDCP reopen (retry / re-plug).
    let (want_replug, want_retry, want_reelevate) = {
    // SAFETY: access is serialized by WDF (automatic timer serialization).
    let st = unsafe { state() };
    let Some(pump) = st.pump.as_mut() else {
        // Hardware released - do not re-arm (ReleaseHardware already stopped us).
        return;
    };

    let status = match pump.status() {
        Ok(status) => status,
        Err(err) => {
            st.last_error = -10;
            println!("ln8000-kmdf: status unavailable: {err}");
            arm_telemetry_timer();
            return;
        }
    };

    // A failed channel read gives a zero that is indistinguishable from a real
    // zero, so validity travels in the sample as separate flags: protection has
    // no right to cut the current or return it to the profile based on garbage.
    // The flag is only the result of the bus read; the plausibility of the value
    // is checked by protection itself (`guard::die_temp_usable`: a zero raw ADC
    // code decodes to +160,0 °C and is cut off by `DIE_TEMP_MAX_PLAUSIBLE_DC`).
    let vbat_read = pump.read_adc(AdcChannel::Vbat);
    let vbus_read = pump.read_adc(AdcChannel::Vin);
    let iin_read = pump.read_adc(AdcChannel::Iin);
    let temp_read = pump.read_adc(AdcChannel::DieTemp);
    let vbat = vbat_read.unwrap_or_default();
    let vbus = vbus_read.unwrap_or_default();
    let iin = iin_read.unwrap_or_default();
    let temp = temp_read.unwrap_or_default();

    let vbat_uv = u32::try_from(vbat.max(0)).unwrap_or(0);
    let vbus_uv = u32::try_from(vbus.max(0)).unwrap_or(0);
    let iin_ua = u32::try_from(iin.max(0)).unwrap_or(0);
    // `FAULT1` bit 4 (`LN8000_MASK_VAC_UNPLUG_STS`, vendor
    // `ln8000_charger.h:62`): the hardware itself reports that the brick is
    // unplugged. The vendor answers the question "is VBUS present" with this bit
    // (`POWER_SUPPLY_PROP_TI_VBUS_PRESENT` in `ln8000_charger.c`), so the flag
    // does not need to be derived from the ADC.
    let vac_unplug = status.fault1_sts & ln8000::regs::FAULT1_VAC_UNPLUG != 0;
    // With the cable unplugged the VIN node is not loaded, and the ADC reads
    // `2 · VBAT` (live measurement 19.09: `Vin = 8,80 V` with a cell at
    // `4,40 V`, current at the floor). By raw `vbus > 0` such an input looked
    // live, and protection, sessions and HVDCP depend on this flag. Current and
    // an active mode override it: they mean the input really works.
    let phantom_input = iin_ua < hvdcp::IIN_DEAD_FLOOR_UA
        && status.op_mode != OpMode::Switching
        && status.op_mode != OpMode::Bypass
        && (vac_unplug || ln8000::encoding::vin_is_doubled_vbat(vbat_uv, vbus_uv));
    let sample = TelemetrySample {
        ts_ms: monotonic_ms(),
        vbat_uv,
        vbus_uv,
        iin_ua,
        die_temp_dc: temp,
        op_mode: status.op_mode,
        input_present: !status.has_critical_fault() && vbus_uv > 0 && !phantom_input,
        vbat_valid: ln8000::encoding::vbat_reading_usable(vbat_read.is_ok(), vbat_uv),
        die_temp_valid: temp_read.is_ok(),
    };
    st.telemetry.push(sample);

    // Whether the input samples are good enough for the power decision. A zero
    // here means "no sample", not "0 V": a read failure collapses to zero above,
    // and the ADC goes into auto-hibernation after ~4 s of idle and then reads
    // successfully but with `0x00` in all channels
    // (`encoding::vbat_reading_usable`). Without this flag 4 s of pump idle plus
    // the 8 s `ONLINE_HOLD_MS` window made Windows switch AC→DC→AC with the cable
    // motionless - that is, a backlight policy reset via Kernel-Power 105.
    let input_readings_usable = sample.vbat_valid && vbus_read.is_ok() && sample.vbus_uv > 0;

    // A sleeping ADC cannot see the brick that has just arrived, and nothing else
    // here wakes it: `ADC_CTRL` is written only inside `Pump::configure()`, which
    // runs at device start and on recovery paths that need a live sample to be
    // reached at all. The hardware's VBUS comparator keeps working while the ADC
    // sleeps, so the tick uses it: VBUS present with unusable samples means the
    // chip is in `AutoHibernate` - wake it and the next tick carries real samples.
    // VBUS gone means there is nothing to measure, so it may sleep again and keep
    // its idle current.
    match battery_policy::adc_wake_needed(input_readings_usable, vac_unplug) {
        Some(true) if !st.adc_awake => match pump.set_adc_mode(AdcMode::Normal) {
            Ok(()) => {
                st.adc_awake = true;
                st.adc_wake_n = st.adc_wake_n.saturating_add(1);
            }
            Err(err) => println!("ln8000-kmdf: ADC wake failed: {err}"),
        },
        Some(false) if st.adc_awake => {
            if pump.set_adc_mode(AdcMode::AutoHibernate).is_ok() {
                st.adc_awake = false;
            }
        }
        _ => {}
    }

    // The fuel counter's direction for this tick: the pack's own SOC rose recently, which
    // is the only charging evidence available while the pump is idle and the platform buck
    // carries the charge. It is read from `battery.rs`, where the SOC sample arrives.
    // SAFETY: BattC status notify is DISPATCH-safe; we run at PASSIVE.
    unsafe {
        battery::update_from_telemetry(
            sample.vbat_uv,
            sample.vbus_uv,
            sample.iin_ua,
            st.max_iin_ua,
            vac_unplug,
            input_readings_usable,
            battery::soc_rising(),
            sample.ts_ms,
        );
    }
    let device = unsafe { DEVICE };
    if !device.is_null() {
        mark_device_value(device, "BattPwr", battery::last_power_state());
        // Live Vin/Iin/mode for operator scripts (they read `SuVinUv`, `SuIin`,
        // `SuMode`); previously nobody wrote these names.
        mark_device_value(device, "SuVinUv", sample.vbus_uv);
        mark_device_value(device, "SuIin", sample.iin_ua);
        mark_device_value(device, "SuMode", u32::from(status.op_mode.code()));
        // Bit mask of channel validity (0 - all read): in a dump a read failure
        // no longer looks like a real zero. Bit 0 also covers ADC hibernation: a
        // sleeping chip reads successfully but returns zero, and zero volts on a
        // live cell is impossible (`encoding::vbat_reading_usable`).
        // bit 0 - VBAT, bit 1 - DieTemp, bit 2 - Iin, bit 3 - Vin.
        mark_device_value(
            device,
            "AdcValid",
            u32::from(!sample.vbat_valid)
                | (u32::from(!sample.die_temp_valid) << 1)
                | (u32::from(iin_read.is_err()) << 2)
                | (u32::from(vbus_read.is_err()) << 3),
        );
        // Suitability of the input samples for the power decision - the input of
        // the `online_raw_with_evidence` branch: the mark shows whether the
        // ordinary path ran on this tick or "no sample, but the hardware says
        // VBUS is present".
        mark_device_value(device, "InputUsable", u32::from(input_readings_usable));
        // The ADC's power mode and the count of comparator wakes: a sleeping ADC
        // reads zeros and makes an insertion invisible, so a dump has to show both
        // whether the chip is awake and whether the wake path has ever run.
        mark_device_value(device, "AdcAwake", u32::from(st.adc_awake));
        mark_device_value(device, "AdcWakeN", st.adc_wake_n);
        // Why the AC verdict came out the way it did. Below 6 V `online_raw` can only
        // reject a live adapter through the floor, the doubled-VBUS veto or the headroom
        // over the pack, and all three read `vbat_uv` - which is the ADC's VBAT channel,
        // a converter node rather than the cell (it reads `Vin / 2` in switching). So a
        // dump has to carry the value the tick actually used next to the two derived
        // tests, or a rejection cannot be told from a sleep.
        mark_device_value(device, "VbatTickMv", sample.vbat_uv / 1000);
        mark_device_value(
            device,
            "DoubledVeto",
            u32::from(ln8000::encoding::vin_is_doubled_vbat(
                sample.vbat_uv,
                sample.vbus_uv,
            )),
        );
        // The raw verdict before any hold: 1 online, 0 offline, 2 no evidence (no
        // usable sample and the hardware did not say VBUS was gone).
        mark_device_value(
            device,
            "OnlineRaw",
            match battery_policy::online_raw_with_evidence(
                sample.vbus_uv,
                sample.vbat_uv,
                sample.iin_ua,
                vac_unplug,
                input_readings_usable,
            ) {
                None => 2,
                Some(false) => 0,
                Some(true) => 1,
            },
        );
        // The fuel counter's direction and the age of the last rise. It is the only
        // charging evidence while the pump is idle and the platform buck carries the
        // charge, so a dark charging flag has to be distinguishable from a counter that
        // is simply not answering - this pair is what says which of the two happened.
        mark_device_value(device, "SocRising", u32::from(battery::soc_rising()));
        mark_device_value(
            device,
            "SocRiseAgeMs",
            u32::try_from(
                sample
                    .ts_ms
                    .saturating_sub(battery::soc_last_rise_ms()),
            )
            .unwrap_or(u32::MAX),
        );
        // Current peak over the observation window: we write it once per
        // IIN_WINDOW_MS and start a new window, so that afterwards it is visible
        // whether the driver drew current at all.
        if sample.iin_ua > st.max_iin_ua {
            st.max_iin_ua = sample.iin_ua;
        }
        if st.iin_window_start_ms == 0
            || sample.ts_ms.saturating_sub(st.iin_window_start_ms) >= IIN_WINDOW_MS
        {
            mark_device_value(device, "MaxIinUa", st.max_iin_ua);
            st.max_iin_ua = sample.iin_ua;
            st.iin_window_start_ms = sample.ts_ms;
        }
    }

    // The PM8150B fuel counter: the only trustworthy source of the percentage. A
    // linear estimate from VBAT on this cell lied in both directions - on a
    // connected brick it gave 100 %, and where the counter says 6 %, it gave 25 %.
    // The read itself was done above, before the mutex (`read_gauge_if_due`); here
    // it is only publication. A failure spoils nothing: the previous value stays
    // (and if the counter never answered, the cell estimate, `SocSrc=2`). The
    // failure counter lives in the battery module: here `pump` holds a mutable
    // reference to the state, and it must not be taken a second time.
    // "The pump is carrying current": active 2:1 and a transfer above the ADC
    // floor. The predicate is lifted here, to the mark publication, because the
    // fate of the buck FCC (below) is decided by it as well: raising the buck
    // limit only makes sense while the pump really transfers. The mode choice
    // below uses the same quantity.
    let pump_alive = status.op_mode == OpMode::Switching
        && sample.iin_ua > hvdcp::IIN_DEAD_FLOOR_UA;
    let device = unsafe { DEVICE };
    if !device.is_null() {
        match gauge {
            GaugePoll::Raw(raw) => {
                unsafe { battery::set_gauge_raw(raw, sample.ts_ms) };
                mark_device_value(device, "SocRaw", u32::from(raw));
            }
            GaugePoll::Failed => {
                let fails = unsafe { battery::note_gauge_failure() };
                mark_device_value(device, "SocFail", fails);
            }
            GaugePoll::Skipped => {}
        }
        mark_device_value(device, "SocSrc", battery::last_soc_source());
        mark_device_value(device, "BattPct", battery::last_percent());
        // Live cell voltage from the chip, not a sample taken once at device
        // start: the previous entry lived only in prepare-hardware and in the
        // IOCTL, so the mark froze at the value of the first tick (live
        // measurement 19.09 12:0x: `BattVbat` 4 010 mV against a live 4 230 mV)
        // and spoiled the analysis of the transfer band, which is computed from
        // `vbat`. During 2:1 the sample comes from the middle of the converter
        // bus (≈ Vin/2), not from the cell - the charge is taken from the
        // PM8150B counter (`BattPct`).
        mark_device_value(device, "BattVbat", sample.vbat_uv / 1000);
        mark_device_value(device, "BattPwr", battery::last_power_state());
        // Age of the last successful read: it shows that the counter is not
        // stuck but polled rarely - once per `GAUGE_POLL_MS`.
        let gauge_last = GAUGE_LAST_MS.load(Ordering::Acquire);
        let age_ms = u32::try_from(sample.ts_ms.saturating_sub(gauge_last)).unwrap_or(u32::MAX);
        mark_device_value(device, "SocAgeMs", age_ms);
        mark_device_value(device, "GaugePollMs", GAUGE_POLL_MS);
        // The raw failure bytes are published as they are: the chip has a group
        // flag for "voltage" failures (`FAULT1` bits 6:0) that the vendor tests as
        // a whole, while only five named bits exist in it. The live
        // `FAULT1=0x21` is two nameless bits of that group: `has_critical_fault`
        // stays silent about them, while the vendor's `volt_qual` says "input
        // unusable". Without these marks the difference between "no failures" and
        // "input unusable" is invisible.
        mark_device_value(device, "Fault1Sts", u32::from(status.fault1_sts));
        mark_device_value(device, "Fault2Sts", u32::from(status.fault2_sts));
        mark_device_value(device, "SysSts", u32::from(status.sys_sts));
        // The same byte under a second name: the loop bits are not visible in the
        // decoded `OpMode` - bit7 `IIN_LOOP` and bit6 `VFLOAT_LOOP` - and it is
        // exactly they that tell whether the pump has hit the input current limit
        // or already the voltage ceiling, and whether it hangs without a loop at
        // all.
        mark_device_value(device, "SysStsRaw", u32::from(status.sys_sts));
        mark_device_value(device, "SafetySts", u32::from(status.safety_sts));
        // The vendor's second stage is only counted with charging enabled;
        // "enabled" means the operating mode accepted by the chip, not the
        // requested one.
        let charging = matches!(status.op_mode, OpMode::Switching | OpMode::Bypass);
        mark_device_value(device, "VoltQual", u32::from(status.volt_qual(charging)));
        // Which stage of the five-volt fallback worked: `1` - a clean write (what
        // worked on 17.09), `2` - a POR, `3` - the failure mask. This mark shows
        // whether the mode lock is cured by clearing the latch or the input is
        // really unusable.
        mark_device_value(device, "BypassStage", u32::from(pump.bypass_stage()));
        // `1` - the POR budget of this input has already been spent: the driver hit
        // a failure and is waiting for the brick to be changed. The mark shows that
        // the repeated ticks do not pull the chip.
        mark_device_value(device, "BypassPorSpent", u32::from(pump.por_spent()));

        // The platform's second charging branch - the SMB5 buck (PM8150B) - and the
        // cell current from the fuel counter. Before these marks it was impossible
        // to see either the FCC the firmware left or the real current into the cell:
        // the pump measures only its own input, and 2:1 by itself does not say where
        // the transfer went. Once per `GAUGE_POLL_MS`: a snapshot is a SUPERUSER
        // open, three grants, seven single-byte reads and three register pairs (cell
        // current and voltage), and on every telemetry tick (250 ms) such a load on
        // the bus is unacceptable - with the counter polled once per 250 ms a
        // third-party SUPERUSER reader was refused in 79 attempts out of 100
        // (measurement 18.09).
        // A read failure does not drown in zeros: it is visible via `ChgrErr`, and
        // the values themselves are then `0xFFFFFFFF`. For `FgIbatUa` that value is
        // unreachable: one bit weighs 488 µA, and the counter cannot deliver a
        // current of 1 µA.
        if st.last_chgr_mark_ms == 0
            || sample.ts_ms.saturating_sub(st.last_chgr_mark_ms) >= u64::from(GAUGE_POLL_MS)
        {
            // The mark is set on failure too: otherwise an unavailable peripheral
            // would be polled on every tick.
            st.last_chgr_mark_ms = sample.ts_ms;
            // SAFETY: passive level; `device` is alive (checked above).
            match unsafe { crate::spb::read_charge_regs(device) } {
                Some(regs) => {
                    // The factory FCC value is captured once: it is exactly what
                    // must be returned after the pump. A raw zero means "not
                    // captured yet", not "zero amperes" - on this board the
                    // firmware leaves 30 (live measurement 19.09), and a zero in
                    // this role shows that the limit must not be raised: there
                    // would be nothing to return the buck to (see
                    // `DriverState::fcc_boot_raw`).
                    if st.fcc_boot_raw == 0 {
                        st.fcc_boot_raw = regs.fcc_raw;
                    }
                    mark_device_value(device, "ChgrFccRaw", u32::from(regs.fcc_raw));
                    mark_device_value(device, "ChgrEn", u32::from(regs.charge_enable));
                    mark_device_value(device, "ChgrInhibit", u32::from(regs.inhibit));
                    mark_device_value(device, "ChgrStatus", u32::from(regs.chgr_status));
                    mark_device_value(device, "ChgrFvRaw", u32::from(regs.fv_raw));
                    mark_device_value(device, "ChgrIclRaw", u32::from(regs.icl_raw));
                    mark_device_value(device, "ChgrAllow", u32::from(regs.usbin_allow));
                    mark_device_value(device, "FgIbatUa", regs.ibatt_ua);
                    // Two cell voltages side by side, in one snapshot - that is the
                    // whole point of the build: their divergence tests the
                    // hypothesis that our `vbat` is the wrong one.
                    //
                    // The 2:1 transfer band is computed from `vbat`
                    // (`ln8000::encoding::window_target_uv`: `2·vbat + {200,300,400} mV`),
                    // so an error in `vbat` moves the whole band. Live measurement
                    // 19.09: the bus sits at 8,672 V, `SuMode = 3`, `SysStsRaw =
                    // 0x04`, while the input draws only 0,787 A - even though the
                    // same brick at 9,136 V delivered 1,25 A. The conclusion "we are
                    // at the floor of the band" was drawn from the number taken from
                    // `AdcChannel::Vbat`, and the LN8000 itself has a caveat (see the
                    // comment at `BattVbat` above, the line about 2:1): during 2:1
                    // this channel reads the middle of the converter bus
                    // (≈ Vin/2), not the cell. If so, `vbat` is understated roughly
                    // by half, `2·vbat` too, and the band is computed for a place
                    // where the bus is not.
                    //
                    // `FgVbattMv` is an independent sample: the PM8150B fuel
                    // counter, the `0x41A0`/`0x41A6` pair, the same SPMI path as the
                    // cell current, in no way connected to the pump ADC.
                    // `VbatAdcMv` is that very number from which the band is
                    // computed (`sample.vbat_uv / 1000`; under the name `BattVbat`
                    // it is published above and stays there - it is repeated here so
                    // that both samples are read from a single journal record and do
                    // not drift apart across ticks). They agree - the hypothesis is
                    // dismissed; they diverge twofold - the band is computed from
                    // the middle of the bus, and it must be moved by `FgVbattMv`.
                    mark_device_value(device, "FgVbattMv", regs.fg_vbatt_uv / 1000);
                    mark_device_value(device, "VbatAdcMv", sample.vbat_uv / 1000);
                    mark_device_value(device, "ChgrErr", 0);

                    // The one-shot buck FCC write probe - the only way to find out
                    // whether the CHGR peripheral accepts a write at all. The live
                    // measurement on 19.09 (92 % cell, 4,375 V, `ChgrFccRaw = 30`)
                    // lies above the policy gate below (`RE_ELEVATE_MAX_VBAT_UV`,
                    // 4,35 V), and the pump is not carrying current: no raise
                    // condition holds, and the write path would never execute. A
                    // silent refusal by the peripheral would then look like "the
                    // limit was raised but the current did not grow". This cannot be
                    // checked from the outside: `\Device\Spmi\SUPERUSER` is a
                    // kernel-level name, it has no user symbolic link, and opening
                    // it from user mode gives `STATUS_OBJECT_PATH_NOT_FOUND`.
                    //
                    // The probe goes strictly downwards - [`FCC_PROBE_UA`] (raw 20)
                    // against the factory 30 - and immediately returns the factory
                    // value: lowering the limit cannot raise the current into the
                    // cell, and at 92 % the buck is driven by the taper, so for the
                    // fractions of a second while the setpoint stands it does not
                    // limit the transfer. The outcome stays in the journal
                    // (`ChgrProbeWrote`/`ChgrProbeRestored`/`ChgrProbeOk`/`ChgrProbeErr`),
                    // and the [`DriverState::fcc_probe_done`] flag stops the repeat:
                    // a write failure must not repeat on every tick.
                    //
                    // One tick - one owner: if the register is held by the policy
                    // (the limit is already raised or will be raised further down in
                    // the code), the probe yields and only marks that. A broken SPMI
                    // snapshot never reaches here at all - the whole block lives in
                    // the successful `read_charge_regs` branch - and a critical chip
                    // failure does not waste the one-shot chance: neither the probe
                    // nor the policy writes into an overheated or overvolted chip.
                    if !st.fcc_probe_done
                        && st.fcc_boot_raw != 0
                        && !status.has_critical_fault()
                    {
                        // The policy holds the register if the pump is carrying
                        // current below the gate or the limit has already been
                        // raised by its hand - then the probe has no right to write:
                        // its write would wipe out the raise. The pair of conditions
                        // here is exactly the same as in the raise itself below, so
                        // a policy that is "on and waiting" does not let the probe
                        // through: without `fcc_raised` the second term holds it. A
                        // disabled policy (`FccRaw` unset or 0) cannot hold the
                        // register, but yielding still happens - the second term
                        // knows nothing about the parameter, and the probe will wait
                        // for the first tick where the pump carries no current or
                        // the cell is above the gate.
                        let policy_owns = st.fcc_raised
                            || (pump_alive && sample.vbat_uv < RE_ELEVATE_MAX_VBAT_UV);
                        // The [`FCC_WRITE_MAX_UA`] ceiling was applied inside `raw_for`.
                        let probe_raw = raw_for(FCC_PROBE_UA);
                        if policy_owns {
                            mark_device_value(device, "ChgrProbeOk", 3);
                            st.fcc_probe_done = true;
                        } else if probe_raw >= st.fcc_boot_raw {
                            // The probe is clamped by skipping, not by `min`: a
                            // setpoint equal to the factory value proves nothing,
                            // and the probe has no right to be above the factory
                            // value under any conditions.
                            mark_device_value(device, "ChgrProbeOk", 2);
                            st.fcc_probe_done = true;
                        } else {
                            // SAFETY: passive level; `device` is alive.
                            let wrote = unsafe { crate::spb::write_fcc_raw(device, probe_raw) };
                            // The restore is written even if the probe returned
                            // `None`: the write may have landed in the register and
                            // not read back, and the factory value must be in the
                            // register in any case.
                            // SAFETY: passive level; `device` is alive.
                            let restored =
                                unsafe { crate::spb::write_fcc_raw(device, st.fcc_boot_raw) };
                            // The read-back is given to the journal as it is, and an
                            // unavailable one as `0xFFFFFFFF`, as for the other SMB5
                            // marks: values of this byte do not reach such a
                            // magnitude.
                            let wrote_marks = wrote.map_or(u32::MAX, u32::from);
                            let restored_marks = restored.map_or(u32::MAX, u32::from);
                            // `ChgrProbeErr` - either of the two writes broke
                            // (session open, grant, the write itself or the
                            // read-back); a zero means both went through whole.
                            let probe_err = u32::from(wrote.is_none() || restored.is_none());
                            // `ChgrProbeOk`: `1` - the read-back returned exactly
                            // what was asked (20), `0` - the chip kept its own value
                            // or the write did not go through at all.
                            let probe_ok = u32::from(wrote == Some(probe_raw));
                            mark_device_value(device, "ChgrProbeWrote", wrote_marks);
                            mark_device_value(device, "ChgrProbeRestored", restored_marks);
                            mark_device_value(device, "ChgrProbeErr", probe_err);
                            mark_device_value(device, "ChgrProbeOk", probe_ok);
                            // The probe touches neither `fcc_boot_raw` nor
                            // `fcc_raised`: the factory value stays what was read at
                            // capture time, and the policy will return it as usual. A
                            // broken restore leaves the register at 20 - below the
                            // factory value, that is safe, and the first successful
                            // policy write returns it to 30.
                            st.fcc_probe_done = true;
                        }
                    }
                }
                None => {
                    for name in [
                        "ChgrFccRaw",
                        "ChgrEn",
                        "ChgrInhibit",
                        "ChgrStatus",
                        "ChgrFvRaw",
                        "ChgrIclRaw",
                        "ChgrAllow",
                        "FgIbatUa",
                        "FgVbattMv",
                    ] {
                        mark_device_value(device, name, 0xFFFF_FFFF);
                    }
                    // `VbatAdcMv` is deliberately not included here: it does not
                    // come from a broken SPMI read but from the pump ADC, which
                    // lives on its own tick (`sample.vbat_uv`, the same quantity as
                    // `BattVbat` above). Sending it to the sentinel would lie about
                    // the ADC, and a zero would look like a real measurement. The
                    // sentinel is unreachable for `FgVbattMv` by magnitude too:
                    // 65535 bits at 122,07 µV is 8 001 mV, the cell ceiling, not
                    // 4,29·10⁹.
                    mark_device_value(device, "VbatAdcMv", sample.vbat_uv / 1000);
                    mark_device_value(device, "ChgrErr", 1);
                }
            }

            // The SMB5 buck limit is the other half of the transfer, and the
            // factory 1,5 A (`ChgrFccRaw = 30`) choke what the platform is capable
            // of: the live measurement on 19.09 gave 2,9 A into the cell at this
            // limit. But raising this limit is not an improvement by default, it is
            // a hypothesis that the same measurement overturned: build `.655` with
            // the raise to 2,5 A gave half the cell charge rate (0,249 %/min
            // against 0,409 %/min on `.652`) and three times less often held 2:1
            // (39 % against 85 %), because the amperes allowed to the buck drain
            // the adapter's eighteen watts and drop its QC3 level (`FAULT1 = 0x21`,
            // `SuMode = 1`, bus 4,5 V); the mechanism is analysed in
            // [`FCC_POLICY_VALUE_NAME`].
            // Therefore the policy writes only when it was enabled by the `FccRaw`
            // parameter: without the parameter and at zero nobody touches register
            // `0x1061`, and this default reproduces `.652` - there was no raise
            // there. The `fcc_boot_raw` capture above does not depend on the
            // policy: it is needed as diagnostics too, and the pair
            // `FccCfgRaw`/`ChgrFccRaw` reveals the pitfall with a "factory" value
            // raised by the previous boot.
            //
            // The raise only happens on a live 2:1 and below the gate
            // [`RE_ELEVATE_MAX_VBAT_UV`]: a limit raised at five volts or at the
            // plateau would hang on when the pump no longer transfers, and would
            // hinder the next platform decision. The return goes through
            // [`FCC_RESTORE_GRACE_MS`] after the loss of transfer, and only if the
            // original value was captured. A critical chip failure stops both
            // writes: raising the current into the cell while the chip reports
            // overheating or overvoltage is not allowed.
            if !status.has_critical_fault() {
                if pump_alive {
                    // The pump has picked up the current again - cancel the
                    // scheduled return.
                    st.fcc_restore_at_ms = 0;
                    if st.fcc_cfg_raw != 0
                        && !st.fcc_raised
                        && st.fcc_boot_raw != 0
                        && sample.vbat_uv < RE_ELEVATE_MAX_VBAT_UV
                    {
                        // `FccRaw` is the raw field value, not microamps: only this
                        // line converts it to microamps, and the ceiling
                        // [`FCC_WRITE_MAX_UA`] is applied by `raw_for`, so the
                        // ceiling cannot be bypassed another way.
                        let want = raw_for(st.fcc_cfg_raw.saturating_mul(FCC_STEP_UA));
                        // SAFETY: passive level; `device` is alive.
                        if unsafe { write_fcc_and_publish(device, want) } {
                            st.fcc_raised = true;
                        }
                    }
                } else if st.fcc_raised {
                    if st.fcc_restore_at_ms == 0 {
                        st.fcc_restore_at_ms =
                            sample.ts_ms.saturating_add(FCC_RESTORE_GRACE_MS);
                    } else if sample.ts_ms >= st.fcc_restore_at_ms {
                        // The return goes through the same encoding as the raise:
                        // the original byte is converted to microamps and back, so
                        // the [`FCC_WRITE_MAX_UA`] ceiling applies here too (for 30
                        // this is an identity - the live measurement gave 1,5 A).
                        let want =
                            raw_for(u32::from(st.fcc_boot_raw).saturating_mul(FCC_STEP_UA));
                        // SAFETY: passive level; `device` is alive.
                        if unsafe { write_fcc_and_publish(device, want) } {
                            st.fcc_raised = false;
                            st.fcc_restore_at_ms = 0;
                        }
                    }
                }
            }
        }
    }

    // The overheat episode is over - the 1:1 failure counter is zeroed: otherwise
    // a second episode ≥ 48 °C would stop the charge at once, without the
    // current step-down stage.
    if bypass_strikes_expired(&sample, &st.limits) {
        st.bypass_denied_strikes = 0;
    }

    // Temperature and current protection: the decision is made from the latest
    // sample. The action is allowed taking Vin into account: 1:1 is only valid
    // inside the bypass window.
    // The third argument is the setpoint that **is actually in the chip**: it is
    // read by `applied_iin_ua` (at the top of the charge the taper writes 1,2 A
    // past the profile, and by the profile protection would "reduce" the current
    // while raising it). The register was not read - the setpoint is unknown, we
    // take no current decisions on this tick.
    // The fourth is the deliberate taper setpoint (`Pump::taper_setpoint_ua`): the
    // return to the profile must stop at it and never lower the limit. Without it,
    // in the window where the taper and return bands overlap, protection would
    // cancel the deliberate current reduction to 1,2 A every 250 ms.
    let applied_iin_ua = pump.applied_iin_ua();
    let ov_latched = status.fault1_sts & ln8000::regs::FAULT1_VBAT_OV != 0;
    let deliberate_iin_ua = pump.taper_setpoint_ua(sample.vbat_uv, sample.vbus_uv, ov_latched);
    let action = evaluate(&sample, &st.limits, applied_iin_ua, deliberate_iin_ua);
    if !device.is_null() {
        // Who removed the mode: without these marks the drop from 2:1 to standby
        // is indistinguishable from a chip failure. `GuardAct`: 0 none, 1 reduce
        // current, 2 restore, 3 switch to 1:1, 4 stop. `DieTempDc` - the die
        // temperature the decision was made on (`0xFFFFFFFF` = channel
        // unreliable).
        mark_device_value(
            device,
            "GuardAct",
            match action {
                GuardAction::None => 0,
                GuardAction::ReduceCurrent { .. } => 1,
                GuardAction::RestoreCurrent { .. } => 2,
                GuardAction::FallbackToBypass { .. } => 3,
                GuardAction::Stop { .. } => 4,
                // `GuardAction` is marked `#[non_exhaustive]`: a new protection
                // stage must be visible in the mark rather than silently give zero.
                _ => 5,
            },
        );
        mark_device_value(
            device,
            "DieTempDc",
            if sample.die_temp_valid {
                u32::try_from(sample.die_temp_dc).unwrap_or(u32::MAX)
            } else {
                u32::MAX
            },
        );
    }
    if action.is_change() {
        apply_guard(
            pump,
            action,
            vbus,
            sample.vbat_uv,
            &st.limits,
            &mut st.bypass_denied_strikes,
        );
        st.actions = st.actions.saturating_add(1);
        println!(
            "ln8000-kmdf: protection {} ({}) at {temp} dC and {iin} uA",
            action.label(),
            match action {
                GuardAction::ReduceCurrent { to_ua, .. }
                | GuardAction::RestoreCurrent { to_ua, .. } => to_ua,
                _ => 0,
            }
        );
    }

    // Autostart / mode change by Vin (cp_qc30): 2:1 at >=8 V, bypass at ~5 V.
    // If already in bypass and Vin has risen via QC - an upgrade to 2:1 is
    // mandatory (do not wait 30 s). We do not do a soft-reset at elevated Vin: it
    // drops the QC latch.
    // Watchdog latch (FAULT1 bit7) forces standby - clear and retry without soft_reset.
    // Near-float VBAT_OV (bit6) likewise blocks mode until soft-cleared by set_charging.
    if status.fault1_sts
        & (ln8000::regs::FAULT1_WATCHDOG | ln8000::regs::FAULT1_VBAT_OV)
        != 0
    {
        let _ = pump.clear_latched_faults();
        if status.fault1_sts & ln8000::regs::FAULT1_WATCHDOG != 0 {
            let _ = pump.service_watchdog();
            println!("ln8000-kmdf: watchdog latch cleared");
        }
        if status.fault1_sts & ln8000::regs::FAULT1_VBAT_OV != 0 {
            println!("ln8000-kmdf: FAULT1_VBAT_OV latched - retry via set_charging");
        }
    }
    let vbus_uv = u32::try_from(vbus.max(0)).unwrap_or(0);
    let vbat_uv = sample.vbat_uv;
    // The mode is chosen from Vin AND Vbat (cp_qc30: 2:1 only at
    // Vin >= 2*Vbat + 250 mV). An elevated Vin without headroom gives None: the
    // 1:1 bypass is forbidden there, and we do not spin a standby loop - the
    // state is visible in `EngageState=4`.
    // We do not downgrade a working transfer on sag: under load the bus drops by
    // 100–250 mV, and the live measurement gives 8,176 V with a cell at 3,985 V,
    // where `charge_mode` requires `2*Vbat + 250 mV` = 8,22 V - that is, bypass.
    // Downgrading on an instantaneous sample tore the working 2:1 every ~5 s
    // (live measurement 19.09 11:28 on MDY-11-EP: mode 1→3→1 at 0,42 A,
    // `ChargeAttemptN` +1 per break). The sticky decision rests on the absolute
    // 2:1 floor (8,0 V): sag is a consequence of load, not a loss of the ability
    // to transfer.
    let desired = if pump_alive && vbus_uv >= u32::try_from(SWITCHING_MIN_VIN_UV).unwrap_or(0) {
        Some(OpMode::Switching)
    } else {
        charge_mode(vbus, vbat_uv)
    };
    let mode_ok = matches!(
        (desired, status.op_mode),
        (Some(OpMode::Switching), OpMode::Switching)
            | (Some(OpMode::Bypass), OpMode::Bypass)
            | (None, OpMode::Standby | OpMode::Unknown)
    );
    if !device.is_null() {
        mark_device_value(
            device,
            "EngageState",
            engage_state(sample.input_present, vbus, vbat_uv, status.op_mode),
        );
    }
    // The 2:1 transfer band follows the cell: while the cell takes on charge, its
    // top edge moves up, and the bus set during negotiation stays BELOW the band
    // - or, conversely, goes above it if negotiation aimed at a fixed 9,5 V. Both
    // end the same way: mode 3 is retained (`SYS_STS=0x04`) while the transfer
    // drops to the 39 mA additive floor.
    //
    // The input counts as elevated from `SWITCHING_MIN_VIN_UV` already: the 1:1
    // bypass is forbidden there, so "elevated but not in 2:1" is a state to fix
    // rather than to leave. The previous form required `desired == Switching`,
    // while a bus BELOW the band floor gives `desired = None` (`charge_mode`
    // requires `2*Vbat + 250 mV`) - and nobody raised it: live measurement
    // 18.09 22:38, Vin 8,88 V with the band at [9,00; 9,20] V, mode 1, 39 mA, the
    // attempt counter stood still because `(None, standby)` counts as a consistent
    // state.
    let elevated = vbus_uv >= u32::try_from(SWITCHING_MIN_VIN_UV).unwrap_or(0);
    // A bus above 5 V is no longer a "five-volt" input: the 1:1 bypass there heats
    // the voltage difference across the chip, while the 2:1 transfer band is only
    // one QC3 step higher. The previous condition (`elevated || dead_band`)
    // required `desired = None`, but at 6–8 V `charge_mode` returns bypass, and
    // nobody led the bus into the transfer band: one step was missing to the 2:1
    // gate.
    //
    // Live measurement 19.09 11:19 on MDY-11-EP: bus 7,888 V, `desired` = bypass,
    // 1:1 gives 2,06 A at 6,75 V (cell 3,915 V - the difference burns on the
    // chip), and the mode flaps 1↔2 every ~15 s: the bypass outside its window is
    // removed, the bus returns to 7,888 V, the bypass turns on again. 39 mA and
    // `ChargeAttemptN` stood still - the correction never ran.
    let above_five = vbus_uv >= u32::try_from(hvdcp::FIVE_V_STAY_MAX_UV).unwrap_or(0);
    let dead_band = desired.is_none() && above_five;
    let bypass_below_gate = matches!(desired, Some(OpMode::Bypass)) && above_five;
    // Leaving the bypass to raise the bus: on this tick the mode decision must not
    // be recomputed, it was computed from the old Vin (see below).
    let mut left_bypass_for_walk = false;
    if vbat_uv > 0 && (elevated || dead_band || bypass_below_gate) {
        // "Dead" current is the ADC floor (39,1 mA), not "little": at the top of
        // the charge the cell draws 0,1–0,5 A, and by the instantaneous sample such
        // ticks looked dead. So it is required that both the instantaneous sample
        // and the peak over the `IIN_WINDOW_MS` window lie on the floor - then
        // there really is no transfer behind the correction.
        let dead = status.op_mode == OpMode::Switching
            && sample.iin_ua <= hvdcp::IIN_DEAD_FLOOR_UA
            && st.max_iin_ua <= hvdcp::IIN_DEAD_FLOOR_UA;
        let outside = !ln8000::encoding::vin_in_switching_window(vbus, vbat_uv);
        if outside || dead {
            let now = monotonic_ms();
            let target = hvdcp::target_vbus_uv(vbat_uv);
            let err = vbus_uv.abs_diff(target);
            // Progress resets the delay; a stall (the brick does not hold the QC3
            // step, or this is a PD adapter for which the pulses are a waste of
            // SPMI) switches the loop to a rare repeat.
            if err.saturating_add(WINDOW_PROGRESS_UV) < st.window_best_err_uv {
                st.window_best_err_uv = err;
                st.window_stall_n = 0;
            }
            let cooldown = if st.window_stall_n < WINDOW_STALL_MAX {
                hvdcp::WINDOW_NUDGE_MS
            } else {
                WINDOW_STALL_MS
            };
            // The first tick in an elevated bypass does not wait for the cooldown:
            // 1:1 at 6–8 V heats the voltage difference across the chip, and every
            // extra tick there is 10 s of wasted work (see `bypass_below_gate`).
            // After that it is the ordinary rate: `window_stall_n` grows on every
            // trigger, so a pulse storm is impossible even if we failed to get out
            // of the bypass.
            let urgent = matches!(desired, Some(OpMode::Bypass))
                && status.op_mode == OpMode::Bypass
                && st.window_stall_n == 0;
            if urgent || now.saturating_sub(st.last_window_nudge_ms) >= cooldown {
                st.last_window_nudge_ms = now;
                st.window_stall_n = st.window_stall_n.saturating_add(1);
                // In 1:1 an INC pulse raises the input directly onto the cell: the
                // bypass FET is closed, and the QC3 step goes not into the bus but
                // into the voltage difference across the chip. So we first take the
                // chip to standby - then the pulse changes the input voltage
                // itself, and the next tick will decide the mode from the new Vin.
                if status.op_mode == OpMode::Bypass {
                    let left = pump.standby().is_ok();
                    left_bypass_for_walk = left;
                    if !device.is_null() {
                        mark_device_value(device, "WalkStandby", u32::from(left));
                    }
                }
                // SAFETY: PASSIVE_LEVEL; the session is open, access is serialized
                // by the timer (see the comment at `state()`).
                let sent = unsafe {
                    hvdcp::nudge_vin_into_window(
                        device,
                        st.usbin_id,
                        &mut st.hvdcp,
                        vbus,
                        vbat_uv,
                        dead,
                    )
                };
                if !device.is_null() {
                    mark_device_value(device, "WindowOut", u32::from(outside));
                    mark_device_value(device, "WindowDead", u32::from(dead));
                }
                if sent > 0 {
                    // The bus has just shifted - we re-decide the mode at once,
                    // without waiting for `CHARGE_RETRY_MS`: a single QC3 step is
                    // below `CHARGE_RETRY_DELTA_UV`, so "input changed" would not
                    // wake it.
                    st.last_charge_attempt_ms = 0;
                }
            }
        } else if st.window_best_err_uv != u32::MAX || st.window_stall_n != 0 {
            // Back inside the band - the stall history is reset.
            st.window_best_err_uv = u32::MAX;
            st.window_stall_n = 0;
        }
    } else if st.window_best_err_uv != u32::MAX || st.window_stall_n != 0 {
        st.window_best_err_uv = u32::MAX;
        st.window_stall_n = 0;
    }
    // Five-volt mode without a transfer is the state in which negotiation is owned
    // by the HVDCP re-elevation (below, `want_reelevate`), not by the fast repeat of
    // the mode decision. Both predicates are lifted here, to their two consumers,
    // in one place: further down they decide both whether the fast repeat stays
    // silent (`changed`) and whether the elevation itself is included.
    let five_v_regime =
        vbus_uv >= u32::try_from(ln8000::encoding::CHARGE_MIN_VIN_UV).unwrap_or(0)
            && vbus_uv < u32::try_from(hvdcp::FIVE_V_STAY_MAX_UV).unwrap_or(0);
    // A single sample does not prove there is no transfer: the ADC floor is
    // 39,1 mA, while at the top of the charge the cell draws 0,1–0,5 A, and the
    // instantaneous measurement falls to the floor between current pulses. The
    // second support, as for `dead` above, is the peak over the `IIN_WINDOW_MS`
    // window: the transfer is live if at least one quantity is above the floor.
    let bypass_carrying = status.op_mode == OpMode::Bypass
        && (sample.iin_ua > hvdcp::IIN_DEAD_FLOOR_UA
            || st.max_iin_ua > hvdcp::IIN_DEAD_FLOOR_UA);
    // The conditions under which the HVDCP re-elevation owns negotiation: input
    // present, bus in five-volt mode, bypass not carrying current, pump not
    // working, cell below the elevation gate. While they hold, the fast repeat "on
    // input change" stays silent (see `changed` below): in this state an input
    // change is a sawtooth tooth created by our own POR, and the repeat would run
    // past the elevation delay. Live measurement 19.09 (input sagged to 4,5–5 V):
    // `ChargeAttemptN` 5→86 in 14,5 min, that is a full negotiation (APSD ~2 s +
    // `FORCE_9V` up to 5 s - about 7 s under the state mutex per attempt) every
    // ~10 s. The elevation delay (60 s doubling to 300 s) is the right rate for
    // this state: it is set precisely as "repeat what re-plugging does" rather
    // than "hammer a POR every five seconds".
    let reelevate_owns_negotiation = sample.input_present
        && vbat_read.is_ok()
        && iin_read.is_ok()
        && five_v_regime
        && !bypass_carrying
        && !pump_alive
        && vbat_uv < RE_ELEVATE_MAX_VBAT_UV;
    // The bypass was removed to raise the bus: `desired` was computed from the old
    // Vin, so we do not re-decide the mode on this tick - the next tick (250 ms)
    // will read the bus again and pick 2:1 if the pulse brought it into the
    // transfer band.
    if mode_ok && !left_bypass_for_walk {
        st.last_charge_attempt_ms = monotonic_ms();
        st.last_attempt_vbus_uv = vbus_uv;
    } else if desired.is_some() && !action.is_change() && !left_bypass_for_walk {
        let now = monotonic_ms();
        let cooled = now.saturating_sub(st.last_charge_attempt_ms) >= CHARGE_RETRY_MS;
        // An input change speeds up the repeat but does not cancel the floor:
        // otherwise the input sawtooth, which the POR itself creates, drives
        // attempts every ~0,6 s (see [`CHARGE_RETRY_FAST_MS`]).
        //
        // In five-volt mode without a transfer the fast repeat stays silent
        // (`reelevate_owns_negotiation` above): there negotiation is owned by the
        // re-elevation with its own delay, and an input change is a sawtooth tooth
        // from our own POR rather than a new QC level. Outside this state (the bus
        // went above 6 V, the bypass carried current, the cell is at the top) the
        // condition is false, and the repeat works as before. The `cooled` cooldown
        // and the instant `upgrade` are untouched: the 30-second repeat remains the
        // safety net for a failure of the mode decision, and `upgrade` is only
        // possible at elevated Vin, where the elevation condition is false.
        let changed = st.last_attempt_vbus_uv.abs_diff(vbus_uv) >= CHARGE_RETRY_DELTA_UV
            && now.saturating_sub(st.last_charge_attempt_ms) >= CHARGE_RETRY_FAST_MS
            && !reelevate_owns_negotiation;
        // The bypass → 2:1 upgrade is done at once (QC raised Vin). standby/unknown
        // is a failure, and the repeat happens no more often than the cooldown
        // rather than every tick.
        let upgrade = matches!(
            (desired, status.op_mode),
            (Some(OpMode::Switching), OpMode::Bypass)
        );
        if cooled || changed || upgrade {
            st.last_charge_attempt_ms = now;
            st.last_attempt_vbus_uv = vbus_uv;
            // SAFETY: the session is open, access is serialized by the timer.
            // Soft-reset + configure already try to charge on the same tick.
            let recovery = recover_ln_shutdown(pump);
            // We do not do a second POR on this same tick: the diagnostics must
            // match the actual actions (see the failure branch below).
            let recovered_this_tick = matches!(recovery, ShutdownRecovery::Recovered(_));
            let result = match recovery {
                ShutdownRecovery::Recovered(outcome) => {
                    st.last_charge_attempt_ms = monotonic_ms();
                    outcome
                }
                ShutdownRecovery::NotInShutdown => pump.set_charging(true, &mut por_delay),
            };
            st.charge_attempts = st.charge_attempts.saturating_add(1);
            st.last_enable_ms = monotonic_ms();
            if !device.is_null() {
                mark_charge_attempt(device, st.charge_attempts, st.last_enable_ms, &result);
            }
            match result {
                Ok(mode) => {
                    st.auto_starts = st.auto_starts.saturating_add(1);
                    st.failed_attempts = 0;
                    st.last_error = 0;
                    println!("ln8000-kmdf: charge enabled automatically, mode {}", mode.code());
                }
                Err(err) => {
                    st.last_error = pump_error_code(err);
                    st.failed_attempts = st.failed_attempts.saturating_add(1);
                    if st.failed_attempts >= CHARGE_FAILS_BEFORE_RESET {
                        st.failed_attempts = 0;
                        if recovered_this_tick {
                            println!(
                                "ln8000-kmdf: retry after recovery failed - second soft_reset skipped on this tick"
                            );
                        } else {
                            match recover_ln_shutdown(pump) {
                                ShutdownRecovery::Recovered(outcome) => {
                                    st.last_charge_attempt_ms = monotonic_ms();
                                    st.charge_attempts = st.charge_attempts.saturating_add(1);
                                    st.last_enable_ms = monotonic_ms();
                                    if !device.is_null() {
                                        mark_charge_attempt(
                                            device,
                                            st.charge_attempts,
                                            st.last_enable_ms,
                                            &outcome,
                                        );
                                    }
                                    match outcome {
                                        Ok(mode) => println!(
                                            "ln8000-kmdf: soft_reset after SHUTDOWN - charge restarted, mode {}",
                                            mode.code()
                                        ),
                                        Err(err) => println!(
                                            "ln8000-kmdf: soft_reset after SHUTDOWN - retry failed: {err}"
                                        ),
                                    }
                                }
                                ShutdownRecovery::NotInShutdown if vbus < SWITCHING_MIN_VIN_UV => {
                                    let _ = pump.soft_reset();
                                    por_delay();
                                    let _ = pump.configure();
                                    println!("ln8000-kmdf: soft_reset + reconfigure (5 V path)");
                                    // Fix 4: we enable the charge on the same tick,
                                    // not after CHARGE_RETRY_MS.
                                    let retry = pump.set_charging(true, &mut por_delay);
                                    st.charge_attempts = st.charge_attempts.saturating_add(1);
                                    st.last_enable_ms = monotonic_ms();
                                    if !device.is_null() {
                                        mark_charge_attempt(
                                            device,
                                            st.charge_attempts,
                                            st.last_enable_ms,
                                            &retry,
                                        );
                                    }
                                    match retry {
                                        Ok(mode) => println!(
                                            "ln8000-kmdf: 5 V recovery - charge restarted, mode {}",
                                            mode.code()
                                        ),
                                        Err(err) => println!(
                                            "ln8000-kmdf: 5 V recovery - retry failed: {err}"
                                        ),
                                    }
                                }
                                ShutdownRecovery::NotInShutdown => {
                                    let _ = pump.clear_latched_faults();
                                    println!(
                                        "ln8000-kmdf: latch cleared (elevated Vin, no soft_reset)"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    } else if desired.is_none()
        && matches!(status.op_mode, OpMode::Switching | OpMode::Bypass)
        && !action.is_change()
    {
        let _ = pump.set_charging(false, &mut por_delay);
    }

    // Chip watchdog: if it is enabled in the profile, it must be serviced more
    // often than the period, otherwise the chip stops charging by itself. When the
    // watchdog is off, the call does nothing.
    if let Err(err) = pump.service_watchdog() {
        println!("ln8000-kmdf: failed to service the watchdog: {err}");
    }

    // Cold-plug / re-plug: decide while pump is borrowed, act after drop.
    let input_now = hvdcp::input_present_from_vin(vbus);
    let want_replug = st.hvdcp_edge_armed
        && hvdcp::should_renegotiate_on_input_edge(
            st.hvdcp.phase.code(),
            st.last_input_present,
            input_now,
        );
    let now = monotonic_ms();
    let want_retry = hvdcp::superuser_retry_due(
        st.hvdcp_retry_pending,
        st.hvdcp_retry_attempts,
        now,
        st.hvdcp_retry_next_ms,
    );
    // Five-volt mode without a transfer. Negotiation sees the input only on an
    // edge, so a lost QC level (the brick fell back to 5 V, the VFAULT latch
    // closed the 1:1 bypass) does not come back by itself - the bus sits at 5 V
    // until the cable is re-plugged. The decision is made here, and the elevation
    // itself happens after `pump` is released (it reopens the bus). The entry
    // condition is the same `reelevate_owns_negotiation` that silenced the fast
    // repeat above: the state has one owner, and its delay is its rate.
    let mut want_reelevate = false;
    if reelevate_owns_negotiation {
        if st.re_elevate_next_ms == 0 {
            st.re_elevate_next_ms = now.saturating_add(RE_ELEVATE_FIRST_MS);
        } else if now >= st.re_elevate_next_ms {
            st.re_elevate_attempts = st.re_elevate_attempts.saturating_add(1);
            st.re_elevate_backoff_ms = if st.re_elevate_backoff_ms == 0 {
                RE_ELEVATE_FIRST_MS
            } else {
                st.re_elevate_backoff_ms
                    .saturating_mul(2)
                    .min(RE_ELEVATE_BACKOFF_MAX_MS)
            };
            st.re_elevate_next_ms = now.saturating_add(st.re_elevate_backoff_ms);
            want_reelevate = true;
        }
    } else if st.re_elevate_next_ms != 0 || st.re_elevate_attempts != 0 {
        // The bus has risen or 5 V really charges - the episode is closed.
        st.re_elevate_next_ms = 0;
        st.re_elevate_attempts = 0;
        st.re_elevate_backoff_ms = 0;
    }
    st.last_input_present = input_now;
    (want_replug, want_retry, want_reelevate)
    };

    // SAFETY: DEVICE set in device_add; null after release_hardware.
    let device = unsafe { DEVICE };
    if device.is_null() {
        // Released - timer already stopped; do not re-arm.
        return;
    }
    if want_replug {
        mark_device_value(device, "HvdcpReplug", 1);
        // A new input - the old `FORCE_9V` latch has no force: the brick may have
        // been replaced with a QC3 one, for which pulses are the very way to
        // elevate, and `pulse_cmd_bit` lets them through under the latch. Holding
        // it across a re-plug means forbidding the pulse path until the driver is
        // restarted. Within one session the latch is still cleared only by
        // `safe_force_5v`: otherwise the first pulse would drop the QC2 level.
        unsafe { state() }.hvdcp.force9v_latched = false;
        println!("ln8000-kmdf: HVDCP re-plug edge - renegotiate");
        let code = run_hvdcp_and_land(device);
        schedule_or_clear_superuser_retry(device, code);
        arm_hvdcp_input_edge(device);
    } else if want_retry {
        // SAFETY: timer serialized with prepare/IOCTL.
        let st = unsafe { state() };
        st.hvdcp_retry_attempts = st.hvdcp_retry_attempts.saturating_add(1);
        st.hvdcp_retry_next_ms =
            monotonic_ms().saturating_add(hvdcp::HVDCP_SUPERUSER_RETRY_MS);
        mark_device_value(device, "HvdcpRetryN", st.hvdcp_retry_attempts);
        println!(
            "ln8000-kmdf: HVDCP SUPERUSER retry #{}",
            st.hvdcp_retry_attempts
        );
        let code = run_hvdcp_and_land(device);
        schedule_or_clear_superuser_retry(device, code);
        if code == 0 {
            arm_hvdcp_input_edge(device);
        }
    } else if want_reelevate {
        // SAFETY: the timer is serialized with prepare/IOCTL; `pump` was released above.
        let st = unsafe { state() };
        mark_device_value(device, "ReElevateN", st.re_elevate_attempts);
        println!(
            "ln8000-kmdf: 5 V without transfer - HVDCP re-elevation #{} (delay {} ms)",
            st.re_elevate_attempts, st.re_elevate_backoff_ms
        );
        let code = run_hvdcp_and_land(device);
        schedule_or_clear_superuser_retry(device, code);
        arm_hvdcp_input_edge(device);
    }

    arm_telemetry_timer();
}

/// Applies a protection decision to the device.
///
/// The switch to 1:1 is only allowed with `Vin` inside the bypass window:
/// `EN_1TO1` feeds the input directly to the battery, so at elevated Vin (QC/PD
/// 9–12 V) protection reduces the current instead of bypassing, and at a stubborn
/// temperature it stops the charge. `denied_strikes` is the counter of ticks when
/// 1:1 was needed and forbidden.
fn apply_guard(
    pump: &mut Pump<SpbBus>,
    action: GuardAction,
    vin_uv: i32,
    vbat_uv: u32,
    limits: &GuardLimits,
    denied_strikes: &mut u32,
) {
    match action {
        GuardAction::None => {
            *denied_strikes = 0;
        }
        GuardAction::ReduceCurrent { to_ua, .. } => {
            *denied_strikes = 0;
            if let Err(err) = pump.set_iin_limit(to_ua) {
                println!("ln8000-kmdf: failed to reduce the current: {err}");
            }
        }
        GuardAction::RestoreCurrent { to_ua, reason } => {
            *denied_strikes = 0;
            println!(
                "ln8000-kmdf: {reason} - returning the profile current limit to {to_ua} µA"
            );
            if let Err(err) = pump.set_iin_limit(to_ua) {
                println!("ln8000-kmdf: failed to restore the current limit: {err}");
            }
        }
        GuardAction::FallbackToBypass { reason } => match resolve_bypass(
            vin_uv,
            vbat_uv,
            *denied_strikes,
            limits,
        ) {
            BypassResolution::Allowed => {
                *denied_strikes = 0;
                if let Err(err) = pump.enable_bypass() {
                    println!("ln8000-kmdf: failed to switch to bypass: {err}");
                }
            }
            BypassResolution::ReduceCurrent { to_ua, reason } => {
                *denied_strikes = denied_strikes.saturating_add(1);
                println!(
                    "ln8000-kmdf: {reason} at Vin {vin_uv} µV - 1:1 forbidden, reducing the current to {to_ua} µA"
                );
                if let Err(err) = pump.set_iin_limit(to_ua) {
                    println!("ln8000-kmdf: failed to reduce the current: {err}");
                }
            }
            BypassResolution::Stop { reason } => {
                println!(
                    "ln8000-kmdf: {reason} at Vin {vin_uv} µV - 1:1 forbidden, stopping the charge"
                );
                if let Err(err) = pump.standby() {
                    println!("ln8000-kmdf: failed to stop the charge: {err}");
                }
            }
            // The enum is marked `non_exhaustive`: we do not expect new
            // permissions, but just in case we do not enable 1:1 (the safe side).
            _ => {
                println!("ln8000-kmdf: unknown bypass verdict ({reason}) - 1:1 not enabled");
            }
        },
        GuardAction::Stop { .. } => {
            *denied_strikes = 0;
            if let Err(err) = pump.standby() {
                println!("ln8000-kmdf: failed to stop the charge: {err}");
            }
        }
        // The enum is marked `non_exhaustive`: we ignore new actions.
        _ => {}
    }
}

/// Handles user-mode control requests.
///
/// # Safety
///
/// Called by WDF; the request buffers are checked by size.
unsafe extern "C" fn evt_io_device_control(
    _queue: WDFQUEUE,
    request: WDFREQUEST,
    _output_buffer_length: usize,
    input_buffer_length: usize,
    io_control_code: ULONG,
) {
    // The handlers below read `STATE` and go out on the bus: without the mutex they
    // would collide with the telemetry timer.
    let _state = lock_state();
    match io_control_code {
        ioctl::IOCTL_LN8000_GET_STATUS => unsafe { handle_get_status(request) },
        ioctl::IOCTL_LN8000_READ_REG => unsafe { handle_read_reg(request, input_buffer_length) },
        ioctl::IOCTL_LN8000_WRITE_REG => unsafe { handle_write_reg(request, input_buffer_length) },
        ioctl::IOCTL_LN8000_SET_LIMITS => unsafe { handle_set_limits(request, input_buffer_length) },
        ioctl::IOCTL_LN8000_SET_MODE => unsafe { handle_set_mode(request, input_buffer_length) },
        ioctl::IOCTL_LN8000_SET_CHARGE => unsafe { handle_set_charge(request, input_buffer_length) },
        ioctl::IOCTL_LN8000_RUN_HVDCP => unsafe { handle_run_hvdcp(request, input_buffer_length) },
        ioctl::IOCTL_LN8000_GET_SESSIONS => unsafe { handle_get_sessions(request) },
        ioctl::IOCTL_LN8000_GET_SAMPLES => unsafe {
            handle_get_samples(request, input_buffer_length)
        },
        _ => unsafe {
            complete(request, wdk_sys::STATUS_INVALID_DEVICE_REQUEST, 0);
        },
    }
}

/// Autostart HVDCP negotiate via SUPERUSER (Usbin RH secondary).
fn try_autostart_hvdcp(device: WDFDEVICE) {
    mark_device_value(device, "HvdcpAuto", 1);
    let code = run_hvdcp_and_land(device);
    if code != 0 {
        println!("ln8000-kmdf: HVDCP autostart rc={code}");
    }
    schedule_or_clear_superuser_retry(device, code);
    arm_hvdcp_input_edge(device);
}

/// Schedule SUPERUSER retry when the slot was full; clear when negotiate progressed.
fn schedule_or_clear_superuser_retry(device: WDFDEVICE, negotiate_rc: i32) {
    // SAFETY: WDF serializes prepare / IOCTL / timer.
    let st = unsafe { state() };
    if hvdcp::should_schedule_superuser_retry(negotiate_rc) {
        if st.hvdcp_retry_attempts >= hvdcp::HVDCP_SUPERUSER_RETRY_MAX {
            st.hvdcp_retry_pending = false;
            mark_device_value(device, "HvdcpRetryPend", 2);
            println!("ln8000-kmdf: HVDCP SUPERUSER retry exhausted");
        } else {
            st.hvdcp_retry_pending = true;
            if st.hvdcp_retry_next_ms == 0 {
                st.hvdcp_retry_next_ms =
                    monotonic_ms().saturating_add(hvdcp::HVDCP_SUPERUSER_RETRY_MS);
            }
            mark_device_value(device, "HvdcpRetryPend", 1);
            mark_device_value(device, "HvdcpRetryN", st.hvdcp_retry_attempts);
            println!("ln8000-kmdf: HVDCP SUPERUSER busy - will retry");
        }
    } else {
        st.hvdcp_retry_pending = false;
        st.hvdcp_retry_attempts = 0;
        st.hvdcp_retry_next_ms = 0;
        mark_device_value(device, "HvdcpRetryPend", 0);
    }
}

/// After PrepareHardware autostart, seed cable-present so the first timer tick
/// does not look like a rising edge (would double-negotiate).
fn arm_hvdcp_input_edge(device: WDFDEVICE) {
    // SAFETY: prepare serialized with timer.
    let st = unsafe { state() };
    let vin = st
        .pump
        .as_mut()
        .and_then(|p| p.read_adc(AdcChannel::Vin).ok())
        .unwrap_or(0);
    st.last_input_present = hvdcp::input_present_from_vin(vin);
    st.hvdcp_edge_armed = true;
    mark_device_value(
        device,
        "HvdcpEdgeArm",
        u32::from(st.last_input_present),
    );
}

/// Negotiate + post-path (boost/trim/ICL + set_charging). Shared by autostart,
/// SUPERUSER retry, re-plug edge, and `IOCTL_RUN_HVDCP`.
fn run_hvdcp_and_land(device: WDFDEVICE) -> i32 {
    // SAFETY: called from prepare / IOCTL / timer; WDF serializes them.
    let st = unsafe { state() };
    let usbin_id = st.usbin_id;
    let vbat_uv = st
        .pump
        .as_mut()
        .and_then(|p| p.read_adc(AdcChannel::Vbat).ok())
        .map(|v| u32::try_from(v.max(0)).unwrap_or(0))
        .unwrap_or(4_000_000);
    let (code, _) = {
        let pump = &mut st.pump;
        let hvdcp_st = &mut st.hvdcp;
        let mut read_vin = || {
            pump
                .as_mut()
                .and_then(|p| p.read_adc(AdcChannel::Vin).ok())
                .unwrap_or(0)
        };
        // SAFETY: PASSIVE_LEVEL; SUPERUSER preferred, Usbin id optional.
        unsafe { hvdcp::run_negotiate_report(device, usbin_id, vbat_uv, hvdcp_st, &mut read_vin) }
    };
    // After Vin elevation (or 5 V stay), land in the correct pump path:
    //  Vin >= 2*Vbat + 250 mV → boost/trim + ICL pump + 2:1
    //  ~5 V → max safe IIN retreat bypass (plain DCP / QC2 brick without elevate)
    //  elevated, but without headroom (8,0–9,15 V) → first pull the bus into the
    //  9,5–9,8 V window (Android `cp_qc30.c:848`: UP while `vbus <= 9500`), then
    //  re-read the ADC and decide again. Previously this case fixed nothing: boost
    //  sat inside `if engage == Switching`, that is behind the very condition it
    //  was supposed to create, and the tick ended with `ModeNotReached`.
    if let Some(pump) = st.pump.as_mut() {
        let mut vin = pump.read_adc(AdcChannel::Vin).unwrap_or(0);
        let vbat_now = pump
            .read_adc(AdcChannel::Vbat)
            .ok()
            .map_or(vbat_uv, |v| u32::try_from(v.max(0)).unwrap_or(vbat_uv));
        let mut engage = charge_mode(vin, vbat_now);
        if engage != Some(OpMode::Switching) && vin >= hvdcp::FIVE_V_STAY_MAX_UV {
            // The input is elevated (not the five-volt branch), but 2:1 is not yet
            // allowed: we pull the bus into the transfer band and re-read the ADC.
            // The threshold is "above 5 V", not `SWITCHING_MIN_VIN_UV`: a live QC3
            // brick in continuous mode settles slightly BELOW the 2:1 gate
            // (measurement 19.09: 7,97 V against the 8,0 V floor), and the last step
            // is made by an INC pulse rather than a failure.
            // SAFETY: PASSIVE_LEVEL; may reopen SUPERUSER for INC pulses.
            let _ = unsafe {
                hvdcp::nudge_vin_into_window(device, usbin_id, &mut st.hvdcp, vin, vbat_now, false)
            };
            vin = pump.read_adc(AdcChannel::Vin).unwrap_or(vin);
            engage = charge_mode(vin, vbat_now);
        }
        if engage == Some(OpMode::Switching) {
            mark_device_value(device, "SuAfc5vPath", 0);
            // The transfer band follows the cell (`[2*Vbat+200, 2*Vbat+400]` mV), so
            // it is held by the live Vbat rather than fixed 9,5–10,5 V: those lie
            // ABOVE the band on a full cell, and the pump delivers exactly 39 mA
            // (live measurement 18.09). `false` - the current has not been measured
            // here yet, the correction is voltage-only; the dead current is caught
            // by the telemetry tick.
            // SAFETY: PASSIVE_LEVEL; may reopen SUPERUSER for pulses.
            let _ = unsafe {
                hvdcp::nudge_vin_into_window(device, usbin_id, &mut st.hvdcp, vin, vbat_now, false)
            };
            // There is no reason to re-read the ADC here: `set_charging` below reads
            // Vin and Vbat itself, and the mode has already been chosen above.
            // SAFETY: PASSIVE_LEVEL; SUPERUSER / Usbin RH for ICL.
            let _ = unsafe { hvdcp::raise_icl_for_pump(device, usbin_id, &mut st.hvdcp) };
        } else if engage == Some(OpMode::Bypass) && hvdcp::vin_stayed_near_5v(vin) {
            // Plain DCP / QC2 FORCE no-op / brick without elevate: 1:1 bypass @ 2.7 A.
            // SAFETY: PASSIVE_LEVEL; raises USBIN ICL for 5 V high-current.
            let _ = unsafe { hvdcp::raise_icl_for_5v_bypass(device, usbin_id, &mut st.hvdcp) };
            // Align pump IIN limit with NABU class-B bus budget.
            let _ = pump.set_iin_limit(2_700_000);
        }
        let result = match recover_ln_shutdown(pump) {
            ShutdownRecovery::Recovered(outcome) => {
                st.last_charge_attempt_ms = monotonic_ms();
                outcome
            }
            ShutdownRecovery::NotInShutdown => pump.set_charging(true, &mut por_delay),
        };
        st.charge_attempts = st.charge_attempts.saturating_add(1);
        st.last_enable_ms = monotonic_ms();
        mark_charge_attempt(device, st.charge_attempts, st.last_enable_ms, &result);
        match result {
            Ok(mode) => {
                st.auto_starts = st.auto_starts.saturating_add(1);
                st.failed_attempts = 0;
                st.last_error = 0;
                mark_device_value(device, "PostHvdcpMode", u32::from(mode.code()));
                mark_device_value(device, "PostHvdcpErr", 0);
                println!("ln8000-kmdf: post-HVDCP charge mode {}", mode.label());
            }
            Err(err) => {
                let ec = pump_error_code(err);
                st.last_error = ec;
                mark_device_value(device, "PostHvdcpMode", 0);
                mark_device_value(device, "PostHvdcpErr", ec as u32);
                println!("ln8000-kmdf: post-HVDCP set_charging failed rc={ec}");
            }
        }
    }
    code
}

/// IOCTL: run or report HVDCP state (SUPERUSER preferred; Usbin RH secondary).
///
/// # Safety
///
/// `request` is a live WDF request; input may be empty (defaults to command=1).
unsafe fn handle_run_hvdcp(request: WDFREQUEST, input_length: usize) {
    let mut answer = Ln8000HvdcpRequest::default();
    if input_length >= core::mem::size_of::<Ln8000HvdcpRequest>() {
        if let Some(req) = unsafe { read_input::<Ln8000HvdcpRequest>(request) } {
            answer.command = req.command;
        }
    } else if input_length > 0 {
        unsafe { complete(request, wdk_sys::STATUS_BUFFER_TOO_SMALL, 0) };
        return;
    } else {
        answer.command = 1;
    }

    let queue =
        unsafe { call_unsafe_wdf_function_binding!(WdfRequestGetIoQueue, request) };
    let device = unsafe { call_unsafe_wdf_function_binding!(WdfIoQueueGetDevice, queue) };

    // SAFETY: IOCTL queue is sequential with timer/prepare.
    let st = unsafe { state() };
    let vbat_uv = st
        .pump
        .as_mut()
        .and_then(|p| p.read_adc(AdcChannel::Vbat).ok())
        .map(|v| u32::try_from(v.max(0)).unwrap_or(0))
        .unwrap_or(4_000_000);

    if answer.command == 0 {
        // Status-only: never touches the bus. Transport may still be SUPERUSER.
        answer.error_code = st.hvdcp.phase.code() as i32;
        if st.hvdcp.phase == hvdcp::HvdcpPhase::Idle && st.usbin_id.is_none() {
            // Idle + no overlay: report 0 (SUPERUSER may still work on command=1).
            answer.error_code = 0;
        }
    } else {
        // Re-elevate is always allowed: FORCE_5V / FiveVBypass / prior Done must
        // not stick until driver reload. RUN_HVDCP re-opens SUPERUSER and negotiates.
        let code = run_hvdcp_and_land(device);
        answer.error_code = code;
        schedule_or_clear_superuser_retry(device, code);
        // Refresh edge baseline after intentional negotiate.
        arm_hvdcp_input_edge(device);
    }

    answer.apsd_status = st.hvdcp.apsd_status;
    answer.apsd_result = st.hvdcp.apsd_result;
    answer.pulse_cnt = u8::try_from(st.hvdcp.pulse_cnt.min(255)).unwrap_or(255);
    answer.phase = st.hvdcp.phase.code();
    if answer.target_vbus_uv == 0 {
        answer.target_vbus_uv = hvdcp::target_vbus_uv(vbat_uv);
    }
    answer.estimated_vbus_uv = hvdcp::estimated_vbus_uv(st.hvdcp.pulse_cnt);
    unsafe { write_output(request, &answer) };
}

/// Exposes the driver state.
///
/// # Safety
///
/// `request` is valid; the output buffer is large enough.
unsafe fn handle_get_status(request: WDFREQUEST) {
    // SAFETY: access is serialized by WDF.
    let st = unsafe { state() };
    let mut status = Ln8000Status {
        magic: LN8000_STATUS_MAGIC,
        version: LN8000_STATUS_VERSION,
        state: match st.pump.as_ref().map(Pump::state) {
            Some(PumpState::Probed) => 1,
            Some(PumpState::Configured) => 2,
            Some(PumpState::Switching) => 3,
            Some(PumpState::Faulted) => 4,
            _ => 0,
        },
        writes: st.writes,
        reads: st.reads,
        last_error: st.last_error,
        sessions: st.telemetry.session_total(),
        samples: st.telemetry.sample_total(),
        ..Ln8000Status::default()
    };
    if let Some(pump) = st.pump.as_ref() {
        status.op_mode = pump.op_mode().code();
    }
    if let Some(pump) = st.pump.as_mut() {
        if let Ok(live) = pump.status() {
            status.op_mode = live.op_mode.code();
            status.sys_sts = live.sys_sts;
            status.fault1_sts = live.fault1_sts;
            status.fault2_sts = live.fault2_sts;
            status.safety_sts = live.safety_sts;
            status.critical_fault = u8::from(live.has_critical_fault());
        }
    }
    if let Some(sample) = st.telemetry.last_sample() {
        status.iin_ua = sample.iin_ua;
        status.vbat_uv = sample.vbat_uv;
        status.vbus_uv = sample.vbus_uv;
        status.die_temp_dc = sample.die_temp_dc;
        // Prefer live op_mode already filled above; keep sample only if no pump.
        if st.pump.is_none() {
            status.op_mode = sample.op_mode.code();
        }
    } else if let Some(pump) = st.pump.as_mut() {
        // There is no stored snapshot yet (the periodic collection has not filled
        // it), so we read the values in place: a bus transfer takes single-digit
        // milliseconds, and the handler does not block.
        status.vbat_uv =
            u32::try_from(pump.read_adc(AdcChannel::Vbat).unwrap_or_default().max(0)).unwrap_or(0);
        status.vbus_uv =
            u32::try_from(pump.read_adc(AdcChannel::Vin).unwrap_or_default().max(0)).unwrap_or(0);
        status.iin_ua =
            u32::try_from(pump.read_adc(AdcChannel::Iin).unwrap_or_default().max(0)).unwrap_or(0);
        status.die_temp_dc = pump.read_adc(AdcChannel::DieTemp).unwrap_or_default();
    }
    // Keep BattC SoC fresh even when the telemetry timer failed to start.
    if status.vbat_uv > 0 || status.vbus_uv > 0 {
        unsafe {
            battery::update_from_telemetry(
                status.vbat_uv,
                status.vbus_uv,
                status.iin_ua,
                st.max_iin_ua,
                status.fault1_sts & ln8000::regs::FAULT1_VAC_UNPLUG != 0,
                // Suitability of the input samples: the same flag as in the
                // telemetry tick - a zero channel means "no sample".
                status.vbat_uv > 0 && status.vbus_uv > 0,
                battery::soc_rising(),
                monotonic_ms(),
            );
        }
        let device = unsafe { DEVICE };
        if !device.is_null() {
            mark_device_value(device, "BattPct", battery::last_percent());
            mark_device_value(device, "BattVbat", status.vbat_uv / 1000);
            mark_device_value(device, "BattPwr", battery::last_power_state());
        }
    }
    unsafe { write_output(request, &status) };
}

/// Reads an LN8000 register (diagnostics).
///
/// # Safety
///
/// `request` is valid; the input buffer contains [`Ln8000RegRequest`].
unsafe fn handle_read_reg(request: WDFREQUEST, input_length: usize) {
    if input_length < core::mem::size_of::<Ln8000RegRequest>() {
        unsafe { complete(request, wdk_sys::STATUS_BUFFER_TOO_SMALL, 0) };
        return;
    }
    // SAFETY: the buffer has been checked by size; the address comes from the
    // client request.
    let Some(mut answer) = (unsafe { read_input::<Ln8000RegRequest>(request) }) else {
        unsafe { complete(request, wdk_sys::STATUS_INVALID_PARAMETER, 0) };
        return;
    };
    // SAFETY: access is serialized by WDF.
    let st = unsafe { state() };
    if let Some(pump) = st.pump.as_mut() {
        match pump.read_register(answer.addr) {
            Ok(value) => answer.value = value,
            Err(err) => answer.error_code = pump_error_code(err),
        }
        let (writes, reads) = pump.counters();
        st.writes = writes;
        st.reads = reads;
    } else {
        answer.error_code = -1;
    }
    unsafe { write_output(request, &answer) };
}

/// Writes an LN8000 register (diagnostics).
///
/// # Safety
///
/// `request` is valid; the input buffer contains [`Ln8000RegRequest`].
unsafe fn handle_write_reg(request: WDFREQUEST, input_length: usize) {
    if input_length < core::mem::size_of::<Ln8000RegRequest>() {
        unsafe { complete(request, wdk_sys::STATUS_BUFFER_TOO_SMALL, 0) };
        return;
    }
    // SAFETY: the buffer has been checked by size; the address and the value come from
    // the client request.
    let Some(mut answer) = (unsafe { read_input::<Ln8000RegRequest>(request) }) else {
        unsafe { complete(request, wdk_sys::STATUS_INVALID_PARAMETER, 0) };
        return;
    };
    // SAFETY: access is serialized by WDF.
    let st = unsafe { state() };
    if let Some(pump) = st.pump.as_mut() {
        if let Err(err) = pump.write_register(answer.addr, answer.value) {
            answer.error_code = pump_error_code(err);
        }
        let (writes, reads) = pump.counters();
        st.writes = writes;
        st.reads = reads;
    } else {
        answer.error_code = -1;
    }
    unsafe { write_output(request, &answer) };
}

/// Sets the current and voltage limits.
///
/// # Safety
///
/// `request` is valid; the input buffer contains [`Ln8000LimitsRequest`].
unsafe fn handle_set_limits(request: WDFREQUEST, input_length: usize) {
    if input_length < core::mem::size_of::<Ln8000LimitsRequest>() {
        unsafe { complete(request, wdk_sys::STATUS_BUFFER_TOO_SMALL, 0) };
        return;
    }
    // SAFETY: the buffer has been checked by size.
    let Some(mut answer) = (unsafe { read_input::<Ln8000LimitsRequest>(request) }) else {
        unsafe { complete(request, wdk_sys::STATUS_INVALID_PARAMETER, 0) };
        return;
    };
    // SAFETY: access is serialized by WDF.
    let st = unsafe { state() };
    if let Some(pump) = st.pump.as_mut() {
        if answer.iin_ua > 0 {
            match pump.set_iin_limit(answer.iin_ua) {
                Ok(code) => {
                    // We report the actually applied current, not the requested
                    // one: the encoding rounds the value to a 50 mA step.
                    let applied = decode_iin_limit(code);
                    answer.applied_iin_ua = applied;
                    st.limits.iin_max_ua = applied;
                    st.limits.iin_target_ua = applied;
                }
                Err(err) => answer.error_code = pump_error_code(err),
            }
        }
        if answer.vbat_uv > 0 {
            if let Err(err) = pump.set_vbat_float(answer.vbat_uv) {
                answer.error_code = pump_error_code(err);
            }
        }
    } else {
        answer.error_code = -1;
    }
    unsafe { write_output(request, &answer) };
}

/// Switches the operating mode.
///
/// # Safety
///
/// `request` is valid; the input buffer contains [`Ln8000ModeRequest`].
unsafe fn handle_set_mode(request: WDFREQUEST, input_length: usize) {
    if input_length < core::mem::size_of::<Ln8000ModeRequest>() {
        unsafe { complete(request, wdk_sys::STATUS_BUFFER_TOO_SMALL, 0) };
        return;
    }
    // SAFETY: the buffer has been checked by size.
    let Some(mut answer) = (unsafe { read_input::<Ln8000ModeRequest>(request) }) else {
        unsafe { complete(request, wdk_sys::STATUS_INVALID_PARAMETER, 0) };
        return;
    };
    if !(1..=3).contains(&answer.mode) {
        unsafe { complete(request, wdk_sys::STATUS_INVALID_PARAMETER, 0) };
        return;
    }
    // SAFETY: access is serialized by WDF.
    let st = unsafe { state() };
    if let Some(pump) = st.pump.as_mut() {
        // mode 2 (1:1) is allowed only inside the bypass window: `EN_1TO1` feeds the
        // input straight to the battery, so with elevated Vin we reject with a
        // dedicated code rather than forwarding the call to the chip.
        if answer.mode == 2 {
            let vin = pump.read_adc(AdcChannel::Vin).unwrap_or(0);
            let vbat =
                u32::try_from(pump.read_adc(AdcChannel::Vbat).unwrap_or(0).max(0)).unwrap_or(0);
            if !bypass_allowed_by_vin(vin, vbat) {
                println!(
                    "ln8000-kmdf: SET_MODE bypass rejected: Vin {vin} µV outside the bypass window"
                );
                answer.error_code = ioctl::ERR_BYPASS_VIN_OUT_OF_WINDOW;
                unsafe { write_output(request, &answer) };
                return;
            }
        }
        let outcome = match answer.mode {
            1 => pump.standby().map(|()| OpMode::Standby.code()),
            2 => pump.enable_bypass().map(|mode| mode.code()),
            _ => pump.enable_switching().map(|mode| mode.code()),
        };
        match outcome {
            Ok(mode) => answer.applied_mode = mode,
            Err(err) => answer.error_code = pump_error_code(err),
        }
    } else {
        answer.error_code = -1;
    }
    unsafe { write_output(request, &answer) };
}

/// Explicit charge start/stop via [`IOCTL_LN8000_SET_CHARGE`].
///
/// # Safety
///
/// `request` is valid; the input/output buffer is large enough.
unsafe fn handle_set_charge(request: WDFREQUEST, input_length: usize) {
    if input_length < core::mem::size_of::<Ln8000ChargeRequest>() {
        unsafe { complete(request, wdk_sys::STATUS_BUFFER_TOO_SMALL, 0) };
        return;
    }
    // SAFETY: the buffer has been checked by size.
    let Some(mut answer) = (unsafe { read_input::<Ln8000ChargeRequest>(request) }) else {
        unsafe { complete(request, wdk_sys::STATUS_INVALID_PARAMETER, 0) };
        return;
    };
    // SAFETY: access to the global WDF state.
    let st = unsafe { state() };
    let queue = unsafe { call_unsafe_wdf_function_binding!(WdfRequestGetIoQueue, request) };
    let device = unsafe { call_unsafe_wdf_function_binding!(WdfIoQueueGetDevice, queue) };
    if let Some(pump) = st.pump.as_mut() {
        let result = if answer.on != 0 {
            // Soft-reset + configure already try to enable charging in this tick.
            let outcome = match recover_ln_shutdown(pump) {
                ShutdownRecovery::Recovered(outcome) => outcome,
                ShutdownRecovery::NotInShutdown => pump.set_charging(true, &mut por_delay),
            };
            st.charge_attempts = st.charge_attempts.saturating_add(1);
            st.last_enable_ms = monotonic_ms();
            mark_charge_attempt(device, st.charge_attempts, st.last_enable_ms, &outcome);
            outcome
        } else {
            pump.set_charging(false, &mut por_delay)
        };
        match result {
            Ok(applied) => {
                answer.applied_mode = applied.code();
                answer.sys_sts = pump.status().map_or(0, |s| s.sys_sts);
                answer.error_code = 0;
                st.last_error = 0;
            }
            Err(err) => {
                let code = pump_error_code(err);
                answer.error_code = code;
                st.last_error = code;
            }
        }
    } else {
        answer.error_code = -1;
    }
    unsafe { write_output(request, &answer) };
}

/// Returns information about the charge sessions.
///
/// # Safety
///
/// `request` is valid; the output is checked by the caller.
unsafe fn handle_get_sessions(request: WDFREQUEST) {
    // SAFETY: access is serialized by WDF.
    let st = unsafe { state() };
    let mut answer = Ln8000Sessions {
        total: st.telemetry.session_total(),
        ..Ln8000Sessions::default()
    };
    let now = monotonic_ms();
    if let Some(current) = st.telemetry.current() {
        answer.current_ms = current.duration_ms(now);
        answer.current_peak_iin_ua = current.peak_iin_ua;
        answer.current_fast = u8::from(current.had_fast_mode());
    }
    if let Some(last) = st.telemetry.last_completed() {
        answer.last_ms = last.duration_ms(now);
        answer.last_peak_iin_ua = last.peak_iin_ua;
        answer.last_peak_temp_dc = last.peak_die_temp_dc;
        answer.last_fast = u8::from(last.had_fast_mode());
    }
    unsafe { write_output(request, &answer) };
}

/// Returns the telemetry samples.
///
/// # Safety
///
/// `request` is valid; the output buffer is filled element by element.
unsafe fn handle_get_samples(request: WDFREQUEST, input_length: usize) {
    if input_length < core::mem::size_of::<Ln8000SamplesRequest>() {
        unsafe { complete(request, wdk_sys::STATUS_BUFFER_TOO_SMALL, 0) };
        return;
    }
    // SAFETY: the buffer has been checked by size.
    let requested_count = unsafe { read_input::<Ln8000SamplesRequest>(request) };
    // SAFETY: access is serialized by WDF.
    let st = unsafe { state() };
    let (buffer, length) = match unsafe {
        output_buffer(request, core::mem::size_of::<Ln8000SamplesRequest>())
    } {
        Ok(value) => value,
        Err(status) => {
            unsafe { complete(request, status, 0) };
            return;
        }
    };
    let header_size = core::mem::size_of::<Ln8000SamplesRequest>();
    let sample_size = core::mem::size_of::<Ln8000Sample>();
    if sample_size == 0 {
        unsafe { complete(request, wdk_sys::STATUS_INVALID_PARAMETER, 0) };
        return;
    }
    let capacity = length.saturating_sub(header_size) / sample_size;
    let requested = requested_count
        .map_or(capacity, |info| usize::try_from(info.count).unwrap_or(capacity));
    let limit = capacity.min(requested).min(ln8000::session::SAMPLE_RING);
    let base = buffer.cast::<u8>();
    let mut written = 0_usize;
    st.telemetry.for_each_sample(|sample| {
        if written < limit {
            // SAFETY: the write stays inside the request output buffer, and
            // `written < limit <= capacity` keeps it within the bounds.
            unsafe {
                let destination = base
                    .add(header_size + written * sample_size)
                    .cast::<Ln8000Sample>();
                core::ptr::write(destination, to_sample(sample));
            }
            written = written.saturating_add(1);
        }
    });
    let mut header = Ln8000SamplesRequest {
        count: u32::try_from(limit).unwrap_or(0),
        available: u32::try_from(written).unwrap_or(0),
        ..Ln8000SamplesRequest::default()
    };
    if written > 0 {
        // SAFETY: the first written sample lies right after the header.
        unsafe {
            core::ptr::copy_nonoverlapping(
                base.add(header_size),
                core::ptr::from_mut(&mut header.first).cast::<u8>(),
                sample_size,
            );
        }
    }
    // SAFETY: the header is written at the start of the request output buffer.
    unsafe {
        core::ptr::copy_nonoverlapping(core::ptr::from_ref(&header).cast::<u8>(), base, header_size);
    }
    let information = header_size + written * sample_size;
    unsafe { complete(request, wdk_sys::STATUS_SUCCESS, information) };
}

/// Converts a core sample into the response structure.
fn to_sample(sample: &TelemetrySample) -> Ln8000Sample {
    Ln8000Sample {
        ts_ms: sample.ts_ms,
        vbat_uv: sample.vbat_uv,
        vbus_uv: sample.vbus_uv,
        iin_ua: sample.iin_ua,
        die_temp_dc: sample.die_temp_dc,
        op_mode: sample.op_mode.code(),
        input_present: u8::from(sample.input_present),
        reserved: [0; 2],
    }
}

/// Copies the request input buffer into a structure.
///
/// Returns `None` if the buffer is smaller than `size_of::<T>()`.
///
/// # Safety
///
/// `request` is valid; called at a level that permits access to the request buffer.
unsafe fn read_input<T: Copy + Default>(request: WDFREQUEST) -> Option<T> {
    let mut buffer: *mut core::ffi::c_void = core::ptr::null_mut();
    let mut length: usize = 0;
    // SAFETY: the request input buffer is created by the framework (METHOD_BUFFERED).
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfRequestRetrieveInputBuffer,
            request,
            core::mem::size_of::<T>(),
            &raw mut buffer,
            &raw mut length,
        )
    };
    if status < 0 || buffer.is_null() || length < core::mem::size_of::<T>() {
        return None;
    }
    let mut value = T::default();
    // SAFETY: the buffer has been checked by size; we copy exactly size_of::<T>().
    unsafe {
        core::ptr::copy_nonoverlapping(
            buffer.cast::<u8>(),
            core::ptr::from_mut(&mut value).cast::<u8>(),
            core::mem::size_of::<T>(),
        );
    }
    Some(value)
}

/// Returns the request output buffer and its size.
///
/// # Safety
///
/// `request` is valid.
unsafe fn output_buffer(
    request: WDFREQUEST,
    min_length: usize,
) -> Result<(*mut core::ffi::c_void, usize), NTSTATUS> {
    let mut buffer: *mut core::ffi::c_void = core::ptr::null_mut();
    let mut length: usize = 0;
    // SAFETY: the request output buffer is created by the framework (METHOD_BUFFERED).
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfRequestRetrieveOutputBuffer,
            request,
            min_length,
            &raw mut buffer,
            &raw mut length,
        )
    };
    if status < 0 {
        return Err(status);
    }
    Ok((buffer, length))
}

/// Copies the structure into the request output buffer and completes it.
///
/// # Safety
///
/// `request` is valid; `value` points to a live structure.
unsafe fn write_output<T: Copy>(request: WDFREQUEST, value: &T) {
    let (buffer, _length) = match unsafe { output_buffer(request, core::mem::size_of::<T>()) } {
        Ok(value) => value,
        Err(status) => {
            unsafe { complete(request, status, 0) };
            return;
        }
    };
    // SAFETY: the buffer has been checked by size; we copy exactly size_of::<T>().
    unsafe {
        core::ptr::copy_nonoverlapping(
            core::ptr::from_ref(value).cast::<u8>(),
            buffer.cast::<u8>(),
            core::mem::size_of::<T>(),
        );
    }
    unsafe { complete(request, wdk_sys::STATUS_SUCCESS, core::mem::size_of::<T>()) };
}

/// Completes the request with a status and an information volume.
///
/// # Safety
///
/// `request` is a valid, not yet completed request.
unsafe fn complete(request: WDFREQUEST, status: NTSTATUS, information: usize) {
    // SAFETY: the request belongs to this call and is not completed yet.
    unsafe {
        call_unsafe_wdf_function_binding!(
            WdfRequestCompleteWithInformation,
            request,
            status,
            u64::try_from(information).unwrap_or(0),
        );
    }
}

/// Monotonic kernel milliseconds.
fn monotonic_ms() -> u64 {
    let mut stamp: u64 = 0;
    // SAFETY: `KeQueryInterruptTimePrecise` is a documented kernel routine;
    // the unit is 100 ns, so we divide by 10 000.
    let ticks = unsafe { wdk_sys::ntddk::KeQueryInterruptTimePrecise(&raw mut stamp) };
    ticks / 10_000
}

fn pump_error_code(err: PumpError) -> i32 {
    match err {
        PumpError::Bus(_) => -1,
        PumpError::NotOpen => -2,
        PumpError::WrongDeviceId { .. } => -3,
        PumpError::ModeNotReached { .. } => -4,
        PumpError::Fault { .. } => -5,
        PumpError::OutOfRange { .. } => -6,
        PumpError::WatchdogExpired => -7,
        // A policy refusal, not a chip failure: 1:1 outside the bypass window.
        PumpError::BypassNeedsFiveVoltVin { .. } => ioctl::ERR_BYPASS_VIN_OUT_OF_WINDOW,
        // The enum is marked `non_exhaustive`: new variants will give -100.
        _ => -100,
    }
}

fn size_of_ulong<T>() -> ULONG {
    u32::try_from(core::mem::size_of::<T>()).unwrap_or(0)
}
