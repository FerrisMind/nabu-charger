//! Battery class miniport for tray / Settings SoC.
//!
//! Xiaomi `qcbattminiclass` stays OK but never publishes `GUID_DEVICE_BATTERY`.
//! Attach BattC to this LN8000 FDO (simbatt pattern) and estimate SoC from VBAT.
//!
//! Settings / `Win32_Battery` also need the WMI path that simbatt wires:
//! `IRP_MJ_SYSTEM_CONTROL` → `BatteryClassSystemControl` plus
//! `IoWMIRegistrationControl(REGISTER)`.

use core::ptr;
use ln8000::battery_policy::{
    self, CHARGING_HOLD_MS, Hold, ONLINE_HOLD_MS, SocTrend, charging_raw,
};
use wdk::println;
use wdk_sys::{
    call_unsafe_wdf_function_binding, IRP_MJ_DEVICE_CONTROL, IRP_MJ_SYSTEM_CONTROL, NTSTATUS,
    PDEVICE_OBJECT, PIRP, PVOID, PWDFDEVICE_INIT, ULONG, UNICODE_STRING, WDFDEVICE,
};

/// BattC class handle from [`BatteryClassInitializeDevice`].
static mut CLASS_HANDLE: PVOID = ptr::null_mut();
/// FDO / PDO captured at init (WMI callbacks need them).
static mut FDO: PDEVICE_OBJECT = ptr::null_mut();
static mut PDO: PDEVICE_OBJECT = ptr::null_mut();
/// True after a successful `IoWMIRegistrationControl(REGISTER)`.
static mut WMI_REGISTERED: bool = false;

/// Driver registry path copy for `QueryWmiRegInfo` (simbatt).
static mut REGISTRY_PATH_BUF: [u16; 260] = [0; 260];
static mut REGISTRY_PATH: UNICODE_STRING = UNICODE_STRING {
    Length: 0,
    MaximumLength: 0,
    Buffer: ptr::null_mut(),
};

/// Last published relative capacity (0–100) or [`BATTERY_UNKNOWN_CAPACITY`].
///
/// Must **not** start at 100. Before the first trustworthy sample the tray and
/// Settings showed a full battery on a pack Android reported at 12 % — the
/// value was simply the initialiser, because VBAT is only sampled while
/// telemetry runs and the OCV map was also feeding it a charging rail.
static mut LAST_PCT: u32 = BATTERY_UNKNOWN_CAPACITY;
/// Percent the class has already been notified about ([`BATTERY_UNKNOWN_CAPACITY`]
/// before the first one).
///
/// Windows re-reads the battery on our [`BatteryClassStatusNotify`], not on its own
/// schedule. While the notification fired only on a `power_state` change, the tray
/// indicator during discharge stood still: live measurement on 19.09 13:35-13:45
/// (pack disconnected) - the driver counter went 162 -> 158 (63 -> 62 %), while
/// `GetSystemPowerStatus` showed 64 for all eight minutes, that is the value taken
/// at the previous notification; it refreshed only after rebooting into Android and
/// back. So we notify on a percent change as well.
static mut NOTIFIED_PCT: u32 = BATTERY_UNKNOWN_CAPACITY;
/// Where [`LAST_PCT`] came from (mark `SocSrc`).
static mut SOC_SRC: u32 = SOC_SRC_NONE;
/// Consecutive gauge read failures (mark `SocFail`); a success resets it.
static mut GAUGE_FAILS: u32 = 0;

/// Percent not obtained from anywhere yet.
pub const SOC_SRC_NONE: u32 = 0;
/// Percent read from the PM8150B fuel gauge (`FG_MONOTONIC_SOC`).
pub const SOC_SRC_GAUGE: u32 = 1;
/// Percent estimated from the cell voltage (linear map, only outside charging).
pub const SOC_SRC_VBAT: u32 = 2;
/// Last VBAT sample (µV) used for SoC.
static mut LAST_VBAT_UV: u32 = 0;
/// Last VBUS sample (µV) — raw input of the online hysteresis.
static mut LAST_VBUS_UV: u32 = 0;
/// Last IIN sample (µA) — raw input of the charging hysteresis.
static mut LAST_IIN_UA: u32 = 0;
/// Hysteresis of the "adapter online" flag: a single tick below the Vin threshold
/// does not clear `POWER_ON_LINE` (see [`ONLINE_HOLD_MS`]).
///
/// Front-armed: the tick that sees the adapter publishes AC, because the HVDCP
/// bring-up blocks the telemetry callback for seconds and would otherwise leave the
/// verdict waiting for a second sample it cannot get (see
/// [`ln8000::battery_policy::HOLD_FRONT_RUN`]; measured 23.09: 8,7 s from cable to AC).
static mut ONLINE_HOLD: Hold = Hold::front_armed();
/// Hysteresis of the "charging" flag: Iin drops to the ADC floor (39 mA) on every
/// QC3 pulse and mode transition, so the hold is longer - [`CHARGING_HOLD_MS`].
///
/// Run-armed, unlike the online flag: the charging witness carries history (a window
/// peak and a SOC rise up to [`ln8000::battery_policy::SOC_RISE_HOLD_MS`] old) and
/// cannot turn true before the bring-up has run, so a second sample costs it nothing.
static mut CHARGING_HOLD: Hold = Hold::new();
/// Consecutive ticks the hardware's "VBUS gone" verdict has held with no current.
///
/// Fed through [`battery_policy::unplug_release`] on every tick: it is what separates a
/// real removal, where the bit asserts and stays asserted for hours, from the 290 ms
/// windows the driver's own engagement pulses create on a working brick.
static mut UNPLUG_RUN: u32 = 0;

