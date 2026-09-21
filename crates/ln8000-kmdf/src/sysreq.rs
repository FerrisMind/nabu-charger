//! System power request: keep the platform from entering Connected Standby.
//!
//! Live data 2026-09-19: the tablet died four times in one morning
//! (07:58, 08:25, 08:30, 08:42) - `Kernel-Power 41` with `BugcheckCode=0`,
//! `PowerButtonTimestamp=0`, `LongPowerButtonPressDetected=false` and
//! `ConnectedStandbyInProgress=true`, that is neither a crash nor the button:
//! the system enters Modern Standby and never leaves. Each time it happened while
//! idle, 4-12 minutes after boot. The platform reports S0 low-power idle as the
//! only available state (S1/S2/S3 are unavailable), the FADT carries
//! `LOW_POWER_S0_IDLE_CAPABLE`, and the port has no wake sources, so entry into
//! CS is one-way for it.
//!
//! What actually puts the system into CS is **the screen turning off** (on CS
//! machines that is the transition), so leaving the screen on forever is not an
//! option: it costs extra power and heat. The `PowerRequestSystemRequired`
//! request tells the power manager that the device needs a working S0 state, and
//! the system no longer enters CS, although the screen still goes dark on its own
//! timeout.
//!
//! The request is held for the whole lifetime of the device (created in
//! `EvtDevicePrepareHardware`, released in `EvtDeviceReleaseHardware`) and is
//! visible from outside as the `SYSTEM` line in `powercfg /requests`, which is
//! the live check.

use core::ptr;
use wdk_sys::{NTSTATUS, PVOID, ULONG, USHORT, WDFDEVICE};

/// `POWER_REQUEST_TYPE::PowerRequestSystemRequired`.
const POWER_REQUEST_SYSTEM_REQUIRED: i32 = 1;
/// `POWER_REQUEST_CONTEXT_VERSION`.
const POWER_REQUEST_CONTEXT_VERSION: ULONG = 0;
/// `POWER_REQUEST_CONTEXT_SIMPLE_STRING`: the reason string sits right in the context.
const POWER_REQUEST_CONTEXT_SIMPLE_STRING: ULONG = 1;

/// Reason text (visible in `powercfg /requests`).
const REASON_TEXT: &str = "nabu ln8000 charge pump";
/// Reason string length in UTF-16 code units.
const REASON_LEN: usize = REASON_TEXT.len();

/// ASCII to UTF-16 at compile time (the string is ASCII only).
const fn widen(text: &str) -> [u16; REASON_LEN] {
    let bytes = text.as_bytes();
    let mut out = [0u16; REASON_LEN];
    let mut i = 0;
    while i < REASON_LEN {
        out[i] = bytes[i] as u16;
        i += 1;
    }
    out
}

/// Reason string in UTF-16; the `UNICODE_STRING` inside the context points at it.
static REASON: [u16; REASON_LEN] = widen(REASON_TEXT);

/// `UNICODE_STRING` (x64: 2 + 2 + padding + pointer = 16 bytes).
#[repr(C)]
struct UnicodeString {
    length: USHORT,
    maximum_length: USHORT,
    buffer: *mut u16,
}

/// `COUNTED_REASON_CONTEXT` with the `SimpleString` variant.
#[repr(C)]
struct CountedReasonContext {
    version: ULONG,
    flags: ULONG,
    simple_string: UnicodeString,
}

unsafe extern "C" {
    /// Creates a power request object (`ntoskrnl`), PASSIVE_LEVEL.
    fn PoCreatePowerRequest(
        power_request: *mut PVOID,
        device_object: wdk_sys::PDEVICE_OBJECT,
        context: *mut CountedReasonContext,
    ) -> NTSTATUS;
    /// Sets the power request; IRQL <= DISPATCH_LEVEL.
    fn PoSetPowerRequest(power_request: PVOID, request_type: i32) -> NTSTATUS;
    /// Clears the power request.
    fn PoClearPowerRequest(power_request: PVOID, request_type: i32) -> NTSTATUS;
    /// Deletes the power request object.
    fn PoDeletePowerRequest(power_request: PVOID);
}

/// Active request (null means not created).
///
/// Lives until [`release`]; there is a single device, hence a single request.
static mut REQUEST: PVOID = ptr::null_mut();

/// Takes `PowerRequestSystemRequired` for the device `device`.
///
/// Returns `STATUS_SUCCESS` on a repeated call too, if the request is already
/// taken: hardware preparation may run several times per boot.
/// Failures are not fatal - the pump driver must work without this protection as
/// well, but the return code goes to the `SysReqSt` mark.
pub unsafe fn acquire(device: WDFDEVICE) -> NTSTATUS {
    if !unsafe { REQUEST }.is_null() {
        return wdk_sys::STATUS_SUCCESS;
    }
    let fdo: wdk_sys::PDEVICE_OBJECT =
        unsafe { wdk_sys::call_unsafe_wdf_function_binding!(WdfDeviceWdmGetDeviceObject, device) };
    if fdo.is_null() {
        return wdk_sys::STATUS_INVALID_PARAMETER;
    }
    // SAFETY: `REASON` is a static buffer, it outlives the context; the string
    // is ASCII only, so bytes equal code units.
    let mut context = CountedReasonContext {
        version: POWER_REQUEST_CONTEXT_VERSION,
        flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
        simple_string: UnicodeString {
            length: USHORT::try_from(REASON_LEN * 2).unwrap_or(u16::MAX),
            maximum_length: USHORT::try_from(REASON_LEN * 2).unwrap_or(u16::MAX),
            buffer: REASON.as_ptr().cast_mut(),
        },
    };
    let mut request: PVOID = ptr::null_mut();
    // SAFETY: the context and the target are locals, `fdo` is checked for null.
    let created = unsafe { PoCreatePowerRequest(&mut request, fdo, &mut context) };
    if created < 0 {
        return created;
    }
    // SAFETY: the object is created; the request type is a documented constant.
    let set = unsafe { PoSetPowerRequest(request, POWER_REQUEST_SYSTEM_REQUIRED) };
    if set < 0 {
        unsafe { PoDeletePowerRequest(request) };
        return set;
    }
    unsafe {
        REQUEST = request;
    }
    wdk_sys::STATUS_SUCCESS
}

/// Clears and deletes the request (called from `EvtDeviceReleaseHardware`).
pub unsafe fn release() {
    let request = unsafe { REQUEST };
    if request.is_null() {
        return;
    }
    // SAFETY: the object is ours and not deleted yet.
    unsafe {
        let _ = PoClearPowerRequest(request, POWER_REQUEST_SYSTEM_REQUIRED);
        PoDeletePowerRequest(request);
        REQUEST = ptr::null_mut();
    }
}

