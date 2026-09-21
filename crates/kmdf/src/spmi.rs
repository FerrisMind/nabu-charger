//! Transport to PMIC registers over the SPMI bus.
//!
//! # What reverse engineering confirmed
//!
//! Reverse engineering of the stock Qualcomm drivers (`qcpmicEIC8150.sys`,
//! `qcspmi8150.sys`) revealed two different mechanisms, and it is important not
//! to confuse them:
//!
//! 1. **SPMI connection** is helper code `0x32C004` (that is
//!    `CTL_CODE(0x32, 1, METHOD_BUFFERED, FILE_READ|FILE_WRITE)`). The stock
//!    client sends an 8-byte request (the low 4 bytes are the signature
//!    `0x42696541`, the high 4 are a constant from `.rdata`) and receives a buffer
//!    that it parses into its context, then installs a table of register access
//!    functions, choosing it by the controller version. The layout of this
//!    request and response is confirmed **partially**.
//!
//! 2. **Register access** is public code `0x41808`:
//!
//!    ```text
//!    0x41808 = CTL_CODE(FILE_DEVICE_CONTROLLER, 0x602, METHOD_BUFFERED, FILE_ANY_ACCESS)
//!            = IOCTL_SPB_EXECUTE_SEQUENCE
//!    ```
//!
//!    That is, registers are read and written with an ordinary SPB transfer list
//!    (`SPB_TRANSFER_LIST`), the same interface the LN8000 driver uses on the I²C
//!    bus. There is no private "register-in-response" protocol, and that is why
//!    looking for such a layout gave no result before.
//!
//! The breakdown with addresses and excerpts is in `docs/SPMI-PATH.md`.
//!
//! # What is missing
//!
//! To send a transfer list, an **SPMI bus I/O target** is required. The stock
//! client obtains it in the first step (`0x32C004`) through the resource hub. Our
//! driver does not perform that step yet: it opens the hub device by name and
//! sends the sequence to it. On hardware this may prove insufficient, in which
//! case the request honestly returns a transport error instead of "quietly doing
//! nothing". The right solution is to obtain the SPMI connection from the `_CRS`
//! of our own node, so the driver needs a resource preparation callback (it does
//! not have one yet).
//!
//! # IRQL level
//!
//! All operations are synchronous, at passive level: they are called from the
//! `EvtIoDeviceControl` of the sequential queue.

use charger_core::{ChargerTransport, RegAddr, TransportError};
use spb::{
    entry_init, SpbTransferList, SpbTransferListEntry, IOCTL_SPB_EXECUTE_SEQUENCE,
    SPB_DIRECTION_FROM_DEVICE, SPB_DIRECTION_TO_DEVICE,
};
use wdk_sys::{
    _POOL_TYPE::NonPagedPool, _WDF_IO_TARGET_OPEN_TYPE::WdfIoTargetOpenByName,
    _WDF_REQUEST_SEND_OPTIONS_FLAGS::WDF_REQUEST_SEND_OPTION_TIMEOUT,
    call_unsafe_wdf_function_binding, NTSTATUS, ULONG, UNICODE_STRING, WDFDEVICE, WDFIOTARGET,
    WDFMEMORY, WDFREQUEST, WDF_IO_TARGET_OPEN_PARAMS, WDF_REQUEST_REUSE_PARAMS,
    WDF_REQUEST_SEND_OPTIONS, WDF_NO_HANDLE, WDF_NO_OBJECT_ATTRIBUTES,
};

/// Helper SPMI connection code (confirmed by reverse engineering; step not run).
#[allow(dead_code)]
pub const IOCTL_RESOURCE_HUB_TRANSACT: u32 = 0x0032_C004;

/// Hub device type (the high 16 bits of code `0x32C004`).
#[allow(dead_code)]
pub const FILE_DEVICE_RESOURCE_HUB: u32 = 0x32;

/// Area for the transfer list, in bytes.
const TRANSFER_AREA: usize = 256;

/// Data buffer (register address and value).
const DATA_LEN: usize = 8;

/// Maximum number of transfers in one sequence.
const MAX_TRANSFERS: usize = 2;

/// Payload offset: right after the list of two transfers.
///
/// `sizeof(SPB_TRANSFER_LIST) + sizeof(SPB_TRANSFER_LIST_ENTRY)` = 48 + 32 = 80.
const PAYLOAD_OFFSET: usize = 80;

/// Timeout of one transaction: 1 s in 100 ns units (the value is negative
/// because the count is relative, as `WDF_REQUEST_SEND_OPTIONS.Timeout` requires).
const SPB_TIMEOUT_100NS: i64 = -10_000_000;