/// The fuel counter's SOC and whether it is on the way up - the direction witness for
/// `CHARGING` (see [`ln8000::battery_policy::SocTrend`]).
///
/// It lives here, not in the driver state, because the sample arrives through
/// [`set_gauge_raw`] on the same 30 s cadence as the percent.
static mut SOC_TREND: SocTrend = SocTrend::new();
/// Last published BattC power_state flags.
static mut LAST_POWER_STATE: u32 = 0;
/// Battery tag (non-zero = present).
static mut BATTERY_TAG: u32 = 1;

const BATTERY_CLASS_MAJOR_VERSION: u16 = 1;
const BATTERY_CLASS_MINOR_VERSION_1: u16 = 1;

const BATTERY_TAG_INVALID: u32 = 0;
const BATTERY_SYSTEM_BATTERY: u32 = 0x8000_0000;
const BATTERY_POWER_ON_LINE: u32 = 0x0000_0001;
const BATTERY_DISCHARGING: u32 = 0x0000_0002;
const BATTERY_CHARGING: u32 = 0x0000_0004;
const BATTERY_CRITICAL: u32 = 0x0000_0008;
const BATTERY_UNKNOWN_RATE: u32 = 0x8000_0000;
const BATTERY_UNKNOWN_TIME: u32 = 0xFFFF_FFFF;
/// Relative capacity is not known (WDK `BATTERY_UNKNOWN_CAPACITY`).
///
/// Sent instead of a number when no trustworthy sample exists: the class
/// driver then shows no percentage rather than a fabricated one. Zero would
/// mean "empty", which is worse — `0` is a valid reading, "unknown" is not.
const BATTERY_UNKNOWN_CAPACITY: u32 = 0xFFFF_FFFF;

const WMIREG_ACTION_REGISTER: u32 = 1;
const WMIREG_ACTION_DEREGISTER: u32 = 2;
const WMIREG_FLAG_INSTANCE_PDO: u32 = 0x0000_0020;
const STATUS_WMI_GUID_NOT_FOUND: NTSTATUS = 0xC000_0295u32 as i32;
const IO_NO_INCREMENT: i8 = 0;

/// Empty / full OCV anchors for nabu Li-ion (µV).
const VBAT_EMPTY_UV: u32 = 3_400_000;
const VBAT_FULL_UV: u32 = 4_350_000;

