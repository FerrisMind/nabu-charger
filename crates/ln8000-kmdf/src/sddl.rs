//! Access policy of the device object: the pump is not open to every process.
//!
//! The whole IOCTL surface is `FILE_ANY_ACCESS` (see [`crate::ioctl`]), and most
//! of it is not diagnostics: `SET_MODE`, `SET_CHARGE` and `SET_LIMITS` change
//! what the pump does to the battery, and `WRITE_REG` writes a chip register
//! directly. There is no requestor check anywhere in the driver - no
//! `RequestorMode`, no privilege test - so the security descriptor on the device
//! object is the only gate there is. Without one, the object carries the WDF
//! default for its device type, and any process on the tablet, including a
//! sandboxed application, can open the device and drive the pump.
//!
//! The descriptor below is applied by the framework every time the device is
//! created, which is why it lives in code rather than only in the INF: the
//! hardware key that holds the INF's `Security` value is rewritten when the
//! driver is reinstalled, while this call runs on every start.
//!
//! `D:P(A;;GA;;;SY)(A;;GA;;;BA)` is a protected DACL (no inherited entries)
//! granting `GENERIC_ALL` to `LocalSystem` and to the built-in Administrators
//! group, and nothing to anyone else.
//!
//! Two things this deliberately does not do. It does not tighten the control
//! codes themselves: they stay access-agnostic because the user-mode tool opens
//! the device once and issues both read and write codes through that one handle,
//! and because the access field is part of the code value, which the deployed
//! tooling has baked in. The DACL is the boundary; the access field is not.
//!
//! It also does not cover the device node. The PDO is created by the ACPI bus
//! driver before this driver sees the device, and the device interface resolves
//! to that PDO, so the interface carries the bus driver's descriptor rather than
//! this one. The INF sets the same policy on the node (an INF `Security` value
//! under the hardware key); the two together cover both open paths, the symbolic
//! link `\\.\nabu_ln8000` through this descriptor and the device interface
//! through the INF's.
//!
//! A consequence to know before using the tooling in `deploy/`: a process that is
//! not elevated does not match `(A;;GA;;;BA)` even when its user is an
//! administrator, because a filtered token carries the group for deny-only. The
//! tools that open the device therefore have to run elevated. Reading the
//! telemetry marks does not: those are registry values.

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
/// Must be called before `WdfDeviceCreate`: the descriptor is stored in the
/// initialisation structure and consumed when the device object is built.
///
/// A failure is returned to the caller and is not treated as fatal. The driver
/// has to run on a tablet that is in use, and a device that starts with the
/// default descriptor is a smaller problem than a device that does not start;
/// the return code is published as the `SddlSt` mark so that a failure is still
/// visible from outside.
///
/// # Safety
///
/// `device_init` must be the structure WDF passed to `EvtDriverDeviceAdd` and
/// must still be owned by the driver, that is, the device must not have been
/// created from it yet.
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