/// Bus access settings.
#[derive(Debug, Clone, Copy)]
pub struct SpmiConfig {
    /// Timeout of one transaction in 100 ns units (a negative value).
    pub timeout_100ns: i64,
    /// Byte order of the register address: `true` means most significant byte first.
    ///
    /// Chosen per the SPMI specification: the command frame transmits the address
    /// starting with the most significant byte. **Not confirmed on hardware**, so it
    /// is a parameter rather than a hard-coded constant: on the first run on the
    /// tablet it can be flipped without touching the logic.
    pub address_big_endian: bool,
}

impl SpmiConfig {
    /// Default configuration for the `nabu` tablet.
    #[must_use]
    pub const fn nabu() -> Self {
        Self {
            timeout_100ns: SPB_TIMEOUT_100NS,
            address_big_endian: true,
        }
    }

    /// Register address bytes in the order set by the configuration.
    #[must_use]
    fn address_bytes(&self, addr: RegAddr) -> [u8; 2] {
        if self.address_big_endian {
            addr.to_be_bytes()
        } else {
            addr.to_le_bytes()
        }
    }
}

/// Bus device name in UTF-16, built at compile time.
const DEVICE_NAME_UTF16: [u16; 20] = utf16_lit("/Device/RESOURCE_HUB");

/// Builds UTF-16 without a trailing zero: `'/'` characters are replaced with `'\\'`.
const fn utf16_lit(ascii: &str) -> [u16; 20] {
    let bytes = ascii.as_bytes();
    let mut out = [0_u16; 20];
    let mut index = 0;
    while index < bytes.len() && index < 20 {
        let byte = bytes[index];
        out[index] = if byte == b'/' { b'\\' as u16 } else { byte as u16 };
        index += 1;
    }
    out
}

fn device_name() -> UNICODE_STRING {
    let length = u16::try_from(DEVICE_NAME_UTF16.len().saturating_mul(2)).unwrap_or(0);
    UNICODE_STRING {
        Length: length,
        MaximumLength: length,
        Buffer: DEVICE_NAME_UTF16.as_ptr().cast_mut(),
    }
}

/// Transport to PMIC registers over the SPMI bus.
///
/// The WDF objects are created once when the device is added and live until its
/// removal, so repeated reads and writes create no new objects.
#[derive(Debug)]
pub struct SpmiTransport {
    target: WDFIOTARGET,
    request: WDFREQUEST,
    input: WDFMEMORY,
    output: WDFMEMORY,
    area: *mut u8,
    data: *mut u8,
    config: SpmiConfig,
}