#[repr(C)]
#[derive(Clone, Copy)]
struct BatteryInformation {
    capabilities: u32,
    technology: u8,
    reserved: [u8; 3],
    chemistry: [u8; 4],
    designed_capacity: u32,
    full_charged_capacity: u32,
    default_alert1: u32,
    default_alert2: u32,
    critical_bias: u32,
    cycle_count: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct BatteryStatus {
    power_state: u32,
    capacity: u32,
    voltage: u32,
    rate: i32,
}

#[repr(C)]
struct BatteryMiniportInfoV11 {
    major_version: u16,
    minor_version: u16,
    context: PVOID,
    query_tag: Option<
        unsafe extern "C" fn(context: PVOID, battery_tag: *mut ULONG) -> NTSTATUS,
    >,
    query_information: Option<
        unsafe extern "C" fn(
            context: PVOID,
            battery_tag: ULONG,
            level: u32,
            at_rate: i32,
            buffer: PVOID,
            buffer_length: ULONG,
            returned_length: *mut ULONG,
        ) -> NTSTATUS,
    >,
    set_information: Option<
        unsafe extern "C" fn(
            context: PVOID,
            battery_tag: ULONG,
            level: u32,
            buffer: PVOID,
        ) -> NTSTATUS,
    >,
    query_status: Option<
        unsafe extern "C" fn(
            context: PVOID,
            battery_tag: ULONG,
            battery_status: *mut BatteryStatus,
        ) -> NTSTATUS,
    >,
    set_status_notify: Option<
        unsafe extern "C" fn(context: PVOID, battery_tag: ULONG, notify: PVOID) -> NTSTATUS,
    >,
    disable_status_notify: Option<unsafe extern "C" fn(context: PVOID) -> NTSTATUS>,
    pdo: PDEVICE_OBJECT,
    device_name: *mut wdk_sys::UNICODE_STRING,
    fdo: PDEVICE_OBJECT,
}

/// Matches `WMILIB_CONTEXT` in wmilib.h.
#[repr(C)]
struct WmiLibContext {
    guid_count: ULONG,
    guid_list: PVOID,
    query_wmi_reg_info: Option<
        unsafe extern "C" fn(
            device_object: PDEVICE_OBJECT,
            reg_flags: *mut ULONG,
            instance_name: *mut UNICODE_STRING,
            registry_path: *mut *mut UNICODE_STRING,
            mof_resource_name: *mut UNICODE_STRING,
            pdo: *mut PDEVICE_OBJECT,
        ) -> NTSTATUS,
    >,
    query_wmi_data_block: Option<
        unsafe extern "C" fn(
            device_object: PDEVICE_OBJECT,
            irp: PIRP,
            guid_index: ULONG,
            instance_index: ULONG,
            instance_count: ULONG,
            instance_length_array: *mut ULONG,
            buffer_avail: ULONG,
            buffer: *mut u8,
        ) -> NTSTATUS,
    >,
    set_wmi_data_block: PVOID,
    set_wmi_data_item: PVOID,
    execute_wmi_method: PVOID,
    wmi_function_control: PVOID,
}

#[repr(i32)]
#[derive(Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
// The names mirror the WMI disposition values from the WDK headers: the `Irp`
// prefix carries meaning here (`SYSCTL_IRP_*`), so the common prefix is not a mistake.
#[allow(clippy::enum_variant_names)]
enum SysctlIrpDisposition {
    IrpProcessed = 0,
    IrpNotCompleted = 1,
    IrpNotWmi = 2,
    IrpForward = 3,
}

static mut WMI_CONTEXT: WmiLibContext = WmiLibContext {
    guid_count: 0,
    guid_list: ptr::null_mut(),
    query_wmi_reg_info: Some(query_wmi_reg_info),
    query_wmi_data_block: Some(query_wmi_data_block),
    set_wmi_data_block: ptr::null_mut(),
    set_wmi_data_item: ptr::null_mut(),
    execute_wmi_method: ptr::null_mut(),
    wmi_function_control: ptr::null_mut(),
};

#[link(name = "battc")]
unsafe extern "C" {
    fn BatteryClassInitializeDevice(
        miniport_info: *mut BatteryMiniportInfoV11,
        class_data: *mut PVOID,
    ) -> NTSTATUS;
    fn BatteryClassUnload(class_data: PVOID) -> NTSTATUS;
    fn BatteryClassIoctl(class_data: PVOID, irp: PIRP) -> NTSTATUS;
    fn BatteryClassStatusNotify(class_data: PVOID) -> NTSTATUS;
    fn BatteryClassSystemControl(
        class_data: PVOID,
        wmi_lib_context: PVOID,
        device_object: PDEVICE_OBJECT,
        irp: PIRP,
        disposition: *mut i32,
    ) -> NTSTATUS;
    fn BatteryClassQueryWmiDataBlock(
        class_data: PVOID,
        device_object: PDEVICE_OBJECT,
        irp: PIRP,
        guid_index: ULONG,
        instance_length_array: *mut ULONG,
        out_buffer_size: ULONG,
        buffer: *mut u8,
    ) -> NTSTATUS;
}

#[link(name = "wmilib")]
unsafe extern "C" {
    fn WmiCompleteRequest(
        device_object: PDEVICE_OBJECT,
        irp: PIRP,
        status: NTSTATUS,
        buffer_used: ULONG,
        priority_boost: i8,
    ) -> NTSTATUS;
}

unsafe extern "C" {
    fn IoWMIRegistrationControl(device_object: PDEVICE_OBJECT, action: ULONG) -> NTSTATUS;
    fn IoCompleteRequest(irp: PIRP, priority_boost: i8);
}

/// Keep a durable copy of the driver registry path for WMI (simbatt).
pub unsafe fn set_registry_path(registry_path: wdk_sys::PCUNICODE_STRING) {
    if registry_path.is_null() {
        return;
    }
    let src = unsafe { &*registry_path };
    if src.Buffer.is_null() || src.Length == 0 {
        return;
    }
    let chars = (src.Length as usize) / 2;
    let n = core::cmp::min(chars, 259);
    let buf = core::ptr::addr_of_mut!(REGISTRY_PATH_BUF).cast::<u16>();
    unsafe {
        ptr::copy_nonoverlapping(src.Buffer, buf, n);
        *buf.add(n) = 0;
        REGISTRY_PATH = UNICODE_STRING {
            Length: (n * 2) as u16,
            MaximumLength: ((n + 1) * 2) as u16,
            Buffer: buf,
        };
    }
}

/// Register IRP preprocess callbacks BattC needs (IOCTL + WMI system control).
///
/// Must run **before** `WdfDeviceCreate` (simbatt).
pub unsafe fn assign_ioctl_preprocess(device_init: PWDFDEVICE_INIT) -> NTSTATUS {
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceInitAssignWdmIrpPreprocessCallback,
            device_init,
            Some(evt_wdm_irp_preprocess_device_control),
            IRP_MJ_DEVICE_CONTROL as u8,
            ptr::null_mut(),
            0,
        )
    };
    if status < 0 {
        return status;
    }
    unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceInitAssignWdmIrpPreprocessCallback,
            device_init,
            Some(evt_wdm_irp_preprocess_system_control),
            IRP_MJ_SYSTEM_CONTROL as u8,
            ptr::null_mut(),
            0,
        )
    }
}

