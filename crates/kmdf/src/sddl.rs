//! Access policy of the device object.
//!
//! Same reasoning as the pump driver's `sddl` module: the control codes are
//! `FILE_ANY_ACCESS` (see [`crate::ioctl`]), the driver makes no requestor check,
//! and the descriptor on the device object is therefore the only gate. The SMB
//! driver's control codes currently answer `STATUS_NOT_IMPLEMENTED`, so nothing
//! is exposed today, but the queue dispatches them and the policy belongs with
//! the device rather than with the handlers.
//!
//! `D:P(A;;GA;;;SY)(A;;GA;;;BA)` is a protected DACL granting `GENERIC_ALL` to
//! `LocalSystem` and to the built-in Administrators group, and nothing to anyone
//! else. It is applied on every device start, which an INF `Security` value is
//! not: reinstalling the driver rewrites the hardware key that holds it. The INF
//! sets the same policy on the device node, whose object is created by the bus
//! driver and so does not carry this one.

use wdk_sys::{call_unsafe_wdf_function_binding, NTSTATUS, PWDFDEVICE_INIT, UNICODE_STRING};

/// Access policy: `LocalSystem` and Administrators, nothing else.
const SDDL_TEXT: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)";

/// Length of the string in UTF-16 code units (the string is ASCII only).
const SDDL_CHARS: usize = SDDL_TEXT.len();

/// The descriptor in UTF-16, without a terminating zero.
const SDDL: [u16; SDDL_CHARS] = ascii_utf16(SDDL_TEXT);

/// Length in bytes, the way `UNICODE_STRING` counts.
const SDDL_BYTES: u16 = (SDDL_CHARS as u16) * 2;

/// ASCII to UTF-16 at compile time.
const fn ascii_utf16(text: &str) -> [u16; SDDL_CHARS] {
    let bytes = text.as_bytes();
    let mut out = [0_u16; SDDL_CHARS];
    let mut index = 0;
    while index < SDDL_CHARS {
        out[index] = bytes[index] as u16;
        index += 1;
    }
    out
}

/// Applies the access policy to the device that is being created.
///
/// Must be called before `WdfDeviceCreate`. A failure is reported and is not
/// fatal: a device that starts with the default descriptor is a smaller problem
/// than a device that does not start.
///
/// # Safety
///
/// `device_init` must be the structure WDF passed to `EvtDriverDeviceAdd` and
/// must still be owned by the driver.
pub unsafe fn assign(device_init: PWDFDEVICE_INIT) -> NTSTATUS {
    let mut sddl = UNICODE_STRING {
        Length: SDDL_BYTES,
        MaximumLength: SDDL_BYTES,
        Buffer: SDDL.as_ptr().cast_mut(),
    };
    // SAFETY: `device_init` is valid until the device is created, and `sddl`
    // points at a static buffer that outlives the call.
    unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceInitAssignSDDLString,
            device_init,
            &raw mut sddl,
        )
    }
}