impl SpmiTransport {
    /// Opens the bus device and prepares the exchange buffers.
    ///
    /// # Errors
    ///
    /// * `Io` - the bus device is unavailable or the WDF objects were not created.
    /// * `Unsupported` - WDF did not support the requested mode.
    ///
    /// # Safety
    ///
    /// Called at passive IRQL (from `EvtDeviceAdd`): creating WDF objects and
    /// opening a target by name at raised IRQL is forbidden.
    pub unsafe fn open(device: WDFDEVICE, config: SpmiConfig) -> Result<Self, TransportError> {
        let mut target: WDFIOTARGET = WDF_NO_HANDLE.cast();
        // SAFETY: `device` is a valid WDFDEVICE; `target` is a local variable.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfIoTargetCreate,
                device,
                WDF_NO_OBJECT_ATTRIBUTES,
                &raw mut target,
            )
        };
        if !nt_ok(status) {
            return Err(TransportError::io("failed to create the I/O target"));
        }

        let mut params: WDF_IO_TARGET_OPEN_PARAMS = unsafe { core::mem::zeroed() };
        params.Size = size_of_ulong::<WDF_IO_TARGET_OPEN_PARAMS>();
        params.Type = WdfIoTargetOpenByName;
        params.TargetDeviceName = device_name();

        // SAFETY: `target` was created above, the parameters are filled in; passive level.
        let status =
            unsafe { call_unsafe_wdf_function_binding!(WdfIoTargetOpen, target, &raw mut params) };
        if !nt_ok(status) {
            return Err(TransportError::io(
                "device \\Device\\RESOURCE_HUB is unavailable",
            ));
        }

        let mut request: WDFREQUEST = WDF_NO_HANDLE.cast();
        let mut input: WDFMEMORY = WDF_NO_HANDLE.cast();
        let mut output: WDFMEMORY = WDF_NO_HANDLE.cast();

        // SAFETY: the handles are local variables for output values.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfRequestCreate,
                WDF_NO_OBJECT_ATTRIBUTES,
                target,
                &raw mut request,
            )
        };
        if !nt_ok(status) {
            return Err(TransportError::unsupported("failed to create WDFREQUEST"));
        }

        let mut area: *mut core::ffi::c_void = core::ptr::null_mut();
        // SAFETY: the memory is allocated from the non-paged pool and lives until the
        // device is removed; the pool alignment is sufficient for `SPB_TRANSFER_LIST`.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfMemoryCreate,
                WDF_NO_OBJECT_ATTRIBUTES,
                NonPagedPool,
                0,
                TRANSFER_AREA,
                &raw mut input,
                &raw mut area,
            )
        };
        if !nt_ok(status) {
            return Err(TransportError::unsupported("failed to allocate the transfer area"));
        }

        let mut data: *mut core::ffi::c_void = core::ptr::null_mut();
        // SAFETY: same as for the transfer area.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfMemoryCreate,
                WDF_NO_OBJECT_ATTRIBUTES,
                NonPagedPool,
                0,
                DATA_LEN,
                &raw mut output,
                &raw mut data,
            )
        };
        if !nt_ok(status) {
            return Err(TransportError::unsupported("failed to allocate the data buffer"));
        }

        Ok(Self {
            target,
            request,
            input,
            output,
            area: area.cast::<u8>(),
            data: data.cast::<u8>(),
            config,
        })
    }

    /// Writes a register byte: one transfer "address (16 bits) + value".
    ///
    /// # Errors
    ///
    /// * `Io` - the bus failed.
    /// * `Timeout` - the bus did not respond within [`SpmiConfig::timeout_100ns`].
    /// * `Protocol` - the bus rejected the sequence.
    pub fn write_reg(&mut self, addr: RegAddr, value: u8) -> Result<(), TransportError> {
        if self.area.is_null() || self.data.is_null() {
            return Err(TransportError::unsupported("SPB buffers were not created"));
        }
        let bytes = self.config.address_bytes(addr);

        // SAFETY: the payload lies inside the allocated area with room to spare; the
        // write happens at passive level, serialization is on the caller's side.
        let (payload, list) = unsafe { self.begin(1)? };
        // SAFETY: we write "address, value" and one transfer of three bytes.
        unsafe {
            core::ptr::write_volatile(payload, bytes[0]);
            core::ptr::write_volatile(payload.add(1), bytes[1]);
            core::ptr::write_volatile(payload.add(2), value);
            *core::ptr::addr_of_mut!((*list).transfers[0]) = entry_init(
                SPB_DIRECTION_TO_DEVICE,
                payload.cast::<core::ffi::c_void>(),
                3,
            );
        }
        self.send()
    }

    /// Reads a register byte: address transfer (16 bits), then a byte read.
    ///
    /// # Errors
    ///
    /// * `Io` - the bus failed.
    /// * `Timeout` - the bus did not respond within [`SpmiConfig::timeout_100ns`].
    /// * `Protocol` - the bus rejected the sequence.
    pub fn read_reg(&mut self, addr: RegAddr) -> Result<u8, TransportError> {
        if self.area.is_null() || self.data.is_null() {
            return Err(TransportError::unsupported("SPB buffers were not created"));
        }
        let bytes = self.config.address_bytes(addr);

        // SAFETY: see `write_reg`; the second transfer refers to the data buffer.
        let (payload, list) = unsafe { self.begin(MAX_TRANSFERS)? };
        // SAFETY: the address is two bytes; the first transfer writes them, the
        // second reads a byte.
        unsafe {
            core::ptr::write_volatile(payload, bytes[0]);
            core::ptr::write_volatile(payload.add(1), bytes[1]);
            *core::ptr::addr_of_mut!((*list).transfers[0]) = entry_init(
                SPB_DIRECTION_TO_DEVICE,
                payload.cast::<core::ffi::c_void>(),
                2,
            );
            let second = self
                .area
                .add(SpbTransferList::header_size())
                .cast::<SpbTransferListEntry>()
                .add(1);
            *second = entry_init(
                SPB_DIRECTION_FROM_DEVICE,
                self.data.cast::<core::ffi::c_void>(),
                1,
            );
        }
        self.send()?;
        // SAFETY: the data buffer was created with size `DATA_LEN`, we read the first byte.
        Ok(unsafe { core::ptr::read_volatile(self.data) })
    }

    /// Prepares the transfer list for `count` transfers and returns payload and list.
    ///
    /// # Errors
    ///
    /// * `Unsupported` - the buffers were not created or the count exceeds `MAX_TRANSFERS`.
    ///
    /// # Safety
    ///
    /// Called at passive level; the buffers belong to the transport.
    unsafe fn begin(&mut self, count: usize) -> Result<(*mut u8, *mut SpbTransferList), TransportError> {
        if count == 0 || count > MAX_TRANSFERS {
            return Err(TransportError::unsupported("invalid transfer count"));
        }
        // SAFETY: the transfer area is allocated with size `TRANSFER_AREA`; the
        // non-paged pool alignment is at least the alignment of `SPB_TRANSFER_LIST`.
        let list = self.area.cast::<SpbTransferList>();
        // SAFETY: the list header is filled in completely.
        unsafe {
            (*list).size = u32::try_from(SpbTransferList::header_size()).unwrap_or(0);
            (*list).reserved = 0;
            (*list).transfer_count = u32::try_from(count)
                .map_err(|_| TransportError::unsupported("too many transfers"))?;
        }
        // SAFETY: the offset lies within `TRANSFER_AREA`.
        let payload = unsafe { self.area.add(PAYLOAD_OFFSET) };
        Ok((payload, list))
    }

    /// Sends the prepared sequence to the bus.
    ///
    /// # Errors
    ///
    /// * `Protocol` - the bus rejected the request format.
    /// * `Timeout` - the bus did not respond.
    /// * `Io` - the bus returned a failure.
    fn send(&mut self) -> Result<(), TransportError> {
        // SAFETY: the request, memory and target are valid; the format is an IOCTL
        // control request, the input is the area with the transfer list, the output
        // is unused.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfIoTargetFormatRequestForIoctl,
                self.target,
                self.request,
                IOCTL_SPB_EXECUTE_SEQUENCE,
                self.input,
                core::ptr::null_mut(),
                self.output,
                core::ptr::null_mut(),
            )
        };
        if !nt_ok(status) {
            return Err(TransportError::protocol("the bus rejected the SPB sequence"));
        }

        let mut options: WDF_REQUEST_SEND_OPTIONS = unsafe { core::mem::zeroed() };
        options.Size = size_of_ulong::<WDF_REQUEST_SEND_OPTIONS>();
        options.Flags = WDF_REQUEST_SEND_OPTION_TIMEOUT as ULONG;
        options.Timeout = self.config.timeout_100ns;

        // SAFETY: the send is synchronous, the level is passive, re-entry is
        // excluded by the device's sequential queue.
        let sent = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfRequestSend,
                self.request,
                self.target,
                &raw mut options,
            )
        };
        if sent == 0 {
            // SAFETY: when the send fails the request must be returned to its initial state.
            unsafe { reuse_request(self.request) };
            return Err(TransportError::timeout("the SPMI bus did not respond"));
        }

        // SAFETY: the request is complete; we read the status and reuse the request.
        let status = unsafe { call_unsafe_wdf_function_binding!(WdfRequestGetStatus, self.request) };
        unsafe { reuse_request(self.request) };
        if !nt_ok(status) {
            return Err(TransportError::io("the SPMI bus returned a failure"));
        }
        Ok(())
    }

    /// Releases the WDF objects. Called when the device is removed.
    ///
    /// # Safety
    ///
    /// The handles must be valid; call at passive level.
    #[allow(dead_code)]
    pub unsafe fn close(self) {
        // SAFETY: the target was created in `open`; we close it properly.
        unsafe {
            let _ = call_unsafe_wdf_function_binding!(WdfIoTargetClose, self.target);
        }
    }
}