/// Attach BattC after the FDO exists (PrepareHardware).
pub unsafe fn initialize(device: WDFDEVICE) -> NTSTATUS {
    if !unsafe { CLASS_HANDLE }.is_null() {
        return wdk_sys::STATUS_SUCCESS;
    }

    let fdo = unsafe {
        call_unsafe_wdf_function_binding!(WdfDeviceWdmGetDeviceObject, device)
    };
    let pdo = unsafe {
        call_unsafe_wdf_function_binding!(WdfDeviceWdmGetPhysicalDevice, device)
    };
    if fdo.is_null() || pdo.is_null() {
        println!("ln8000-kmdf: battery: null FDO/PDO");
        return wdk_sys::STATUS_INVALID_DEVICE_STATE;
    }

    let mut info = BatteryMiniportInfoV11 {
        major_version: BATTERY_CLASS_MAJOR_VERSION,
        minor_version: BATTERY_CLASS_MINOR_VERSION_1,
        context: ptr::null_mut(),
        query_tag: Some(query_tag),
        query_information: Some(query_information),
        set_information: Some(set_information),
        query_status: Some(query_status),
        set_status_notify: Some(set_status_notify),
        disable_status_notify: Some(disable_status_notify),
        pdo,
        device_name: ptr::null_mut(),
        fdo,
    };

    let mut handle: PVOID = ptr::null_mut();
    let status = unsafe { BatteryClassInitializeDevice(&mut info, &mut handle) };
    crate::mark_device_value(device, "BattClassSt", status as u32);
    if status < 0 {
        println!("ln8000-kmdf: BatteryClassInitializeDevice failed: {status:#010X}");
        return status;
    }
    unsafe {
        CLASS_HANDLE = handle;
        FDO = fdo;
        PDO = pdo;
    }
    crate::mark_device_value(device, "BattClassOk", 1);

    // WMI registration is what feeds Settings / Win32_Battery (simbatt).
    let wmi_st = unsafe { IoWMIRegistrationControl(fdo, WMIREG_ACTION_REGISTER) };
    crate::mark_device_value(device, "BattWmiSt", wmi_st as u32);
    if wmi_st >= 0 {
        unsafe { WMI_REGISTERED = true };
        crate::mark_device_value(device, "BattWmiOk", 1);
        println!("ln8000-kmdf: battery WMI registered");
    } else {
        println!("ln8000-kmdf: IoWMIRegistrationControl failed: {wmi_st:#010X}");
    }

    println!("ln8000-kmdf: battery class attached");
    wdk_sys::STATUS_SUCCESS
}

/// Detach BattC on release.
pub unsafe fn unload() {
    let fdo = unsafe { FDO };
    if unsafe { WMI_REGISTERED } && !fdo.is_null() {
        unsafe {
            let _ = IoWMIRegistrationControl(fdo, WMIREG_ACTION_DEREGISTER);
            WMI_REGISTERED = false;
        }
    }
    let handle = unsafe { CLASS_HANDLE };
    if handle.is_null() {
        return;
    }
    unsafe {
        let _ = BatteryClassUnload(handle);
        CLASS_HANDLE = ptr::null_mut();
        FDO = ptr::null_mut();
        PDO = ptr::null_mut();
    }
}

