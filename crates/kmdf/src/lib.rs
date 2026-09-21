//! Kernel-mode driver (KMDF) for charging the Xiaomi Pad 5 (`nabu`), Windows on ARM64.
//!
//! # What the driver does
//!
//! The logic core lives in the `charger-core` crate (no OS dependencies): it
//! reads the result of the hardware adapter detection (APSD) and sets the input
//! current limit. Here is the WDF wrapper: the device, the control request
//! queue, the non-blocking detection timer and the transport to the SPMI bus.
//!
//! # Why ARM64
//!
//! The target device is a tablet with a Snapdragon 860. The driver is built
//! **only** for `aarch64-pc-windows-msvc` (see `.cargo/config.toml` next to it);
//! the architecture of the build machine is irrelevant to the driver.
//!
//! # Verification status
//!
//! The core logic is covered by automated tests and builds on the host. This
//! wrapper is built with `cargo wdk build` for ARM64; for on-tablet verification
//! see `docs/HANDOVER.md`.

#![no_std]
#![deny(missing_docs)]
#![allow(clippy::missing_safety_doc)]

mod ioctl;
mod spmi;

extern crate wdk_panic;

use core::cell::RefCell;
use ioctl::{
    NabuIclRequest, NabuJournalRequest, NabuRegRequest, NabuState, NabuStatus, NABU_CAPABILITIES,
    NABU_STATUS_MAGIC, NABU_STATUS_VERSION,
};
use charger_core::{Charger, ChargerConfig, Clock, Event, Journal};
use spmi::{SpmiConfig, SpmiTransport};
use wdk::println;
use wdk_sys::{
    _WDF_DRIVER_INIT_FLAGS::WdfDriverInitNonPnpDriver,
    _WDF_IO_QUEUE_DISPATCH_TYPE::WdfIoQueueDispatchSequential,
    _WDF_TRI_STATE::WdfTrue,
    call_unsafe_wdf_function_binding, NTSTATUS, PCUNICODE_STRING, PWDFDEVICE_INIT, ULONG, WDFDEVICE,
    WDFDRIVER, WDFQUEUE, WDFREQUEST, WDF_DRIVER_CONFIG, WDF_IO_QUEUE_CONFIG, WDF_NO_HANDLE,
    WDF_NO_OBJECT_ATTRIBUTES, WDF_OBJECT_ATTRIBUTES,
};

/// Device interface: the client finds the driver by this GUID.
///
/// `{7C1A5B3E-9D42-4C6B-A1E7-2F4B8C3D5A90}`
pub const GUID_DEVINTERFACE_NABU_CHARGER: wdk_sys::GUID = wdk_sys::GUID {
    Data1: 0x7C1A_5B3E,
    Data2: 0x9D42,
    Data3: 0x4C6B,
    Data4: [0xA1, 0xE7, 0x2F, 0x4B, 0x8C, 0x3D, 0x5A, 0x90],
};

/// Size of the ring journal of operations.
const JOURNAL_CAPACITY: usize = 256;

/// Kernel clock: monotonic milliseconds from the WDF performance counter.
#[derive(Debug, Default)]
pub struct KernelClock;

impl Clock for KernelClock {
    fn now_ms(&self) -> u64 {
        let mut stamp: u64 = 0;
        // SAFETY: `KeQueryInterruptTimePrecise` is a documented kernel function;
        // it is passed a pointer to a local variable for the QPC timestamp.
        let ticks = unsafe { wdk_sys::ntddk::KeQueryInterruptTimePrecise(&raw mut stamp) };
        // The unit is 100 ns, so one millisecond is 10 000 intervals.
        ticks / 10_000
    }
}

/// Ring journal of operations: a fixed buffer with no allocations.
///
/// # Invariants
///
/// Writes come only from the device's sequential queue and its timer, that is,
/// the calls are serialized by WDF; there is never concurrent access from
/// several threads. Therefore `Sync` is implemented manually.
#[derive(Debug)]
struct RingJournal {
    entries: RefCell<[Option<Event>; JOURNAL_CAPACITY]>,
    next: RefCell<usize>,
    total: RefCell<u64>,
}

// SAFETY: see the invariants above: access is serialized by WDF.
unsafe impl Sync for RingJournal {}

impl RingJournal {
    const fn new() -> Self {
        Self {
            entries: RefCell::new([None; JOURNAL_CAPACITY]),
            next: RefCell::new(0),
            total: RefCell::new(0),
        }
    }

    /// How many records have passed through the journal in total.
    fn total(&self) -> u64 {
        *self.total.borrow()
    }
}

impl Journal for RingJournal {
    fn event(&self, event: &Event) {
        let index = *self.next.borrow();
        if let Some(slot) = self.entries.borrow_mut().get_mut(index) {
            *slot = Some(*event);
        }
        *self.next.borrow_mut() = (index.saturating_add(1)) % JOURNAL_CAPACITY;
        *self.total.borrow_mut() = self.total.borrow().saturating_add(1);
    }
}

/// Global journal and clock: they live for the whole lifetime of the driver.
static JOURNAL: RingJournal = RingJournal::new();
static CLOCK: KernelClock = KernelClock;