impl ChargerTransport for SpmiTransport {
    fn read(&mut self, addr: RegAddr) -> Result<u8, TransportError> {
        self.read_reg(addr)
    }

    fn write(&mut self, addr: RegAddr, value: u8) -> Result<(), TransportError> {
        self.write_reg(addr, value)
    }

    /// No reset is required: the bus connection is permanent, the hub holds the state.
    fn reset(&mut self) -> Result<(), TransportError> {
        Ok(())
    }

    fn name(&self) -> &'static str {
        "spmi"
    }
}

fn nt_ok(status: NTSTATUS) -> bool {
    status >= 0
}

/// Returns a completed request to its initial state for the next transaction.
///
/// # Safety
///
/// `request` must be complete and must not be used concurrently.
unsafe fn reuse_request(request: WDFREQUEST) {
    let mut params = WDF_REQUEST_REUSE_PARAMS {
        Size: size_of_ulong::<WDF_REQUEST_REUSE_PARAMS>(),
        Flags: 0,
        Status: wdk_sys::STATUS_SUCCESS,
        NewIrp: core::ptr::null_mut(),
    };
    params.Size = size_of_ulong::<WDF_REQUEST_REUSE_PARAMS>();
    // SAFETY: the request is complete; the parameters are filled in.
    unsafe {
        let _ = call_unsafe_wdf_function_binding!(WdfRequestReuse, request, &raw mut params);
    }
}

/// Structure size as a `ULONG` for the `Size` field.
fn size_of_ulong<T>() -> ULONG {
    u32::try_from(core::mem::size_of::<T>()).unwrap_or(0)
}