/// Update cached samples and wake waiting status IRPs.
///
/// Percent is only recomputed from a pack that the charger is **not** driving.
/// With the adapter online, LN8000's VBAT follows the charge rail (float at
/// 4.42 V), so the linear OCV map saturates at 100 % on a nearly empty pack —
/// that is the W11 bug (Windows showed 100 %, Android on the same battery
/// 12 %). While charging we therefore keep the last offline estimate, and
/// `BATTERY_UNKNOWN_CAPACITY` if there never was one.
///
/// Both reported flags are **held**: `POWER_ON_LINE` follows the online hold and
/// `CHARGING` the charging hold (see [`ONLINE_HOLD_MS`] / [`CHARGING_HOLD_MS`]),
/// so one tick at the ADC floor or below the Vin threshold neither clears a flag
/// nor fires [`BatteryClassStatusNotify`].
///
/// Ignores `vbat_uv == 0` (ADC not ready) so a bad first sample cannot pin SoC at 0%.
/// Calls [`BatteryClassStatusNotify`] when `power_state` **or** the published
/// percentage changes, so the tray/Settings see plug/unplug on the same telemetry
/// tick (not after a later poll) and keep counting down while discharging — the
/// class driver re-reads the miniport on our notification, not on its own timer.
///
/// `iin_peak_ua` is the peak Iin over the observation window (`DriverState::max_iin_ua`),
/// not an instantaneous sample: on a QC3 pulse or mode transition tick Iin sits at the
/// ADC floor (39 mA), and without the peak the charging flag would drop on every
/// pulse. `now_ms` is the monotonic time of the same tick.
///
/// `input_readings_usable` says whether the tick carries any input measurement at
/// all. A zero in this telemetry sample means "no sample", not "0 V": a bus failure
/// collapses to zero on the caller side, and the LN8000 ADC goes into auto
/// hibernation after 4 s of idle and then reads **successfully** but with `0x00` in
/// every channel (`encoding::vbat_reading_usable`). Such a tick has no right to say
/// "0 V at the input, so the pack is gone" and does not age the hold - except when
/// the hardware itself set `FAULT1` bit 4: then the pack really is disconnected
/// (`battery_policy::online_raw_with_evidence`), and with no current flowing the
/// window ends on that tick rather than after `ONLINE_HOLD_MS`.
///
/// `soc_rising` is the fuel counter's direction for this tick
/// ([`ln8000::battery_policy::SocTrend`], fed by [`set_gauge_raw`]): the pack's own SOC
/// has risen recently. It is the only charging evidence that exists while the pump is
/// idle, because LN8000 `Iin` measures the pump's own input and sits on the 39 mA floor
/// when the platform buck carries the charge. Live 22.09: `Vin = 4.416 V`, `Iin` on the
/// floor, the buck charging at ~1.5 A with the operator's meter showing it, and the tray
/// published "on battery" for the whole session. The counter's cell current is *not* used
/// for this: its sign was measured not to discriminate the two directions on this board
/// (see the note on `ChargeRegs::ibatt_ua`).
pub unsafe fn update_from_telemetry(
    vbat_uv: u32,
    vbus_uv: u32,
    iin_ua: u32,
    iin_peak_ua: u32,
    vac_unplug: bool,
    input_readings_usable: bool,
    soc_rising: bool,
    now_ms: u64,
) {
    let prev_power = unsafe { LAST_POWER_STATE };
    if vbat_uv > 0 {
        unsafe {
            LAST_VBAT_UV = vbat_uv;
        }
    }
    // The counter is more trustworthy than any estimate: while it answers, the
    // cell voltage is not used in the calculation at all.
    //
    // `None` is a tick with no input readings and no hardware verdict: it does not
    // age the hold, so no cell-based estimate is published on it either. "No idea"
    // is not proof of discharge, and `soc_percent(0)` on such a tick would give zero.
    let raw_online = battery_policy::online_raw_with_evidence(
        vbus_uv,
        vbat_uv,
        iin_ua,
        vac_unplug,
        input_readings_usable,
    );
    if vbat_uv > 0 && unsafe { SOC_SRC } != SOC_SRC_GAUGE && matches!(raw_online, Some(false)) {
        unsafe {
            LAST_PCT = soc_percent(vbat_uv);
            SOC_SRC = SOC_SRC_VBAT;
        }
    }
    // The hysteresis is computed from the raw flags of the tick, while the held
    // value is written to the state: a raw Vin/Iin removal lasts one tick, and the
    // tray must not re-read the input on every QC3 pulse.
    // A hardware unplug with no current into the pack ends the online window on the
    // tick it is confirmed instead of after `ONLINE_HOLD_MS`. Measured on 22.09: the
    // removal was visible in the telemetry in under a second and Windows was told about
    // it 7-8 s later, every second of which was this hold; `FAULT1` bit 4 asserted on
    // both real removals and never once in 8 685 samples of steady charging. Two
    // guards keep it honest, and both are measurements rather than caution:
    //
    // * current into the pack wins outright, so a stale bit cannot drop the input while
    //   the pack is really taking charge;
    // * the verdict must hold for `UNPLUG_RELEASE_RUN` consecutive ticks - three, because
    //   at 20:34 the same bit came and went in 290 ms windows every 15-40 s with a working
    //   5 V brick attached (the driver's own engagement pulses collapse the adapter's
    //   output) and a 290 ms window is longer than the 250 ms telemetry tick, so two
    //   samples fit inside one window. Releasing on the first of those dropped
    //   `POWER_ON_LINE` on every pulse, and with the pump unable to reach its mode there
    //   was no current to re-arm it - the tray stayed dark for the whole session. The SOC
    //   trend is deliberately not part of this test: a rise can be up to
    //   `SOC_RISE_HOLD_MS` old, and a history cannot outvote the live verdict of a
    //   removal. It is gated on `online_hold.held` instead, so it can never outlive one.
    //
    // The charging hold is deliberately left alone: `charging` is gated on `online`, so
    // the icon drops with it, and an armed charging hold is what lets the icon come back
    // quickly when the cable returns.
    let (hardware_unplug, unplug_run) =
        battery_policy::unplug_release(vac_unplug, iin_ua, unsafe { UNPLUG_RUN });
    unsafe { UNPLUG_RUN = unplug_run };
    let online_hold = if hardware_unplug {
        // `hold_ms = 0`: the window ends on this tick.
        unsafe { ONLINE_HOLD }.update(false, now_ms, 0)
    } else {
        unsafe { ONLINE_HOLD }.update_evidence(raw_online, now_ms, ONLINE_HOLD_MS)
    };
    let charging_hold = unsafe { CHARGING_HOLD }.update(
        charging_raw(online_hold.held, iin_ua, iin_peak_ua, soc_rising),
        now_ms,
        CHARGING_HOLD_MS,
    );
    unsafe {
        ONLINE_HOLD = online_hold;
        CHARGING_HOLD = charging_hold;
        LAST_VBUS_UV = vbus_uv;
        LAST_IIN_UA = iin_ua;
        let _ = build_status();
    }
    let new_power = unsafe { LAST_POWER_STATE };
    let new_pct = unsafe { LAST_PCT };
    // A percent change is as good a reason to re-read the battery as a power
    // change: during discharge `power_state` is constant, and without this condition
    // the indicator froze (see [`NOTIFIED_PCT`]). The comparison is against the
    // **notified** value, not the previous [`LAST_PCT`]: if the class is not attached
    // yet, the notification repeats on the next tick instead of being lost.
    if new_power == prev_power && new_pct == unsafe { NOTIFIED_PCT } {
        return;
    }
    let handle = unsafe { CLASS_HANDLE };
    if !handle.is_null() {
        unsafe {
            NOTIFIED_PCT = new_pct;
            let _ = BatteryClassStatusNotify(handle);
        }
    }
}