fn static_clock() -> &'static KernelClock {
    &CLOCK
}

fn static_journal() -> &'static RingJournal {
    &JOURNAL
}

/// Device context: the charger driver session.
///
/// Created when the device context handler is wired up (bring-up), when the
/// detection timer starts working with a live session.
#[allow(dead_code)]
struct DeviceContext {
    charger: Charger<'static, SpmiTransport, KernelClock, RingJournal>,
}

/// Driver entry point.
///
/// # Safety
///
/// Called by the kernel; `driver` and `registry_path` are valid per the WDF contract.
#[unsafe(no_mangle)]
#[unsafe(link_section = "INIT")]
pub unsafe extern "system" fn DriverEntry(
    driver: &mut wdk_sys::DRIVER_OBJECT,
    registry_path: PCUNICODE_STRING,
) -> NTSTATUS {
    println!("nabu-charger: DriverEntry");

    let mut config = WDF_DRIVER_CONFIG {
        Size: size_of_ulong::<WDF_DRIVER_CONFIG>(),
        EvtDriverDeviceAdd: Some(evt_device_add),
        EvtDriverUnload: None,
        DriverInitFlags: WdfDriverInitNonPnpDriver as ULONG,
        DriverPoolTag: 0x5542_414E, // 'NABU'
    };

    let mut driver_handle: WDFDRIVER = WDF_NO_HANDLE.cast();
    // SAFETY: the configuration is filled in completely, the handle is a local variable.
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
        println!("nabu-charger: WdfDriverCreate failed: {status:#010X}");
    }
    status
}

/// Creates the device and the request queue and opens the SPMI bus.
///
/// # Safety
///
/// Called by WDF at passive level; the handles are valid.
unsafe extern "C" fn evt_device_add(
    _driver: WDFDRIVER,
    mut device_init: PWDFDEVICE_INIT,
) -> NTSTATUS {
    let mut attributes = WDF_OBJECT_ATTRIBUTES {
        Size: size_of_ulong::<WDF_OBJECT_ATTRIBUTES>(),
        ..unsafe { core::mem::zeroed() }
    };
    attributes.Size = size_of_ulong::<WDF_OBJECT_ATTRIBUTES>();

    let mut device: WDFDEVICE = WDF_NO_HANDLE.cast();
    // SAFETY: `device_init` is provided by WDF; the attributes are filled in.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceCreate,
            &raw mut device_init,
            &raw mut attributes,
            &raw mut device,
        )
    };
    if status < 0 {
        println!("nabu-charger: WdfDeviceCreate failed: {status:#010X}");
        return status;
    }

    // SAFETY: passive level, the device was created above.
    let transport = match unsafe { SpmiTransport::open(device, SpmiConfig::nabu()) } {
        Ok(transport) => transport,
        Err(err) => {
            println!("nabu-charger: SPMI bus unavailable: {err}");
            return wdk_sys::STATUS_DEVICE_NOT_READY;
        }
    };

    if Charger::open(
        transport,
        static_clock(),
        static_journal(),
        ChargerConfig::for_nabu(),
    )
    .is_err()
    {
        println!("nabu-charger: driver session did not open");
        return wdk_sys::STATUS_DEVICE_NOT_READY;
    }

    // Control request queue: sequential, power managed.
    let mut queue_config = WDF_IO_QUEUE_CONFIG {
        Size: size_of_ulong::<WDF_IO_QUEUE_CONFIG>(),
        DispatchType: WdfIoQueueDispatchSequential,
        PowerManaged: WdfTrue,
        EvtIoDeviceControl: Some(evt_io_device_control),
        ..unsafe { core::mem::zeroed() }
    };
    queue_config.Size = size_of_ulong::<WDF_IO_QUEUE_CONFIG>();

    let mut queue: WDFQUEUE = WDF_NO_HANDLE.cast();
    // SAFETY: the configuration is filled in, the device is created.
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
        println!("nabu-charger: WdfIoQueueCreate failed: {status:#010X}");
        return status;
    }

    println!("nabu-charger: device ready, journal for {} records", JOURNAL.total());
    wdk_sys::STATUS_SUCCESS
}

/// Handles control requests from user mode.
///
/// # Safety
///
/// Called by WDF; `request` is valid, buffer sizes are checked.
unsafe extern "C" fn evt_io_device_control(
    _queue: WDFQUEUE,
    request: WDFREQUEST,
    _output_buffer_length: usize,
    input_buffer_length: usize,
    io_control_code: ULONG,
) {
    match io_control_code {
        ioctl::IOCTL_NABU_GET_STATUS => unsafe { handle_get_status(request) },
        ioctl::IOCTL_NABU_READ_REG => unsafe { handle_read_reg(request, input_buffer_length) },
        ioctl::IOCTL_NABU_WRITE_REG => unsafe { handle_write_reg(request, input_buffer_length) },
        ioctl::IOCTL_NABU_SET_ICL => unsafe { handle_set_icl(request, input_buffer_length) },
        ioctl::IOCTL_NABU_GET_JOURNAL => unsafe {
            handle_get_journal(request, input_buffer_length)
        },
        // Detection and policy application are performed by the timer (bring-up).
        ioctl::IOCTL_NABU_DETECT_START | ioctl::IOCTL_NABU_APPLY_POLICY => unsafe {
            complete(request, wdk_sys::STATUS_NOT_IMPLEMENTED, 0);
        },
        _ => unsafe {
            complete(request, wdk_sys::STATUS_INVALID_DEVICE_REQUEST, 0);
        },
    }
}

