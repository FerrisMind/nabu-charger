//! Battery class miniport for tray / Settings SoC.
//!
//! Xiaomi `qcbattminiclass` stays OK but never publishes `GUID_DEVICE_BATTERY`.
//! Attach BattC to this LN8000 FDO (simbatt pattern) and estimate SoC from VBAT.
//!
//! Settings / `Win32_Battery` also need the WMI path that simbatt wires:
//! `IRP_MJ_SYSTEM_CONTROL` → `BatteryClassSystemControl` plus
//! `IoWMIRegistrationControl(REGISTER)`.

use core::ptr;
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

/// Last published relative capacity (0–100).
static mut LAST_PCT: u32 = 100;
/// Last VBAT sample (µV) used for SoC.
static mut LAST_VBAT_UV: u32 = 0;
/// Last VBUS sample (µV) for AC/charge flags.
static mut LAST_VBUS_UV: u32 = 0;
/// Last IIN sample (µA).
static mut LAST_IIN_UA: u32 = 0;
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

const WMIREG_ACTION_REGISTER: u32 = 1;
const WMIREG_ACTION_DEREGISTER: u32 = 2;
const WMIREG_FLAG_INSTANCE_PDO: u32 = 0x0000_0020;
const STATUS_WMI_GUID_NOT_FOUND: NTSTATUS = 0xC000_0295u32 as i32;
const IO_NO_INCREMENT: i8 = 0;

/// Empty / full OCV anchors for nabu Li-ion (µV).
const VBAT_EMPTY_UV: u32 = 3_400_000;
const VBAT_FULL_UV: u32 = 4_350_000;
/// True adapter / USB rail floor (µV).
///
/// Must sit **above** Li-ion OCV. With the cable unplugged, LN8000 Vin often
/// tracks VBAT while in bypass (~4.2–4.4 V) — the old 4.2 V threshold left
/// `BATTERY_POWER_ON_LINE` stuck and the tray kept showing "charging".
const VBUS_ONLINE_UV: u32 = 4_600_000;
/// Vin must exceed VBAT by this much when Vin is below 6 V (µV).
const VBUS_ABOVE_VBAT_UV: u32 = 200_000;
/// Elevated QC/PD rail — always AC even if VBAT is high (µV).
const VBUS_ELEVATED_UV: u32 = 6_000_000;
/// Charging current floor (µA).
const IIN_CHARGING_UA: u32 = 80_000;

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
// Имена повторяют значения WMI-диспозиции из заголовков WDK: префикс `Irp`
// здесь несёт смысл (`SYSCTL_IRP_*`), поэтому общий префикс — не ошибка.
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
/// Ignores `vbat_uv == 0` (ADC not ready) so a bad first sample cannot pin SoC at 0%.
/// Calls [`BatteryClassStatusNotify`] only when `power_state` changes so the
/// tray/Settings see plug/unplug on the same telemetry tick (not after a later poll).
pub unsafe fn update_from_telemetry(vbat_uv: u32, vbus_uv: u32, iin_ua: u32) {
    let prev_power = unsafe { LAST_POWER_STATE };
    if vbat_uv > 0 {
        let pct = soc_percent(vbat_uv);
        unsafe {
            LAST_VBAT_UV = vbat_uv;
            LAST_PCT = pct;
        }
    }
    unsafe {
        LAST_VBUS_UV = vbus_uv;
        LAST_IIN_UA = iin_ua;
        let _ = build_status();
    }
    let new_power = unsafe { LAST_POWER_STATE };
    if new_power == prev_power {
        return;
    }
    let handle = unsafe { CLASS_HANDLE };
    if !handle.is_null() {
        unsafe {
            let _ = BatteryClassStatusNotify(handle);
        }
    }
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

/// AC / USB-adapter present? Vin ADC alone is not enough: unplugged bypass
/// leaves Vin ≈ VBAT above a naive 4.2 V floor.
fn is_ac_online(vbus_uv: u32, vbat_uv: u32) -> bool {
    if vbus_uv >= VBUS_ELEVATED_UV {
        return true;
    }
    if vbus_uv < VBUS_ONLINE_UV {
        return false;
    }
    // 4.6–6.0 V: require Vin clearly above pack voltage so VBAT float ≠ AC.
    if vbat_uv > 0 && vbus_uv < vbat_uv.saturating_add(VBUS_ABOVE_VBAT_UV) {
        return false;
    }
    true
}

fn build_status() -> BatteryStatus {
    let vbat = unsafe { LAST_VBAT_UV };
    let vbus = unsafe { LAST_VBUS_UV };
    let iin = unsafe { LAST_IIN_UA };
    let pct = unsafe { LAST_PCT };
    let online = is_ac_online(vbus, vbat);
    let charging = online && iin >= IIN_CHARGING_UA;
    let mut power = 0u32;
    if online {
        power |= BATTERY_POWER_ON_LINE;
    }
    if charging {
        power |= BATTERY_CHARGING;
    } else if !online {
        power |= BATTERY_DISCHARGING;
    }
    if pct <= 5 {
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