/// Publishes the percent from the fuel gauge: `raw` is the raw `FG_MONOTONIC_SOC`.
///
/// The conversion is taken verbatim from Android (`fg_get_msoc`): `255` is exactly
/// 100 %, `0` is exactly 0, and intermediate `1...254` are stretched onto `1...99`
/// (`DIV_ROUND_CLOSEST((raw - 1) * 98, 253) + 1`). The linear `raw * 100 / 255`
/// diverges from it already at the edges of a nearly empty cell: at `raw = 1` it
/// gives 0 % instead of 1 %, which would make Windows show "empty" on a cell that
/// Android still holds a percent on.
///
/// The same sample feeds [`SOC_TREND`], which is the direction witness for the charging
/// flag: the raw counter falls while the pack drains and rises while it fills, and it is
/// the only direction this platform can give (the cell current's sign was measured not to
/// discriminate - see [`update_from_telemetry`]). `now_ms` is the monotonic time of the
/// telemetry tick that carried the sample.
pub unsafe fn set_gauge_raw(raw: u8, now_ms: u64) {
    let pct = if raw == 255 {
        100
    } else if raw == 0 {
        0
    } else {
        // SAFETY: values 1...254 give at most 99; 98 * 253 + 126 < u32::MAX.
        (((u32::from(raw) - 1) * 98 + 126) / 253) + 1
    };
    unsafe {
        LAST_PCT = pct;
        SOC_SRC = SOC_SRC_GAUGE;
        GAUGE_FAILS = 0;
        SOC_TREND = SOC_TREND.update(raw, now_ms);
    }
}

/// Whether the fuel counter's SOC has risen recently ([`SOC_TREND`]).
///
/// Read by the telemetry tick and published as the `SocRising` mark, so a dark charging
/// flag can be told apart from a counter that is simply not answering.
pub fn soc_rising() -> bool {
    unsafe { SOC_TREND.rising }
}

/// Monotonic time of the last SOC rise, ms (zero - none yet).
pub fn soc_last_rise_ms() -> u64 {
    unsafe { SOC_TREND.last_rise_ms }
}

/// Counts consecutive failed gauge reads and returns the new number.
///
/// It lives here rather than in the driver state: during the telemetry tick `pump`
/// holds a mutable reference to the state, and it cannot be taken a second time.
/// The counter is reset by the first successful read, so a non-zero value in the
/// `SocFail` mark means a run of failures, not their accumulation over all time.
pub unsafe fn note_gauge_failure() -> u32 {
    unsafe {
        GAUGE_FAILS = GAUGE_FAILS.saturating_add(1);
        GAUGE_FAILS
    }
}

/// Source of the published percent (mark `SocSrc`).
pub fn last_soc_source() -> u32 {
    unsafe { SOC_SRC }
}

fn soc_percent(vbat_uv: u32) -> u32 {
    if vbat_uv <= VBAT_EMPTY_UV {
        return 0;
    }
    if vbat_uv >= VBAT_FULL_UV {
        return 100;
    }
    let span = VBAT_FULL_UV - VBAT_EMPTY_UV;
    let rise = vbat_uv - VBAT_EMPTY_UV;
    ((u64::from(rise) * 100) / u64::from(span)) as u32
}

/// Last published relative SoC (0–100), for registry marks.
pub fn last_percent() -> u32 {
    unsafe { LAST_PCT }
}

/// Last power-state flags published to BattC (for registry marks).
pub fn last_power_state() -> u32 {
    unsafe { LAST_POWER_STATE }
}

fn build_status() -> BatteryStatus {
    let vbat = unsafe { LAST_VBAT_UV };
    let pct = unsafe { LAST_PCT };
    // Held flags, not instantaneous samples: see `update_from_telemetry` and
    // `ln8000::battery_policy`. DISCHARGING is computed from the held online -
    // otherwise a single offline tick would show "discharging" on a connected pack.
    let online = unsafe { ONLINE_HOLD }.held;
    // `CHARGING` implies `POWER_ON_LINE`: the holds expire independently
    // (8 s vs 20 s), and a bare `0x4` mask — charging with no AC — is a
    // combination the old `charging = online && ...` form could never produce
    // and one Windows has no sensible reading for.
    let charging = unsafe { CHARGING_HOLD }.held && online;
    let mut power = 0u32;
    if online {
        power |= BATTERY_POWER_ON_LINE;
    }
    if charging {
        power |= BATTERY_CHARGING;
    } else if !online {
        power |= BATTERY_DISCHARGING;
    }
    // Critical only on a known value: an unknown capacity is not an empty one,
    // and flagging it critical puts the pack icon in alarm state on a full
    // battery that simply has no reading yet.
    if pct != BATTERY_UNKNOWN_CAPACITY && pct <= 5 {
        power |= BATTERY_CRITICAL;
    }
    unsafe {
        LAST_POWER_STATE = power;
    }
    BatteryStatus {
        power_state: power,
        capacity: pct,
        voltage: if vbat == 0 { 0 } else { vbat / 1000 },
        // Positive rate = charging mW unknown → report 0 when floating on AC;
        // negative unknown when discharging so UI does not keep a charge glyph.
        rate: if charging {
            BATTERY_UNKNOWN_RATE as i32
        } else if online {
            0
        } else {
            BATTERY_UNKNOWN_RATE as i32
        },
    }
}