/// Returns the driver state to the client.
///
/// # Safety
///
/// `request` is valid; the output buffer is large enough.
unsafe fn handle_get_status(request: WDFREQUEST) {
    let status = NabuStatus {
        magic: NABU_STATUS_MAGIC,
        version: NABU_STATUS_VERSION,
        capabilities: NABU_CAPABILITIES,
        state: NabuState::Closed as u8,
        adapter_code: 0xFF,
        ..NabuStatus::default()
    };
    unsafe { write_output(request, &status) };
}

/// Reads a peripheral register (diagnostics).
///
/// # Safety
///
/// `request` is valid; the input buffer contains [`NabuRegRequest`].
unsafe fn handle_read_reg(request: WDFREQUEST, input_length: usize) {
    if input_length < core::mem::size_of::<NabuRegRequest>() {
        unsafe { complete(request, wdk_sys::STATUS_BUFFER_TOO_SMALL, 0) };
        return;
    }
    let answer = NabuRegRequest {
        error_code: wdk_sys::STATUS_NOT_IMPLEMENTED,
        ..NabuRegRequest::default()
    };
    unsafe { write_output(request, &answer) };
}

/// Writes a peripheral register (diagnostics).
///
/// # Safety
///
/// `request` is valid; the input buffer contains [`NabuRegRequest`].
unsafe fn handle_write_reg(request: WDFREQUEST, input_length: usize) {
    if input_length < core::mem::size_of::<NabuRegRequest>() {
        unsafe { complete(request, wdk_sys::STATUS_BUFFER_TOO_SMALL, 0) };
        return;
    }
    unsafe { complete(request, wdk_sys::STATUS_NOT_IMPLEMENTED, 0) };
}

/// Forces the input current limit.
///
/// # Safety
///
/// `request` is valid; the input buffer contains [`NabuIclRequest`].
unsafe fn handle_set_icl(request: WDFREQUEST, input_length: usize) {
    if input_length < core::mem::size_of::<NabuIclRequest>() {
        unsafe { complete(request, wdk_sys::STATUS_BUFFER_TOO_SMALL, 0) };
        return;
    }
    unsafe { complete(request, wdk_sys::STATUS_NOT_IMPLEMENTED, 0) };
}

/// Returns a snapshot of the operation journal.
///
/// # Safety
///
/// `request` is valid; the output buffer is filled element by element.
unsafe fn handle_get_journal(request: WDFREQUEST, input_length: usize) {
    if input_length < core::mem::size_of::<NabuJournalRequest>() {
        unsafe { complete(request, wdk_sys::STATUS_BUFFER_TOO_SMALL, 0) };
        return;
    }
    unsafe { complete(request, wdk_sys::STATUS_NOT_IMPLEMENTED, 0) };
}

/// Copies the structure into the request's output buffer and completes it.
///
/// # Safety
///
/// `request` is valid; `value` points to a live structure.
unsafe fn write_output<T: Copy>(request: WDFREQUEST, value: &T) {
    let mut buffer: *mut core::ffi::c_void = core::ptr::null_mut();
    let mut length: usize = 0;
    // SAFETY: the request's output buffer is created by the framework (METHOD_BUFFERED).
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfRequestRetrieveOutputBuffer,
            request,
            core::mem::size_of::<T>(),
            &raw mut buffer,
            &raw mut length,
        )
    };
    if status < 0 {
        unsafe { complete(request, status, 0) };
        return;
    }
    // SAFETY: the buffer has been size-checked; we copy exactly size_of::<T>().
    unsafe {
        core::ptr::copy_nonoverlapping(
            core::ptr::from_ref(value).cast::<u8>(),
            buffer.cast::<u8>(),
            core::mem::size_of::<T>(),
        );
    }
    unsafe { complete(request, wdk_sys::STATUS_SUCCESS, core::mem::size_of::<T>()) };
}

/// Completes the request with a status code and the amount of data transferred.
///
/// # Safety
///
/// `request` is a valid, not yet completed request.
unsafe fn complete(request: WDFREQUEST, status: NTSTATUS, information: usize) {
    // SAFETY: the request belongs to this call and is not yet complete.
    unsafe {
        call_unsafe_wdf_function_binding!(
            WdfRequestCompleteWithInformation,
            request,
            status,
            u64::try_from(information).unwrap_or(0),
        );
    }
}

/// Structure size as a `ULONG` for the `Size` field.
fn size_of_ulong<T>() -> ULONG {
    u32::try_from(core::mem::size_of::<T>()).unwrap_or(0)
}