unsafe extern "C" fn evt_wdm_irp_preprocess_device_control(
    device: WDFDEVICE,
    irp: PIRP,
) -> NTSTATUS {
    let handle = unsafe { CLASS_HANDLE };
    let mut status = wdk_sys::STATUS_NOT_SUPPORTED;
    if !handle.is_null() {
        status = unsafe { BatteryClassIoctl(handle, irp) };
    }
    if status == wdk_sys::STATUS_NOT_SUPPORTED {
        unsafe { io_skip_current_irp_stack_location(irp) };
        unsafe {
            call_unsafe_wdf_function_binding!(WdfDeviceWdmDispatchPreprocessedIrp, device, irp)
        }
    } else {
        status
    }
}

unsafe extern "C" fn evt_wdm_irp_preprocess_system_control(
    device: WDFDEVICE,
    irp: PIRP,
) -> NTSTATUS {
    let handle = unsafe { CLASS_HANDLE };
    let mut disposition = SysctlIrpDisposition::IrpForward as i32;
    let mut status = wdk_sys::STATUS_NOT_IMPLEMENTED;
    if !handle.is_null() {
        let fdo = unsafe {
            call_unsafe_wdf_function_binding!(WdfDeviceWdmGetDeviceObject, device)
        };
        status = unsafe {
            BatteryClassSystemControl(
                handle,
                core::ptr::addr_of_mut!(WMI_CONTEXT).cast(),
                fdo,
                irp,
                &mut disposition,
            )
        };
    }
    match disposition {
        d if d == SysctlIrpDisposition::IrpProcessed as i32 => status,
        d if d == SysctlIrpDisposition::IrpNotCompleted as i32 => {
            unsafe { IoCompleteRequest(irp, IO_NO_INCREMENT) };
            status
        }
        _ => {
            unsafe { io_skip_current_irp_stack_location(irp) };
            unsafe {
                call_unsafe_wdf_function_binding!(WdfDeviceWdmDispatchPreprocessedIrp, device, irp)
            }
        }
    }
}

/// Inline `IoSkipCurrentIrpStackLocation` (not exported from ntos).
unsafe fn io_skip_current_irp_stack_location(irp: PIRP) {
    let irp = unsafe { &mut *irp };
    irp.CurrentLocation = irp.CurrentLocation.wrapping_add(1);
    let stack = unsafe {
        irp.Tail
            .Overlay
            .__bindgen_anon_2
            .__bindgen_anon_1
            .CurrentStackLocation
    };
    unsafe {
        irp.Tail
            .Overlay
            .__bindgen_anon_2
            .__bindgen_anon_1
            .CurrentStackLocation = stack.add(1);
    }
}

unsafe extern "C" fn query_tag(_context: PVOID, battery_tag: *mut ULONG) -> NTSTATUS {
    let tag = unsafe { BATTERY_TAG };
    unsafe { *battery_tag = tag };
    if tag == BATTERY_TAG_INVALID {
        wdk_sys::STATUS_NO_SUCH_DEVICE
    } else {
        wdk_sys::STATUS_SUCCESS
    }
}

unsafe extern "C" fn query_information(
    _context: PVOID,
    battery_tag: ULONG,
    level: u32,
    _at_rate: i32,
    buffer: PVOID,
    buffer_length: ULONG,
    returned_length: *mut ULONG,
) -> NTSTATUS {
    if battery_tag != unsafe { BATTERY_TAG } {
        return wdk_sys::STATUS_NO_SUCH_DEVICE;
    }
    unsafe { *returned_length = 0 };

    match level {
        0 => {
            // BatteryInformation
            let info = BatteryInformation {
                // Match simbatt: SYSTEM without RELATIVE so WinRT/Settings get
                // non-null mWh-style capacities (values are 0–100 units).
                capabilities: BATTERY_SYSTEM_BATTERY,
                technology: 1,
                reserved: [0; 3],
                chemistry: *b"LION",
                designed_capacity: 100,
                full_charged_capacity: 100,
                default_alert1: 10,
                default_alert2: 5,
                critical_bias: 0,
                cycle_count: 0,
            };
            copy_out(buffer, buffer_length, returned_length, &info)
        }
        3 => {
            // BatteryEstimatedTime — unknown without fuel gauge.
            let t = BATTERY_UNKNOWN_TIME;
            copy_out(buffer, buffer_length, returned_length, &t)
        }
        4 => {
            // BatteryDeviceName
            copy_wstring(buffer, buffer_length, returned_length, "Nabu LN8000")
        }
        6 => {
            // BatteryManufactureName
            copy_wstring(buffer, buffer_length, returned_length, "Xiaomi")
        }
        7 => {
            // BatteryUniqueID
            copy_wstring(buffer, buffer_length, returned_length, "NABU-LN8000-VBAT")
        }
        _ => wdk_sys::STATUS_INVALID_DEVICE_REQUEST,
    }
}

unsafe extern "C" fn set_information(
    _context: PVOID,
    _battery_tag: ULONG,
    _level: u32,
    _buffer: PVOID,
) -> NTSTATUS {
    wdk_sys::STATUS_INVALID_DEVICE_REQUEST
}

unsafe extern "C" fn query_status(
    _context: PVOID,
    battery_tag: ULONG,
    battery_status: *mut BatteryStatus,
) -> NTSTATUS {
    if battery_tag != unsafe { BATTERY_TAG } {
        return wdk_sys::STATUS_NO_SUCH_DEVICE;
    }
    if battery_status.is_null() {
        return wdk_sys::STATUS_INVALID_PARAMETER;
    }
    unsafe { *battery_status = build_status() };
    wdk_sys::STATUS_SUCCESS
}

unsafe extern "C" fn set_status_notify(
    _context: PVOID,
    _battery_tag: ULONG,
    _notify: PVOID,
) -> NTSTATUS {
    wdk_sys::STATUS_SUCCESS
}

unsafe extern "C" fn disable_status_notify(_context: PVOID) -> NTSTATUS {
    wdk_sys::STATUS_SUCCESS
}

unsafe extern "C" fn query_wmi_reg_info(
    _device_object: PDEVICE_OBJECT,
    reg_flags: *mut ULONG,
    _instance_name: *mut UNICODE_STRING,
    registry_path: *mut *mut UNICODE_STRING,
    _mof_resource_name: *mut UNICODE_STRING,
    pdo: *mut PDEVICE_OBJECT,
) -> NTSTATUS {
    unsafe {
        *reg_flags = WMIREG_FLAG_INSTANCE_PDO;
        *registry_path = core::ptr::addr_of_mut!(REGISTRY_PATH);
        *pdo = PDO;
    }
    wdk_sys::STATUS_SUCCESS
}

unsafe extern "C" fn query_wmi_data_block(
    device_object: PDEVICE_OBJECT,
    irp: PIRP,
    guid_index: ULONG,
    _instance_index: ULONG,
    _instance_count: ULONG,
    instance_length_array: *mut ULONG,
    buffer_avail: ULONG,
    buffer: *mut u8,
) -> NTSTATUS {
    if instance_length_array.is_null() {
        return wdk_sys::STATUS_BUFFER_TOO_SMALL;
    }
    let handle = unsafe { CLASS_HANDLE };
    if handle.is_null() {
        return unsafe {
            WmiCompleteRequest(
                device_object,
                irp,
                STATUS_WMI_GUID_NOT_FOUND,
                0,
                IO_NO_INCREMENT,
            )
        };
    }
    let status = unsafe {
        BatteryClassQueryWmiDataBlock(
            handle,
            device_object,
            irp,
            guid_index,
            instance_length_array,
            buffer_avail,
            buffer,
        )
    };
    if status == STATUS_WMI_GUID_NOT_FOUND {
        unsafe {
            WmiCompleteRequest(
                device_object,
                irp,
                STATUS_WMI_GUID_NOT_FOUND,
                0,
                IO_NO_INCREMENT,
            )
        }
    } else {
        status
    }
}

fn copy_out<T: Copy>(
    buffer: PVOID,
    buffer_length: ULONG,
    returned_length: *mut ULONG,
    value: &T,
) -> NTSTATUS {
    let need = core::mem::size_of::<T>() as ULONG;
    unsafe { *returned_length = need };
    if buffer.is_null() || buffer_length < need {
        return wdk_sys::STATUS_BUFFER_TOO_SMALL;
    }
    unsafe {
        ptr::copy_nonoverlapping(
            (value as *const T).cast::<u8>(),
            buffer.cast::<u8>(),
            need as usize,
        );
    }
    wdk_sys::STATUS_SUCCESS
}

fn copy_wstring(
    buffer: PVOID,
    buffer_length: ULONG,
    returned_length: *mut ULONG,
    text: &str,
) -> NTSTATUS {
    // UTF-16LE + NUL
    let chars = text.encode_utf16().count() + 1;
    let need = (chars * 2) as ULONG;
    unsafe { *returned_length = need };
    if buffer.is_null() || buffer_length < need {
        return wdk_sys::STATUS_BUFFER_TOO_SMALL;
    }
    let out = buffer.cast::<u16>();
    for (i, c) in text.encode_utf16().enumerate() {
        unsafe { *out.add(i) = c };
    }
    unsafe { *out.add(chars - 1) = 0 };
    wdk_sys::STATUS_SUCCESS
}
