//! LN8000 transport: the PEIC I2C node through Resource Hub and SPB.
//!
//! # How this works in Windows
//!
//! A peripheral driver for an I2C device receives an `I2cSerialBusV2` resource with
//! a connection identifier (`ConnectionId`) in `_CRS`. The path to the bus is built
//! by the documented Resource Hub rule: the prefix `\Device\RESOURCE_HUB\` plus
//! **16 hexadecimal digits** of the identifier
//! (`RESOURCE_HUB_ID_TO_FILE_NAME`, format `%0*I64x`, `reshub.h`).
//!
//! The exchange goes through `IOCTL_SPB_EXECUTE_SEQUENCE` with a transfer list
//! ([`crate::spb_abi`]): to read a register, first write its address, then read the
//! byte; to write, a single "address + value" transfer.
//!
//! Sources: `08-driver-samples/Windows-driver-samples/spb/SpbTestTool/sys/`
//! (`peripheral.cpp`, `device.cpp`), the WDK headers `spb.h` and `reshub.h`.

use crate::spb_abi::{
    ATTACH_MAGIC, ATTACH_REPLY_LEN, IOCTL_ATTACH, IOCTL_SPB_EXECUTE_SEQUENCE, IOCTL_SPB_LOCK_CONNECTION,
    IOCTL_SPMI_SUPERUSER_GRANT, IOCTL_SPMI_SUPERUSER_READ, IOCTL_SPMI_SUPERUSER_WRITE,
    SPMI_SUPERUSER_HEADER_LEN, SPB_DIRECTION_FROM_DEVICE, SPB_DIRECTION_NONE,
    SPB_DIRECTION_TO_DEVICE, SPB_FORMAT_SIMPLE, SpbTransferList, SpbTransferListEntry, entry_init,
};
use ln8000::{BusError, RegAddr, RegisterBus};
use wdk_sys::{
    call_unsafe_wdf_function_binding, NTSTATUS, ULONG, UNICODE_STRING,
    WDFDEVICE, WDFIOTARGET, WDFMEMORY, WDFREQUEST, WDF_IO_TARGET_OPEN_PARAMS,
    WDF_REQUEST_SEND_OPTIONS, WDF_NO_HANDLE, WDF_NO_OBJECT_ATTRIBUTES,
    _WDF_IO_TARGET_OPEN_TYPE::WdfIoTargetOpenByName,
    _WDF_REQUEST_SEND_OPTIONS_FLAGS::{WDF_REQUEST_SEND_OPTION_SYNCHRONOUS, WDF_REQUEST_SEND_OPTION_TIMEOUT},
};

/// Resource Hub path prefix (`RESOURCE_HUB_DEVICE_NAME_PREFIX`).
pub const RESOURCE_HUB_PREFIX: &str = "/Device/RESOURCE_HUB/";

/// Maximum number of transfers in a sequence.
pub const MAX_TRANSFERS: usize = 3;

/// Transaction timeout in 100 ns units (one second).
const SPB_TIMEOUT_100NS: i64 = -10_000 * 1_000;

/// Access mask when opening the Resource Hub node: same as the stock driver
/// (`FILE_GENERIC_READ|FILE_GENERIC_WRITE|SYNCHRONIZE` = 0x1F01FF).
const HUB_DESIRED_ACCESS: u32 = 0x001F_01FF;

/// `SimpleNonPaged` buffer format: allows a buffer outside the request buffers.
const SPB_FORMAT_SIMPLE_NON_PAGED: u32 = 3;

/// Variant 1: data in the request data buffer, `Simple` format.
const VARIANT_OUTPUT_SIMPLE: u8 = 0;
/// Variant 2: same data, but `SimpleNonPaged` format.
const VARIANT_OUTPUT_NON_PAGED: u8 = 1;
/// Variant 5: the list terminates with an entry whose direction is `None`.
const VARIANT_TERMINATED: u8 = 3;
/// Variant 6: data inside the output buffer that was passed into the request.
const VARIANT_OUTPUT_MEMORY: u8 = 4;
/// Variant 7: same, but `SimpleNonPaged` format.
const VARIANT_OUTPUT_MEMORY_NON_PAGED: u8 = 5;

/// Transfer area size with room for entries and data.
/// Transfer area size: exactly for the maximum number of transfers.
const TRANSFER_AREA: usize = SpbTransferList::area_size(MAX_TRANSFERS);

/// LN8000 transport over SPB.
///
/// The WDF objects are created once in [`SpbBus::open`] and live until the device
/// is removed: repeated reads and writes do not create new objects.
#[derive(Debug)]
pub struct SpbBus {
    target: WDFIOTARGET,
    request: WDFREQUEST,
    /// Owner of the transfer area: the buffer itself is not read, it just holds
    /// the allocation.
    #[allow(dead_code)]
    input: WDFMEMORY,
    /// Data buffer: not passed into the request (the reference sample does that),
    /// but needed as the area for the exchange bytes.
    #[allow(dead_code)]
    output: WDFMEMORY,
    area: *mut u8,
    data: *mut u8,
    peripheral_id: u64,
    name: &'static str,
    /// Status of the last exchange: needed when analysing failures on hardware
    /// where the driver's debug output is unavailable.
    last_status: i32,
    /// The send did not complete and WDF still owns the cached request. Resending
    /// such a request is a fatal WDF error, so until the device is restarted the
    /// bus answers with a failure instead of exchanging data.
    request_lost: bool,
    /// How to format the request: see `VARIANT_*`. Switched by trial.
    variant: u8,
    /// View of the transfer buffer for one transfer: exact length 48 bytes.
    input_one: WDFMEMORY,
    /// View of the transfer buffer for two transfers: exact length 80 bytes.
    input_two: WDFMEMORY,
    /// View of the transfer buffer for three transfers (with the terminating entry).
    input_three: WDFMEMORY,
    /// Connection request input buffer (8 bytes).
    attach_in: WDFMEMORY,
    /// Connection request reply buffer (1024 bytes).
    attach_out: WDFMEMORY,
    /// Pointer to the connection input.
    attach_in_ptr: *mut u8,
    /// Pointer to the connection reply.
    attach_out_ptr: *mut u8,
    /// First words of the node reply: evidence for the registry.
    attach_words: [u32; 4],
    /// How many transfers the last built list declared.
    last_count: u32,
}

impl SpbBus {
    /// Opens the bus for the peripheral with the given connection identifier.
    ///
    /// # Errors
    ///
    /// * `Io` - the target was not created or the Resource Hub node is unavailable.
    /// * `Unsupported` - WDF failed to create the request or the buffers.
    ///
    /// # Safety
    ///
    /// Called at passive IRQL (from `EvtDevicePrepareHardware`).
    pub unsafe fn open(device: WDFDEVICE, peripheral_id: u64, use_hub: bool) -> Result<Self, BusError> {
        // The target is either the device parent (the bus controller stack) or the
        // Resource Hub node for the connection identifier.
        //
        // Verified on the tablet: a sequence to the parent goes through, but the
        // device address is not there; to the node the address is there, but the
        // request is rejected. The only difference between our node open and the
        // stock driver is the access mask, so it is kept separately.
        let target: WDFIOTARGET = if use_hub {
            let mut hub: WDFIOTARGET = WDF_NO_HANDLE.cast();
            // SAFETY: the device is created; the handle is a local variable.
            let status = unsafe {
                call_unsafe_wdf_function_binding!(
                    WdfIoTargetCreate,
                    device,
                    WDF_NO_OBJECT_ATTRIBUTES,
                    &raw mut hub,
                )
            };
            if !nt_ok(status) {
                return Err(BusError::io("failed to create the I/O target"));
            }
            let path = resource_hub_path(peripheral_id);
            let mut params: WDF_IO_TARGET_OPEN_PARAMS = unsafe { core::mem::zeroed() };
            params.Size = size_of_ulong::<WDF_IO_TARGET_OPEN_PARAMS>();
            params.Type = WdfIoTargetOpenByName;
            params.TargetDeviceName = path.as_unicode_string();
            // The mask is the same as the stock node driver uses: with it the node
            // returns an object that accepts sequences.
            params.DesiredAccess = HUB_DESIRED_ACCESS;
            params.ShareAccess = 0;
            params.CreateDisposition = wdk_sys::FILE_OPEN;
            params.FileAttributes = wdk_sys::FILE_ATTRIBUTE_NORMAL;
            // SAFETY: the target is created, the parameters are filled in, passive level.
            let status = unsafe {
                call_unsafe_wdf_function_binding!(WdfIoTargetOpen, hub, &raw mut params)
            };
            if !nt_ok(status) {
                return Err(BusError::io("Resource Hub node unavailable"));
            }
            hub
        } else {
            // SAFETY: the device is created; the target is owned by WDF.
            let parent: WDFIOTARGET =
                unsafe { call_unsafe_wdf_function_binding!(WdfDeviceGetIoTarget, device) };
            if parent.is_null() {
                return Err(BusError::io("no device parent target"));
            }
            parent
        };

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
            return Err(BusError::unsupported("failed to create WDFREQUEST"));
        }

        let mut area: *mut core::ffi::c_void = core::ptr::null_mut();
        // SAFETY: the memory is allocated from the non-paged pool for the transfer list.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfMemoryCreate,
                WDF_NO_OBJECT_ATTRIBUTES,
                wdk_sys::_POOL_TYPE::NonPagedPool,
                0,
                TRANSFER_AREA,
                &raw mut input,
                &raw mut area,
            )
        };
        if !nt_ok(status) {
            return Err(BusError::unsupported("failed to allocate the transfer buffer"));
        }

        // Views of the same buffer with an exact length: the node checks the length
        // of the transfer list, so it rejects an oversized one.
        let one_len = SpbTransferList::area_size(1);
        let two_len = SpbTransferList::area_size(2);
        let mut input_one: WDFMEMORY = WDF_NO_HANDLE.cast();
        // SAFETY: the buffer was allocated above and lives until the device is removed.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfMemoryCreatePreallocated,
                WDF_NO_OBJECT_ATTRIBUTES,
                area,
                one_len,
                &raw mut input_one,
            )
        };
        if !nt_ok(status) {
            return Err(BusError::unsupported("failed to create the one-transfer buffer view"));
        }
        let mut input_two: WDFMEMORY = WDF_NO_HANDLE.cast();
        // SAFETY: same as above, length for two transfers.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfMemoryCreatePreallocated,
                WDF_NO_OBJECT_ATTRIBUTES,
                area,
                two_len,
                &raw mut input_two,
            )
        };
        if !nt_ok(status) {
            return Err(BusError::unsupported("failed to create the two-transfer buffer view"));
        }
        let three_len = SpbTransferList::area_size(3);
        let mut input_three: WDFMEMORY = WDF_NO_HANDLE.cast();
        // SAFETY: same as above, length for three transfers.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfMemoryCreatePreallocated,
                WDF_NO_OBJECT_ATTRIBUTES,
                area,
                three_len,
                &raw mut input_three,
            )
        };
        if !nt_ok(status) {
            return Err(BusError::unsupported("failed to create the three-transfer buffer view"));
        }

        let mut attach_in: *mut core::ffi::c_void = core::ptr::null_mut();
        let mut attach_in_mem: WDFMEMORY = WDF_NO_HANDLE.cast();
        // SAFETY: the connection input is eight bytes in the non-paged pool.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfMemoryCreate,
                WDF_NO_OBJECT_ATTRIBUTES,
                wdk_sys::_POOL_TYPE::NonPagedPool,
                0,
                8,
                &raw mut attach_in_mem,
                &raw mut attach_in,
            )
        };
        if !nt_ok(status) {
            return Err(BusError::unsupported("failed to allocate the connection input"));
        }
        let mut attach_out: *mut core::ffi::c_void = core::ptr::null_mut();
        let mut attach_out_mem: WDFMEMORY = WDF_NO_HANDLE.cast();
        // SAFETY: the connection reply is 1024 bytes in the non-paged pool.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfMemoryCreate,
                WDF_NO_OBJECT_ATTRIBUTES,
                wdk_sys::_POOL_TYPE::NonPagedPool,
                0,
                ATTACH_REPLY_LEN,
                &raw mut attach_out_mem,
                &raw mut attach_out,
            )
        };
        if !nt_ok(status) {
            return Err(BusError::unsupported("failed to allocate the connection reply"));
        }

        let mut data: *mut core::ffi::c_void = core::ptr::null_mut();
        // SAFETY: same as the transfer buffer.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfMemoryCreate,
                WDF_NO_OBJECT_ATTRIBUTES,
                wdk_sys::_POOL_TYPE::NonPagedPool,
                0,
                8,
                &raw mut output,
                &raw mut data,
            )
        };
        if !nt_ok(status) {
            return Err(BusError::unsupported("failed to allocate the data buffer"));
        }

        Ok(Self {
            target,
            request,
            input,
            output,
            area: area.cast::<u8>(),
            data: data.cast::<u8>(),
            peripheral_id,
            name: "spb-i2c",
            last_status: 0,
            request_lost: false,
            variant: VARIANT_OUTPUT_SIMPLE,
            input_one,
            input_two,
            input_three,
            attach_in: attach_in_mem,
            attach_out: attach_out_mem,
            attach_in_ptr: attach_in.cast::<u8>(),
            attach_out_ptr: attach_out.cast::<u8>(),
            attach_words: [0; 4],
            last_count: 0,
        })
    }

    /// Switches the request format for the bus node.
    pub fn set_variant(&mut self, variant: u8) {
        self.variant = variant;
    }

    /// Raw status of the last bus exchange (`NTSTATUS`).
    ///
    /// Zero is success; a negative value is the failure code from WDF or the bus node.
    #[must_use]
    pub fn last_status(&self) -> i32 {
        self.last_status
    }

    /// Connection identifier obtained from `_CRS`.
    ///
    /// Kept for the journal and diagnostics during bring-up.
    #[allow(dead_code)]
    #[must_use]
    pub const fn peripheral_id(&self) -> u64 {
        self.peripheral_id
    }

    /// Human-readable bus path (for the journal).
    #[must_use]
    pub fn path_string(&self) -> [u8; HUB_PATH_CHARS] {
        resource_hub_path(self.peripheral_id).as_ascii()
    }

    /// Checks that the cached request can be sent again.
    ///
    /// # Errors
    ///
    /// `Timeout` - the request is lost (see [`Self::send_failed`]).
    fn cached_request(&self) -> Result<(), BusError> {
        if self.request_lost {
            return Err(BusError::timeout(
                "cached request lost: a device restart is required",
            ));
        }
        Ok(())
    }

    /// Marks the cached request as lost after a failed send.
    ///
    /// `WdfRequestSend` returned `false` with the `TIMEOUT` option set, which means
    /// the request stayed with the I/O target: it will complete later or be cancelled.
    /// Calling `WdfRequestReuse` on it and then resending is not allowed - WDF answers
    /// with `WDF_VIOLATION (0x10D)` and `Arg2 = 3` ("request already sent to the I/O
    /// target"). That is exactly how the kernel crashed three times on 18.09. WDF
    /// itself releases the request when the device is removed, so from here on the
    /// driver only fails until the restart.
    fn send_failed(&mut self, reason: &'static str) -> BusError {
        self.request_lost = true;
        self.last_status = -1;
        BusError::timeout(reason)
    }

    /// Performs one transaction: a register read or write.
    ///
    /// # Errors
    ///
    /// * `Protocol` - the transfer list could not be prepared.
    /// * `Timeout` - the bus did not answer within one second.
    /// * `Io` - the bus returned a failure.
    pub fn transact(&mut self, addr: RegAddr, value: Option<u8>) -> Result<u8, BusError> {
        self.cached_request()?;
        // SAFETY: the transfer area is allocated with size TRANSFER_AREA and lives
        // until the device is removed; the pointers inside the list refer to it.
        unsafe { self.prepare(addr, value)? };

        // SAFETY: the request, memory and target are valid; the format is an IOCTL.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfIoTargetFormatRequestForIoctl,
                self.target,
                self.request,
                IOCTL_SPB_EXECUTE_SEQUENCE,
                match self.last_count {
                    1 => self.input_one,
                    2 => self.input_two,
                    _ => self.input_three,
                },
                core::ptr::null_mut(),
                // The output buffer is passed only in the variants that check for
                // it: the data lives inside it.
                if self.variant >= VARIANT_OUTPUT_MEMORY {
                    self.output
                } else {
                    core::ptr::null_mut()
                },
                core::ptr::null_mut(),
            )
        };
        if !nt_ok(status) {
            return Err(BusError::protocol("SPB rejected the sequence"));
        }

        let mut options: WDF_REQUEST_SEND_OPTIONS = unsafe { core::mem::zeroed() };
        options.Size = size_of_ulong::<WDF_REQUEST_SEND_OPTIONS>();
        options.Flags = (WDF_REQUEST_SEND_OPTION_SYNCHRONOUS | WDF_REQUEST_SEND_OPTION_TIMEOUT) as ULONG;
        options.Timeout = SPB_TIMEOUT_100NS;

        // SAFETY: the send is synchronous, passive level; the bus is protected
        // against re-entry by the state mutex in `lib.rs`, because the WDF queue
        // serializes only IOCTLs while the telemetry timer runs in its own context.
        let sent = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfRequestSend,
                self.request,
                self.target,
                &raw mut options,
            )
        };
        if sent == 0 {
            return Err(self.send_failed("I²C bus did not answer"));
        }

        // SAFETY: the request is complete; we read the status and the value.
        let status = unsafe { call_unsafe_wdf_function_binding!(WdfRequestGetStatus, self.request) };
        self.last_status = status;
        // SAFETY: the data buffer is at least two bytes; the reply byte is second,
        // the register address was written first.
        let byte = unsafe { core::ptr::read_volatile(self.data.add(1)) };
        unsafe { reuse_request(self.request) };
        if !nt_ok(status) {
            return Err(BusError::io("SPB returned a failure for the transaction"));
        }
        Ok(if value.is_none() { byte } else { 0 })
    }

    /// SPMI transaction with a 16-bit register address (USBIN / PM8150B).
    ///
    /// Unlike I²C on the LN8000 (1 address byte), SPMI sends two address bytes,
    /// then the value. The byte order is set by `big_endian` (BE by default, as in
    /// the SPMI specification; on hardware LE may be required).
    ///
    /// # Errors
    ///
    /// The same as [`Self::transact`].
    pub fn transact_spmi16(
        &mut self,
        addr: u16,
        value: Option<u8>,
        big_endian: bool,
    ) -> Result<u8, BusError> {
        self.cached_request()?;
        // SAFETY: the buffers are created in `open`; passive level.
        unsafe { self.prepare_spmi16(addr, value, big_endian)? };
        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfIoTargetFormatRequestForIoctl,
                self.target,
                self.request,
                IOCTL_SPB_EXECUTE_SEQUENCE,
                match self.last_count {
                    1 => self.input_one,
                    2 => self.input_two,
                    _ => self.input_three,
                },
                core::ptr::null_mut(),
                if self.variant >= VARIANT_OUTPUT_MEMORY {
                    self.output
                } else {
                    core::ptr::null_mut()
                },
                core::ptr::null_mut(),
            )
        };
        if !nt_ok(status) {
            return Err(BusError::protocol("SPB rejected the SPMI sequence"));
        }

        let mut options: WDF_REQUEST_SEND_OPTIONS = unsafe { core::mem::zeroed() };
        options.Size = size_of_ulong::<WDF_REQUEST_SEND_OPTIONS>();
        options.Flags =
            (WDF_REQUEST_SEND_OPTION_SYNCHRONOUS | WDF_REQUEST_SEND_OPTION_TIMEOUT) as ULONG;
        options.Timeout = SPB_TIMEOUT_100NS;

        let sent = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfRequestSend,
                self.request,
                self.target,
                &raw mut options,
            )
        };
        if sent == 0 {
            return Err(self.send_failed("SPMI bus did not answer"));
        }

        let status = unsafe { call_unsafe_wdf_function_binding!(WdfRequestGetStatus, self.request) };
        self.last_status = status;
        // The address is two bytes; the reply is the third byte of the data buffer.
        let byte = unsafe { core::ptr::read_volatile(self.data.add(2)) };
        unsafe { reuse_request(self.request) };
        if !nt_ok(status) {
            return Err(BusError::io("SPB returned a failure for the SPMI transaction"));
        }
        Ok(if value.is_none() { byte } else { 0 })
    }

    /// Tries to perform the peripheral connection.
    ///
    /// The reference driver does this step before any register access: it sends
    /// eight bytes as input (the magic `0x42696541` plus four bytes) and receives
    /// a 1024 byte reply. Returns the request status; the first reply words are kept.
    pub fn attach(&mut self) -> i32 {
        if self.request_lost {
            return -1;
        }
        if self.attach_in_ptr.is_null() || self.attach_out_ptr.is_null() {
            return -1;
        }
        // SAFETY: the buffers are created in `open`; the input is exactly eight bytes.
        unsafe {
            core::ptr::write_volatile(self.attach_in_ptr.cast::<u32>(), ATTACH_MAGIC);
            core::ptr::write_volatile(self.attach_in_ptr.add(4).cast::<u32>(), 1);
        }
        // SAFETY: the target and request are valid; the buffers are created in `open`.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfIoTargetFormatRequestForIoctl,
                self.target,
                self.request,
                IOCTL_ATTACH,
                self.attach_in,
                core::ptr::null_mut(),
                self.attach_out,
                core::ptr::null_mut(),
            )
        };
        if !nt_ok(status) {
            self.last_status = status;
            return status;
        }
        let mut options: WDF_REQUEST_SEND_OPTIONS = unsafe { core::mem::zeroed() };
        options.Size = size_of_ulong::<WDF_REQUEST_SEND_OPTIONS>();
        options.Flags = (WDF_REQUEST_SEND_OPTION_SYNCHRONOUS | WDF_REQUEST_SEND_OPTION_TIMEOUT) as ULONG;
        options.Timeout = SPB_TIMEOUT_100NS;
        // SAFETY: synchronous send at passive level.
        let sent = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfRequestSend,
                self.request,
                self.target,
                &raw mut options,
            )
        };
        if sent == 0 {
            let _ = self.send_failed("the I/O target did not complete the request");
            return -1;
        }
        // SAFETY: the request is complete; we read the status and the first reply words.
        let status = unsafe { call_unsafe_wdf_function_binding!(WdfRequestGetStatus, self.request) };
        self.last_status = status;
        if nt_ok(status) {
            for index in 0..4 {
                // SAFETY: the reply is 1024 bytes, four words are within it.
                self.attach_words[index] = unsafe {
                    core::ptr::read_volatile(self.attach_out_ptr.add(index * 4).cast::<u32>())
                };
            }
        }
        unsafe { reuse_request(self.request) };
        status
    }

    /// One word from the node reply to the connection request.
    #[must_use]
    pub fn attach_word(&self, index: usize) -> u32 {
        self.attach_words.get(index).copied().unwrap_or(0)
    }

    /// Builds a transfer list entry with the given buffer format.
    fn entry_with_format(
        format: u32,
        direction: u32,
        buffer: *mut core::ffi::c_void,
        buffer_cb: u32,
    ) -> SpbTransferListEntry {
        let mut entry = entry_init(direction, buffer, buffer_cb);
        entry.buffer.format = format;
        entry
    }

    /// Sends an SPB control request without buffers and returns the status.
    ///
    /// Needed to find out which requests the node supports at all: one request
    /// code per call. The caller writes the status into the registry.
    pub fn probe_ioctl(&mut self, code: u32) -> i32 {
        if self.request_lost {
            return -1;
        }
        // SAFETY: the target and request are valid; the request has no buffers.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfIoTargetFormatRequestForIoctl,
                self.target,
                self.request,
                code,
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                core::ptr::null_mut(),
            )
        };
        if !nt_ok(status) {
            self.last_status = status;
            return status;
        }
        let mut options: WDF_REQUEST_SEND_OPTIONS = unsafe { core::mem::zeroed() };
        options.Size = size_of_ulong::<WDF_REQUEST_SEND_OPTIONS>();
        options.Flags =
            (WDF_REQUEST_SEND_OPTION_SYNCHRONOUS | WDF_REQUEST_SEND_OPTION_TIMEOUT) as ULONG;
        options.Timeout = SPB_TIMEOUT_100NS;
        // SAFETY: synchronous send at passive level.
        let sent = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfRequestSend,
                self.request,
                self.target,
                &raw mut options,
            )
        };
        if sent == 0 {
            let _ = self.send_failed("the I/O target did not complete the request");
            return -1;
        }
        // SAFETY: the request is complete, we read its status.
        let status = unsafe { call_unsafe_wdf_function_binding!(WdfRequestGetStatus, self.request) };
        self.last_status = status;
        unsafe { reuse_request(self.request) };
        status
    }

    /// Tries to lock the connection (`IOCTL_SPB_LOCK_CONNECTION`).
    ///
    /// This is a target check: if the node answers with success, this is a real SPB
    /// connection and the problem lies in the sequence format; if it answers with a
    /// failure, the target is wrong.
    pub fn lock_connection(&mut self) -> i32 {
        if self.request_lost {
            return -1;
        }
        // SAFETY: the target and request are valid; the request has no buffers.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfIoTargetFormatRequestForIoctl,
                self.target,
                self.request,
                IOCTL_SPB_LOCK_CONNECTION,
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                core::ptr::null_mut(),
            )
        };
        if !nt_ok(status) {
            self.last_status = status;
            return status;
        }
        let mut options: WDF_REQUEST_SEND_OPTIONS = unsafe { core::mem::zeroed() };
        options.Size = size_of_ulong::<WDF_REQUEST_SEND_OPTIONS>();
        options.Flags = (WDF_REQUEST_SEND_OPTION_SYNCHRONOUS | WDF_REQUEST_SEND_OPTION_TIMEOUT) as ULONG;
        options.Timeout = SPB_TIMEOUT_100NS;
        // SAFETY: synchronous send at passive level.
        let sent = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfRequestSend,
                self.request,
                self.target,
                &raw mut options,
            )
        };
        if sent == 0 {
            let _ = self.send_failed("the I/O target did not complete the request");
            return -1;
        }
        // SAFETY: the request is complete, we read its status.
        let status = unsafe { call_unsafe_wdf_function_binding!(WdfRequestGetStatus, self.request) };
        self.last_status = status;
        unsafe { reuse_request(self.request) };
        status
    }

    /// Fills the transfer area for an SPMI register with a 16-bit address.
    ///
    /// # Safety
    ///
    /// The transfer area is allocated with size [`TRANSFER_AREA`]; passive level.
    unsafe fn prepare_spmi16(
        &mut self,
        addr: u16,
        value: Option<u8>,
        big_endian: bool,
    ) -> Result<(), BusError> {
        if self.area.is_null() || self.data.is_null() {
            return Err(BusError::unsupported("SPB buffers not created"));
        }
        let list = self.area.cast::<SpbTransferList>();
        let write = value.is_some();
        let count: u32 = if write { 1 } else { 2 };
        self.last_count = count;
        let size_field = SpbTransferList::header_size();
        // SAFETY: writing the list header into the allocated area.
        unsafe {
            (*list).size = u32::try_from(size_field).unwrap_or(0);
            (*list).reserved = 0;
            (*list).transfer_count = count;
        }
        let format = if self.variant == VARIANT_OUTPUT_NON_PAGED
            || self.variant == VARIANT_OUTPUT_MEMORY_NON_PAGED
        {
            SPB_FORMAT_SIMPLE_NON_PAGED
        } else {
            SPB_FORMAT_SIMPLE
        };
        let addr_bytes = if big_endian {
            addr.to_be_bytes()
        } else {
            addr.to_le_bytes()
        };
        let payload = self.data;
        let read_target = unsafe { self.data.add(2) };
        match value {
            Some(byte) => {
                // SAFETY: the data buffer is >= 3 bytes; one "address + value" transfer.
                unsafe {
                    core::ptr::write_volatile(payload, addr_bytes[0]);
                    core::ptr::write_volatile(payload.add(1), addr_bytes[1]);
                    core::ptr::write_volatile(payload.add(2), byte);
                    let entry = core::ptr::addr_of_mut!((*list).transfers[0]);
                    *entry = Self::entry_with_format(
                        format,
                        SPB_DIRECTION_TO_DEVICE,
                        payload.cast::<core::ffi::c_void>(),
                        3,
                    );
                }
            }
            None => {
                // SAFETY: the address is 2 bytes; the second transfer reads 1 byte.
                unsafe {
                    core::ptr::write_volatile(payload, addr_bytes[0]);
                    core::ptr::write_volatile(payload.add(1), addr_bytes[1]);
                    let first = core::ptr::addr_of_mut!((*list).transfers[0]);
                    *first = Self::entry_with_format(
                        format,
                        SPB_DIRECTION_TO_DEVICE,
                        payload.cast::<core::ffi::c_void>(),
                        2,
                    );
                    let second = self
                        .area
                        .add(SpbTransferList::header_size())
                        .cast::<SpbTransferListEntry>();
                    *second = Self::entry_with_format(
                        format,
                        SPB_DIRECTION_FROM_DEVICE,
                        read_target.cast::<core::ffi::c_void>(),
                        1,
                    );
                }
            }
        }
        Ok(())
    }

    /// Fills the transfer area for a register read or write.
    ///
    /// # Safety
    ///
    /// The transfer area is allocated with size [`TRANSFER_AREA`]; the call is made
    /// at passive level and serialization is provided by the caller.
    unsafe fn prepare(&mut self, addr: RegAddr, value: Option<u8>) -> Result<(), BusError> {
        if self.area.is_null() || self.data.is_null() {
            return Err(BusError::unsupported("SPB buffers not created"));
        }
        // SAFETY: the area alignment is provided by the non-paged pool allocator.
        let list = self.area.cast::<SpbTransferList>();
        // How many transfers we declare and what we write into the `Size` field.
        let write = value.is_some();
        let terminated = self.variant == VARIANT_TERMINATED;
        let count: u32 = match (write, terminated) {
            (true, false) => 1,
            (false, false) => 2,
            (true, true) => 2,
            (false, true) => 3,
        };
        self.last_count = count;
        // The `Size` field is ALWAYS `sizeof(SPB_TRANSFER_LIST)` - the header plus
        // one entry, regardless of the number of transfers (see `spb.h`:
        // "List size - must be set to sizeof(SPB_TRANSFER_LIST)").
        let size_field = SpbTransferList::header_size();
        // SAFETY: writing the list header.
        unsafe {
            (*list).size = u32::try_from(size_field).unwrap_or(0);
            (*list).reserved = 0;
            (*list).transfer_count = count;
        }

        // The bytes live in the request data buffer: the transfer list fills the
        // input buffer completely, so the data does not fit inside it.
        let format =
            if self.variant == VARIANT_OUTPUT_NON_PAGED || self.variant == VARIANT_OUTPUT_MEMORY_NON_PAGED {
                SPB_FORMAT_SIMPLE_NON_PAGED
            } else {
                SPB_FORMAT_SIMPLE
            };
        let payload = self.data;
        // The reply byte follows the address in the same buffer.
        // SAFETY: the output buffer is 8 bytes, the second byte is within it.
        let read_target = unsafe { self.data.add(1) };
        match value {
            Some(byte) => {
                // Write: one "address + value" transfer.
                let mut frame = [addr, byte];
                // SAFETY: we copy two bytes into the allocated area.
                unsafe {
                    core::ptr::copy_nonoverlapping(frame.as_mut_ptr(), payload, 2);
                    let entry = core::ptr::addr_of_mut!((*list).transfers[0]);
                    *entry = Self::entry_with_format(
                        format,
                        SPB_DIRECTION_TO_DEVICE,
                        payload.cast::<core::ffi::c_void>(),
                        2,
                    );
                }
                // No second transfer is needed: the list declares one transfer, and
                // its buffer has exactly that length.
            }
            None => {
                // Read: write the address, then read the byte.
                // SAFETY: we write the address into the data area.
                unsafe {
                    core::ptr::write_volatile(payload, addr);
                    let first = core::ptr::addr_of_mut!((*list).transfers[0]);
                    *first = Self::entry_with_format(
                        format,
                        SPB_DIRECTION_TO_DEVICE,
                        payload.cast::<core::ffi::c_void>(),
                        1,
                    );
                    let second = self
                        .area
                        .add(SpbTransferList::header_size())
                        .cast::<SpbTransferListEntry>();
                    *second = Self::entry_with_format(
                        format,
                        SPB_DIRECTION_FROM_DEVICE,
                        read_target.cast::<core::ffi::c_void>(),
                        1,
                    );
                }
            }
        }
        if terminated {
            // Terminating entry with the `None` direction: this is the only ABI
            // element we never used at all before.
            // SAFETY: the entry lies within the allocated area.
            unsafe {
                let offset = SpbTransferList::header_size()
                    + usize::try_from(count).unwrap_or(2).saturating_sub(2)
                        * SpbTransferList::entry_size();
                let tail = self.area.add(offset).cast::<SpbTransferListEntry>();
                *tail =
                    Self::entry_with_format(format, SPB_DIRECTION_NONE, core::ptr::null_mut(), 0);
            }
        }
        Ok(())
    }

    /// Releases the WDF objects.
    ///
    /// Kept for explicit closing during bring-up: the I/O target belongs to the
    /// device and is released by WDF automatically.
    ///
    /// # Safety
    ///
    /// The handles must be valid; the call is at passive level.
    #[allow(dead_code)]
    pub unsafe fn close(self) {
        // SAFETY: the target was created in `open`.
        unsafe {
            call_unsafe_wdf_function_binding!(WdfIoTargetClose, self.target);
        }
    }
}

impl RegisterBus for SpbBus {
    fn read(&mut self, addr: RegAddr) -> Result<u8, BusError> {
        self.transact(addr, None)
    }

    fn write(&mut self, addr: RegAddr, value: u8) -> Result<(), BusError> {
        self.transact(addr, Some(value)).map(|_| ())
    }

    /// Reopening the bus is done at the device level: the target lives until the
    /// device is removed, so confirming readiness is enough here.
    fn reset(&mut self) -> Result<(), BusError> {
        Ok(())
    }

    fn name(&self) -> &'static str {
        self.name
    }
}

fn nt_ok(status: NTSTATUS) -> bool {
    status >= 0
}

fn size_of_ulong<T>() -> ULONG {
    u32::try_from(core::mem::size_of::<T>()).unwrap_or(0)
}

/// Returns a completed request to its initial state.
///
/// # Safety
///
/// `request` must be completed and not used concurrently.
unsafe fn reuse_request(request: WDFREQUEST) {
    let mut params = wdk_sys::WDF_REQUEST_REUSE_PARAMS {
        Size: size_of_ulong::<wdk_sys::WDF_REQUEST_REUSE_PARAMS>(),
        Flags: 0,
        Status: wdk_sys::STATUS_SUCCESS,
        NewIrp: core::ptr::null_mut(),
    };
    params.Size = size_of_ulong::<wdk_sys::WDF_REQUEST_REUSE_PARAMS>();
    // SAFETY: the request is complete, the parameters are filled in.
    unsafe {
        let _ = call_unsafe_wdf_function_binding!(WdfRequestReuse, request, &raw mut params);
    }
}

/// Path to the Resource Hub node: prefix plus 16 hexadecimal digits.
///
/// The format matches `RESOURCE_HUB_ID_TO_FILE_NAME` (`%0*I64x`, width 16) from
/// the WDK `reshub.h`.
#[must_use]
pub fn resource_hub_path(peripheral_id: u64) -> HubPath {
    let mut chars = [0_u16; HUB_PATH_CHARS];
    let prefix = RESOURCE_HUB_PREFIX.as_bytes();
    for (index, byte) in prefix.iter().enumerate() {
        if let Some(slot) = chars.get_mut(index) {
            *slot = if *byte == b'/' {
                b'\\' as u16
            } else {
                u16::from(*byte)
            };
        }
    }
    for position in 0..16_u32 {
        let shift = 60_u32.saturating_sub(position.saturating_mul(4));
        let nibble = ((peripheral_id >> shift) & 0xF) as u8;
        let symbol = match nibble {
            0..=9 => b'0' + nibble,
            _ => b'a' + (nibble - 10),
        };
        let index = RESOURCE_HUB_PREFIX.len().saturating_add(usize::try_from(position).unwrap_or(0));
        if let Some(slot) = chars.get_mut(index) {
            *slot = u16::from(symbol);
        }
    }
    HubPath { chars }
}

/// Maximum length of an object name in a probe (in characters).
pub const PROBE_NAME_CHARS: usize = 64;

/// Share access: read + write + delete (`FILE_SHARE_READ|WRITE|DELETE`).
pub const PROBE_SHARE_ALL: u32 = 0x0000_0007;

/// Probe: whether a kernel object with the given name can be opened.
///
/// Needed for diagnostics: we check whether the kernel namespace holds an SPMI bus
/// object (for example `\Device\Spmi\SUPERUSER`) or a symbolic link of the stock
/// PMIC/ADC (`\DosDevices\Global\QCOMPMIC`, `\??\QCOM_ADC`). Some of them are
/// invisible from user mode, while the driver opens them the same way as the
/// resource node: by name through `WdfIoTargetOpenByName`.
///
/// The target is closed and deleted after the probe: otherwise a successful open
/// would hold an exclusive reference to someone else's stack (SUPERUSER / ADC).
///
/// Names are ASCII only: the buffer is filled byte by byte.
///
/// # Safety
///
/// Passive IRQL, the device is created and not being removed.
pub unsafe fn probe_named_target(device: WDFDEVICE, name: &str, desired_access: u32) -> i32 {
    // SAFETY: passive level; we delegate to the common probe with zero ShareAccess.
    unsafe { probe_named_target_ex(device, name, desired_access, 0) }
}

/// Probe for opening a kernel object with an explicit share-access mask.
///
/// # Safety
///
/// Passive IRQL, the device is created and not being removed.
pub unsafe fn probe_named_target_ex(
    device: WDFDEVICE,
    name: &str,
    desired_access: u32,
    share_access: u32,
) -> i32 {
    let mut chars = [0_u16; PROBE_NAME_CHARS];
    let mut length = 0_usize;
    for byte in name.bytes() {
        if let Some(slot) = chars.get_mut(length) {
            *slot = u16::from(byte);
            length = length.saturating_add(1);
        }
    }
    let mut io_target: WDFIOTARGET = WDF_NO_HANDLE.cast();
    // SAFETY: the device is created; the handle is a local variable.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfIoTargetCreate,
            device,
            WDF_NO_OBJECT_ATTRIBUTES,
            &raw mut io_target,
        )
    };
    if !nt_ok(status) {
        return status;
    }
    let bytes = u16::try_from(length.saturating_mul(2)).unwrap_or(0);
    let mut params: WDF_IO_TARGET_OPEN_PARAMS = unsafe { core::mem::zeroed() };
    params.Size = size_of_ulong::<WDF_IO_TARGET_OPEN_PARAMS>();
    params.Type = WdfIoTargetOpenByName;
    params.TargetDeviceName = UNICODE_STRING {
        Length: bytes,
        MaximumLength: bytes,
        Buffer: chars.as_mut_ptr(),
    };
    params.DesiredAccess = desired_access;
    params.ShareAccess = share_access;
    params.CreateDisposition = wdk_sys::FILE_OPEN;
    params.FileAttributes = wdk_sys::FILE_ATTRIBUTE_NORMAL;
    // Same as qcpmic8150: FILE_NON_DIRECTORY_FILE.
    params.CreateOptions = 0x0000_0040;
    // SAFETY: the target is created, the parameters are filled in, passive level.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(WdfIoTargetOpen, io_target, &raw mut params)
    };
    // The target is always closed: on success (we do not hold someone else's stack)
    // and on failure (otherwise WdfIoTargetCreate leaves an unclosed object).
    // SAFETY: the target was created above; passive level.
    unsafe {
        if nt_ok(status) {
            call_unsafe_wdf_function_binding!(WdfIoTargetClose, io_target);
        }
        call_unsafe_wdf_function_binding!(WdfObjectDelete, io_target.cast());
    }
    status
}

/// Result of the SUPERUSER probe: the open status and the APSD byte (if the read succeeded).
#[derive(Debug, Clone, Copy)]
pub struct SuperuserApsdProbe {
    /// `NTSTATUS` of the `\Device\Spmi\SUPERUSER` open.
    pub open_status: i32,
    /// `NTSTATUS` of the peri-grant `0x13` (or `0xFFFFFFFF` if the open failed).
    pub grant_status: i32,
    /// `NTSTATUS` of the `0x1307` read (or `0xFFFFFFFF` if open/grant cut the path off).
    pub read_status: i32,
    /// `APSD_STATUS` value if `read_status == 0`.
    pub value: u8,
}

/// Opens `\Device\Spmi\SUPERUSER`, grants peri `0x13`, reads `APSD_STATUS` (`0x1307`).
///
/// The SUPERUSER slot is limited to three concurrent opens (`qcpmic` /
/// `qcpmicext` / `qcpmgpio`). If all three are taken, the open returns `0xC0000001`.
/// With a free slot (after a reboot or disabling one client) the path works.
///
/// # Safety
///
/// Passive IRQL, the device is created and not being removed.
pub unsafe fn probe_superuser_apsd(device: WDFDEVICE) -> SuperuserApsdProbe {
    let mut out = SuperuserApsdProbe {
        open_status: -1,
        grant_status: -1_i32,
        read_status: -1_i32,
        value: 0,
    };
    let mut chars = [0_u16; PROBE_NAME_CHARS];
    let name = "\\Device\\Spmi\\SUPERUSER";
    let mut length = 0_usize;
    for byte in name.bytes() {
        if let Some(slot) = chars.get_mut(length) {
            *slot = u16::from(byte);
            length = length.saturating_add(1);
        }
    }
    let mut io_target: WDFIOTARGET = WDF_NO_HANDLE.cast();
    // SAFETY: the device is created; the handle is local.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfIoTargetCreate,
            device,
            WDF_NO_OBJECT_ATTRIBUTES,
            &raw mut io_target,
        )
    };
    if !nt_ok(status) {
        out.open_status = status;
        return out;
    }
    let bytes = u16::try_from(length.saturating_mul(2)).unwrap_or(0);
    let mut params: WDF_IO_TARGET_OPEN_PARAMS = unsafe { core::mem::zeroed() };
    params.Size = size_of_ulong::<WDF_IO_TARGET_OPEN_PARAMS>();
    params.Type = WdfIoTargetOpenByName;
    params.TargetDeviceName = UNICODE_STRING {
        Length: bytes,
        MaximumLength: bytes,
        Buffer: chars.as_mut_ptr(),
    };
    // Same as qcpmic8150: GENERIC_READ|GENERIC_WRITE, share all, FILE_NON_DIRECTORY_FILE.
    params.DesiredAccess = 0xC000_0000;
    params.ShareAccess = PROBE_SHARE_ALL;
    params.CreateDisposition = wdk_sys::FILE_OPEN;
    params.FileAttributes = wdk_sys::FILE_ATTRIBUTE_NORMAL;
    params.CreateOptions = 0x0000_0040;
    // SAFETY: the target is created, passive level.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(WdfIoTargetOpen, io_target, &raw mut params)
    };
    out.open_status = status;
    if !nt_ok(status) {
        unsafe {
            call_unsafe_wdf_function_binding!(WdfObjectDelete, io_target.cast());
        }
        return out;
    }

    // peri-grant: u16 count=1, u16 peri=0x0013
    let grant = [1_u8, 0, 0x13, 0];
    let mut request: WDFREQUEST = WDF_NO_HANDLE.cast();
    let mut in_mem: WDFMEMORY = WDF_NO_HANDLE.cast();
    let mut in_ptr: *mut core::ffi::c_void = core::ptr::null_mut();
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfRequestCreate,
            WDF_NO_OBJECT_ATTRIBUTES,
            io_target,
            &raw mut request,
        )
    };
    if !nt_ok(status) {
        out.grant_status = status;
        unsafe {
            call_unsafe_wdf_function_binding!(WdfIoTargetClose, io_target);
            call_unsafe_wdf_function_binding!(WdfObjectDelete, io_target.cast());
        }
        return out;
    }
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfMemoryCreate,
            WDF_NO_OBJECT_ATTRIBUTES,
            wdk_sys::_POOL_TYPE::NonPagedPool,
            0,
            grant.len(),
            &raw mut in_mem,
            &raw mut in_ptr,
        )
    };
    if !nt_ok(status) {
        out.grant_status = status;
        unsafe {
            call_unsafe_wdf_function_binding!(WdfObjectDelete, request.cast());
            call_unsafe_wdf_function_binding!(WdfIoTargetClose, io_target);
            call_unsafe_wdf_function_binding!(WdfObjectDelete, io_target.cast());
        }
        return out;
    }
    unsafe {
        core::ptr::copy_nonoverlapping(grant.as_ptr(), in_ptr.cast::<u8>(), grant.len());
    }
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfIoTargetFormatRequestForIoctl,
            io_target,
            request,
            IOCTL_SPMI_SUPERUSER_GRANT,
            in_mem,
            core::ptr::null_mut(),
            WDF_NO_HANDLE.cast(),
            core::ptr::null_mut(),
        )
    };
    if nt_ok(status) {
        let mut options: WDF_REQUEST_SEND_OPTIONS = unsafe { core::mem::zeroed() };
        options.Size = size_of_ulong::<WDF_REQUEST_SEND_OPTIONS>();
        options.Flags =
            (WDF_REQUEST_SEND_OPTION_SYNCHRONOUS | WDF_REQUEST_SEND_OPTION_TIMEOUT) as ULONG;
        options.Timeout = SPB_TIMEOUT_100NS;
        let sent = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfRequestSend,
                request,
                io_target,
                &raw mut options,
            )
        };
        out.grant_status = if sent == 0 {
            -1
        } else {
            unsafe { call_unsafe_wdf_function_binding!(WdfRequestGetStatus, request) }
        };
    } else {
        out.grant_status = status;
    }
    unsafe {
        call_unsafe_wdf_function_binding!(WdfObjectDelete, in_mem.cast());
        call_unsafe_wdf_function_binding!(WdfObjectDelete, request.cast());
    }

    // READ APSD_STATUS: header + 1-byte output. addr_enc = (sid2 << 16) | 0x1307
    let mut header = [0_u8; SPMI_SUPERUSER_HEADER_LEN];
    header[4] = 0x07;
    header[5] = 0x13;
    header[6] = 0x02;
    header[7] = 0x00;
    header[8] = 0x01;
    let mut request: WDFREQUEST = WDF_NO_HANDLE.cast();
    let mut in_mem: WDFMEMORY = WDF_NO_HANDLE.cast();
    let mut out_mem: WDFMEMORY = WDF_NO_HANDLE.cast();
    let mut in_ptr: *mut core::ffi::c_void = core::ptr::null_mut();
    let mut out_ptr: *mut core::ffi::c_void = core::ptr::null_mut();
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfRequestCreate,
            WDF_NO_OBJECT_ATTRIBUTES,
            io_target,
            &raw mut request,
        )
    };
    if !nt_ok(status) {
        out.read_status = status;
        unsafe {
            call_unsafe_wdf_function_binding!(WdfIoTargetClose, io_target);
            call_unsafe_wdf_function_binding!(WdfObjectDelete, io_target.cast());
        }
        return out;
    }
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfMemoryCreate,
            WDF_NO_OBJECT_ATTRIBUTES,
            wdk_sys::_POOL_TYPE::NonPagedPool,
            0,
            header.len(),
            &raw mut in_mem,
            &raw mut in_ptr,
        )
    };
    if !nt_ok(status) {
        out.read_status = status;
        unsafe {
            call_unsafe_wdf_function_binding!(WdfObjectDelete, request.cast());
            call_unsafe_wdf_function_binding!(WdfIoTargetClose, io_target);
            call_unsafe_wdf_function_binding!(WdfObjectDelete, io_target.cast());
        }
        return out;
    }
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfMemoryCreate,
            WDF_NO_OBJECT_ATTRIBUTES,
            wdk_sys::_POOL_TYPE::NonPagedPool,
            0,
            1,
            &raw mut out_mem,
            &raw mut out_ptr,
        )
    };
    if !nt_ok(status) {
        out.read_status = status;
        unsafe {
            call_unsafe_wdf_function_binding!(WdfObjectDelete, in_mem.cast());
            call_unsafe_wdf_function_binding!(WdfObjectDelete, request.cast());
            call_unsafe_wdf_function_binding!(WdfIoTargetClose, io_target);
            call_unsafe_wdf_function_binding!(WdfObjectDelete, io_target.cast());
        }
        return out;
    }
    unsafe {
        core::ptr::copy_nonoverlapping(header.as_ptr(), in_ptr.cast::<u8>(), header.len());
        core::ptr::write_volatile(out_ptr.cast::<u8>(), 0xFF);
    }
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfIoTargetFormatRequestForIoctl,
            io_target,
            request,
            IOCTL_SPMI_SUPERUSER_READ,
            in_mem,
            core::ptr::null_mut(),
            out_mem,
            core::ptr::null_mut(),
        )
    };
    if nt_ok(status) {
        let mut options: WDF_REQUEST_SEND_OPTIONS = unsafe { core::mem::zeroed() };
        options.Size = size_of_ulong::<WDF_REQUEST_SEND_OPTIONS>();
        options.Flags =
            (WDF_REQUEST_SEND_OPTION_SYNCHRONOUS | WDF_REQUEST_SEND_OPTION_TIMEOUT) as ULONG;
        options.Timeout = SPB_TIMEOUT_100NS;
        let sent = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfRequestSend,
                request,
                io_target,
                &raw mut options,
            )
        };
        out.read_status = if sent == 0 {
            -1
        } else {
            unsafe { call_unsafe_wdf_function_binding!(WdfRequestGetStatus, request) }
        };
        if nt_ok(out.read_status) {
            out.value = unsafe { core::ptr::read_volatile(out_ptr.cast::<u8>()) };
        }
    } else {
        out.read_status = status;
    }
    unsafe {
        call_unsafe_wdf_function_binding!(WdfObjectDelete, out_mem.cast());
        call_unsafe_wdf_function_binding!(WdfObjectDelete, in_mem.cast());
        call_unsafe_wdf_function_binding!(WdfObjectDelete, request.cast());
        call_unsafe_wdf_function_binding!(WdfIoTargetClose, io_target);
        call_unsafe_wdf_function_binding!(WdfObjectDelete, io_target.cast());
    }
    out
}

/// SID PM8150B USBIN (charger peripheral on SPMI).
pub const SPMI_SID_USBIN: u8 = 2;
/// Peri-grant id for USBIN (`0x13` — matches register bank `0x13xx`).
pub const SPMI_PERI_USBIN: u16 = 0x0013;
/// Peri-grant id of the PM8150B `batt_soc` peripheral (fuel gauge, bank `0x40xx`).
pub const SPMI_PERI_BATT_SOC: u16 = 0x0040;
/// `BATT_SOC_SUBTYPE`: on PM8150B it is `0x10` (`FG_BATT_SOC_PM8150B`).
pub const SPMI_REG_BATT_SOC_SUBTYPE: u16 = 0x4005;
/// `FG_MONOTONIC_SOC` - raw state of charge, 8 bits, `0...255` (255 = 100 %).
///
/// The shadow copy of the value follows it (`+0x0A`); both are read in one
/// two-byte request, as in `fg_get_msoc_raw`, and must match.
pub const SPMI_REG_BATT_SOC: u16 = 0x4009;
/// Expected subtype of the `batt_soc` peripheral on PM8150B.
pub const SPMI_BATT_SOC_SUBTYPE_PM8150B: u8 = 0x10;

/// Reads the raw state of charge from the PM8150B fuel gauge.
///
/// Android takes the same value in `fg_get_msoc_raw` (`drivers_power_supply_qcom_fg-util.c`):
/// it reads **two** bytes at `FG_MONOTONIC_SOC` and requires them to be equal (up to
/// five attempts), because these are shadow registers of one number, and a mismatch
/// means the gauge is being updated right now. The same check applies here: on a
/// mismatch the read counts as failed rather than as a reason to guess, and the value
/// arrives on the next telemetry tick.
///
/// The subtype is read in the same open: the address `0x4000` may hold a different
/// peripheral on another platform, and then `0x4009` is not a state of charge.
///
/// # Safety
///
/// `PASSIVE_LEVEL`; `device` is live.
pub unsafe fn read_batt_soc_raw(device: WDFDEVICE) -> Option<u8> {
    // SAFETY: passive level, the device is created.
    let mut su = unsafe { SuperuserBus::open(device) }.ok()?;
    su.grant(SPMI_PERI_BATT_SOC).ok()?;
    let subtype = su.read_u8(SPMI_SID_USBIN, SPMI_REG_BATT_SOC_SUBTYPE).ok()?;
    if subtype != SPMI_BATT_SOC_SUBTYPE_PM8150B {
        return None;
    }
    let mut cap = [0_u8; 2];
    su.read_bytes(SPMI_SID_USBIN, SPMI_REG_BATT_SOC, &mut cap)
        .ok()?;
    // The second cell is the copy; it is read at `+0x0A`, but `read_bytes` returned
    // both bytes in one request because the registers are consecutive.
    if cap[0] != cap[1] {
        return None;
    }
    Some(cap[0])
}

/// Peri-grant id of the CHGR peripheral (`0x10` - bank `0x10xx`).
///
/// The value is not derived from the address but taken from the nabu vendor DT
/// (`android_kernel_xiaomi_nabu/arch/arm64/boot/dts/qcom/pm8150b.dtsi:190-191`):
/// the node `qcom,chgr@1000`, `reg = <0x1000 0x100>`. The same base address sits
/// at the head of the SMB5 register map - `drivers_power_supply_qcom_smb5-reg.h:18`
/// (`CHGR_BASE 0x1000`).
pub const SPMI_PERI_CHGR: u16 = 0x0010;

/// Peri-grant id of the `batt_info` peripheral (`0x41` - bank `0x41xx`).
///
/// The cell current does not live in `batt_soc` (`0x4000`, where the driver takes
/// the percent from) but in the neighbouring `batt_info` peripheral: in the same DT
/// `qcom,fg-batt-info@4100` with `reg = <0x4100 0x100>` (same file, lines 413-415),
/// and the vendor parser assigns the base address from the peripheral subtype:
/// `android_kernel_xiaomi_nabu/drivers/power/supply/qcom/qpnp-fg-gen4.c:6704-6705`
/// (`case FG_BATT_INFO_PM8150B: fg->batt_info_base = base;`).
pub const SPMI_PERI_BATT_INFO: u16 = 0x0041;

/// `CHGR_FAST_CHARGE_CURRENT_CFG_REG` - FCC of the SMB5 buck, 50 mA per step.
///
/// Address: `drivers_power_supply_qcom_smb5-reg.h:79` (`CHGR_BASE + 0x61`). The step
/// and the ceiling are the PM8150B parameters from
/// `drivers_power_supply_qcom_qpnp-smb5.c:128-134` (`.min_u = 0`, `.max_u = 8 000 000`,
/// `.step_u = 50 000`), that is raw `0x14` = 1.00 A.
pub const SPMI_REG_CHGR_FCC: u16 = 0x1061;

/// `CHARGING_ENABLE_CMD_REG`, bit 0 - the "charging enabled" command
/// (`drivers_power_supply_qcom_smb5-reg.h:67-68`).
pub const SPMI_REG_CHGR_CHARGING_ENABLE: u16 = 0x1042;

/// `CHGR_CFG2_REG`, bit 0 `CHARGER_INHIBIT_BIT` - hardware charge inhibit
/// (`drivers_power_supply_qcom_smb5-reg.h:73-77`).
pub const SPMI_REG_CHGR_CFG2: u16 = 0x1051;

/// `BATTERY_CHARGER_STATUS_1_REG`, bits `[2:0]` - charge phase
/// (`drivers_power_supply_qcom_smb5-reg.h:36-45`).
pub const SPMI_REG_CHGR_STATUS_1: u16 = 0x1006;

/// `CHGR_FLOAT_VOLTAGE_CFG_REG` - charge termination voltage, 10 mV per step from
/// 3.6 V (`drivers_power_supply_qcom_smb5-reg.h:95`; the step is in
/// `drivers_power_supply_qcom_qpnp-smb5.c:136-141`).
pub const SPMI_REG_CHGR_FLOAT_VOLTAGE: u16 = 0x1070;

/// `USBIN_CURRENT_LIMIT_CFG_REG` - USBIN input current limit, 50 mA per step
/// (`drivers_power_supply_qcom_smb5-reg.h:322`; the step is in
/// `drivers_power_supply_qcom_qpnp-smb5.c:143-148`).
pub const SPMI_REG_USBIN_ICL: u16 = 0x1370;

/// `USBIN_ADAPTER_ALLOW_CFG_REG` - which voltages are allowed for the adapter
/// (`drivers_power_supply_qcom_smb5-reg.h:285`).
pub const SPMI_REG_USBIN_ADAPTER_ALLOW: u16 = 0x1360;

/// `BATT_INFO_IBATT_LSB` - low byte of the cell current (16 bits, LE, sign is bit 15).
///
/// The offset is given in `drivers_power_supply_qcom_fg-reg.h:250-251`
/// (`batt_info_base + 0xA2`/`+0xA3`); the base address of this peripheral on nabu is
/// `0x4100` (see [`SPMI_PERI_BATT_INFO`]).
pub const SPMI_REG_FG_IBATT_LSB: u16 = 0x41A2;

/// Shadow copy of the same current (`BATT_INFO_IBATT_LSB_CP`,
/// `drivers_power_supply_qcom_fg-reg.h:261`).
///
/// The vendor reads both pairs and requires them to be equal
/// (`drivers_power_supply_qcom_fg-util.c:1005-1027`, the comparison is at `:1020`):
/// the pair is updated by the gauge as a whole, and a mismatch means the read fell
/// inside an update.
pub const SPMI_REG_FG_IBATT_LSB_CP: u16 = 0x41A8;

/// `BATT_INFO_VBATT_LSB` - low byte of the cell voltage (16 bits, LE, unsigned).
///
/// The offset is given in `drivers_power_supply_qcom_fg-reg.h:246-247`
/// (`batt_info_base + 0xA0`/`+0xA1`), the same `0x4100` peripheral as the current.
pub const SPMI_REG_FG_VBATT_LSB: u16 = 0x41A0;

/// Shadow copy of the same voltage (`BATT_INFO_VBATT_LSB_CP`,
/// `drivers_power_supply_qcom_fg-reg.h:259-260`, `batt_info_base + 0xA6`).
///
/// Checked against the header: the address matches what the vendor reads in
/// `drivers_power_supply_qcom_fg-util.c:1057`, and belongs to the same v2.0+ pair
/// as [`SPMI_REG_FG_IBATT_LSB_CP`]. The equality condition is there too, at
/// `:1064`, and it is exactly the same as for the current.
pub const SPMI_REG_FG_VBATT_LSB_CP: u16 = 0x41A6;

/// Cell voltage step numerator: `V[µV] = raw * 122070 / 1000`.
///
/// Taken from the vendor decoder `drivers_power_supply_qcom_fg-util.c:1041-1042`
/// (`BATT_VOLTAGE_NUMR 122070`, `BATT_VOLTAGE_DENR 1000`) and its use in the same
/// file at `:1079`; the result goes straight into `POWER_SUPPLY_PROP_VOLTAGE_NOW`
/// (`drivers_power_supply_qcom_qpnp-fg-gen4.c:5184-5188`), and that quantity in
/// power_supply is microvolts (same file at `:5131`, `vbatt_uv/1000` is millivolts).
/// That gives 122.07 µV per step, about 8 mV over the whole cell range.
pub const FG_VBATT_NUMER: u32 = 122_070;

/// Cell voltage step denominator (`BATT_VOLTAGE_DENR`,
/// `drivers_power_supply_qcom_fg-util.c:1042`).
pub const FG_VBATT_DENOM: u32 = 1_000;

/// Cell current step numerator: `I[µA] = raw * 488281 / 1000`.
///
/// Taken from the vendor decoder `drivers_power_supply_qcom_fg-util.c:997-998`
/// (`BATT_CURRENT_NUMR 488281`, `BATT_CURRENT_DENR 1000`) and its use in the same
/// file at `:1036-1037` (`sign_extend32(temp, 15)`, then
/// `temp * BATT_CURRENT_NUMR / BATT_CURRENT_DENR`); the result goes straight into
/// `POWER_SUPPLY_PROP_CURRENT_NOW`
/// (`android_kernel_xiaomi_nabu/drivers/power/supply/qcom/qpnp-fg-gen4.c:5190-5191`),
/// and that quantity in power_supply is microamps. That gives 1/2048 A per step.
pub const FG_IBATT_NUMER: i32 = 488_281;

/// Cell current step denominator (`BATT_CURRENT_DENR`,
/// `drivers_power_supply_qcom_fg-util.c:998`).
pub const FG_IBATT_DENOM: i32 = 1_000;

/// Snapshot of the SMB5 (PM8150B) registers and the cell current - for
/// `Chgr*`/`FgIbatUa` diagnostics.
///
/// The fields are raw bytes as they sit in SPMI: decoding is left to the journal
/// analysis, because each quantity has its own steps and offsets (see the constants
/// above), and an error in the decoder costs more than it helps.
#[derive(Debug, Clone, Copy)]
pub struct ChargeRegs {
    /// Buck FCC (`0x1061`), 50 mA per step.
    pub fcc_raw: u8,
    /// "Charging enabled" command (`0x1042`), bit 0.
    pub charge_enable: u8,
    /// Hardware charge inhibit (`0x1051`), bit 0.
    pub inhibit: u8,
    /// Charge phase (`0x1006`), bits `[2:0]`.
    pub chgr_status: u8,
    /// Charge termination voltage (`0x1070`), 10 mV per step from 3.6 V.
    pub fv_raw: u8,
    /// USBIN input current limit (`0x1370`), 50 mA per step.
    pub icl_raw: u8,
    /// Voltages allowed for the adapter (`0x1360`).
    pub usbin_allow: u8,
    /// Cell current from the fuel gauge, µA. The sign is the vendor's (bit 15 of
    /// the raw value): **negative is current into the cell (charge)**, positive is
    /// discharge. In this `u32` a negative quantity sits in two's complement, so the
    /// consumer needs the magnitude, not the number itself:
    /// `(ibatt_ua as i32).unsigned_abs()`.
    ///
    /// The vendor reads the sign the same way: `qcom/smb5-lib.c` (16.0 branch) treats
    /// the cell as charging at `ibat < -450 mA`, and `ti/cp_qc30.c` flips the sign of
    /// the gauge current before use.
    ///
    /// **But the sign was measured on 22.09 not to discriminate the two directions on
    /// this board**, so it must not be used as a direction witness: the field read
    /// negative in *both* states - while the pump pushed ~3.5 A into a pack whose SOC
    /// climbed 223 -> 234, and while the pack drained at 0.54 A with the cable out, the
    /// cell falling 4.231 -> 4.221 V and the SOC falling 234 -> 233. The magnitude
    /// matched the cell current in both cases; only the sign did not follow. The
    /// direction used by the policy therefore comes from the SOC trend
    /// (`battery_policy::SocTrend`), and this field stays a diagnostic.
    pub ibatt_ua: u32,
    /// Cell voltage from the PM8150B fuel gauge, µV. **Unsigned**: this pair has no
    /// sign, unlike the current.
    ///
    /// This is a cell reading independent of the LN8000. It is needed because
    /// [`ChargeRegs`] is the only place where the real `vbat` is visible:
    /// `AdcChannel::Vbat` on the LN8000 measures the middle of the converter bus
    /// (≈ Vin/2) during 2:1, not the cell, and the whole transfer band is computed
    /// from `vbat` (see `ln8000::encoding::window_target_uv`). An error in `vbat`
    /// shifts the band by a factor of two.
    ///
    /// One step weighs 122.07 µV, and the full 16-bit range (0...65535) fits in a
    /// `u32` with room to spare, unlike the current: negative values never occur
    /// here, so two's complement is not needed.
    pub fg_vbatt_uv: u32,
}

/// Reads the SMB5 (PM8150B) registers and the cell current from the fuel gauge.
///
/// # Why
///
/// On the live tablet the pump holds 2:1 with ~8.6 V and ~0.55 A at the input, so it
/// moves ~1.1 A into the node, yet the cell charge grows as if about 2 A were going
/// in. The platform's second charging branch, the SMB5 (PM8150B) buck, is not
/// configured by the driver: its FCC is still whatever the firmware left, and it has
/// been invisible until now. The LN8000 has no cell current at all - only the
/// PM8150B fuel gauge measures it. Both quantities are read over SPMI the same way
/// the driver takes the percent ([`read_batt_soc_raw`]), and that is the only way to
/// tell "the pump is delivering current into the cell" from "the current is going
/// into the SMB5 buck".
///
/// # What it does
///
/// One SUPERUSER session: open, three grants (CHGR, USBIN, `batt_info`), register
/// reads, close in [`Drop`]. Not a single write. Any failed read gives `None`: a
/// partially filled snapshot would look valid, and decisions are made from it
/// afterwards.
///
/// # Safety
///
/// `PASSIVE_LEVEL`; `device` is live.
pub unsafe fn read_charge_regs(device: WDFDEVICE) -> Option<ChargeRegs> {
    // SAFETY: passive level, the device is created.
    let mut su = unsafe { SuperuserBus::open(device) }.ok()?;
    su.grant(SPMI_PERI_CHGR).ok()?;
    su.grant(SPMI_PERI_USBIN).ok()?;
    su.grant(SPMI_PERI_BATT_INFO).ok()?;
    let fcc_raw = su.read_u8(SPMI_SID_USBIN, SPMI_REG_CHGR_FCC).ok()?;
    let charge_enable = su
        .read_u8(SPMI_SID_USBIN, SPMI_REG_CHGR_CHARGING_ENABLE)
        .ok()?;
    let inhibit = su.read_u8(SPMI_SID_USBIN, SPMI_REG_CHGR_CFG2).ok()?;
    let chgr_status = su.read_u8(SPMI_SID_USBIN, SPMI_REG_CHGR_STATUS_1).ok()?;
    let fv_raw = su
        .read_u8(SPMI_SID_USBIN, SPMI_REG_CHGR_FLOAT_VOLTAGE)
        .ok()?;
    let icl_raw = su.read_u8(SPMI_SID_USBIN, SPMI_REG_USBIN_ICL).ok()?;
    let usbin_allow = su
        .read_u8(SPMI_SID_USBIN, SPMI_REG_USBIN_ADAPTER_ALLOW)
        .ok()?;
    let mut ibatt = [0_u8; 2];
    su.read_bytes(SPMI_SID_USBIN, SPMI_REG_FG_IBATT_LSB, &mut ibatt)
        .ok()?;
    let mut ibatt_cp = [0_u8; 2];
    su.read_bytes(SPMI_SID_USBIN, SPMI_REG_FG_IBATT_LSB_CP, &mut ibatt_cp)
        .ok()?;
    // As in `read_batt_soc_raw` and `fg_get_battery_current`: a mismatch between the
    // pair and its copy is not a reason to guess, the value arrives on the next tick.
    if ibatt != ibatt_cp {
        return None;
    }
    // The cell voltage comes from the same peripheral and the same session: the
    // vendor reads both quantities in the same loop with the same shadow check
    // (`drivers_power_supply_qcom_fg-util.c:1005-1027` for the current and `:1049-1071`
    // for the voltage). The byte order and the absence of a sign follow `:1077`
    // (`temp = buf[1] << 8 | buf[0]`); the `PMI8998_V1_REV_WA` branch (`:1073`) does
    // not apply to nabu: on PM8150B the flag is never set - it is not mentioned in
    // `qpnp-fg-gen4.c`, and only `qpnp-fg-gen3.c` sets it, for `PMI8998_SUBTYPE`.
    let mut vbatt = [0_u8; 2];
    su.read_bytes(SPMI_SID_USBIN, SPMI_REG_FG_VBATT_LSB, &mut vbatt)
        .ok()?;
    let mut vbatt_cp = [0_u8; 2];
    su.read_bytes(SPMI_SID_USBIN, SPMI_REG_FG_VBATT_LSB_CP, &mut vbatt_cp)
        .ok()?;
    // The same cost of a mismatch as for the current: the snapshot is incomplete,
    // so it does not exist.
    if vbatt != vbatt_cp {
        return None;
    }
    // The low byte sits at 0x41A0, the high byte at 0x41A1 (`temp = buf[1] << 8 | buf[0]`
    // in `drivers_power_supply_qcom_fg-util.c:1077`); there is no sign - the vendor does
    // not call `sign_extend32` here, unlike for the current (`:1036`).
    let raw_v = u32::from(u16::from_le_bytes(vbatt));
    // The product is computed in `u64`: 65535 steps is 8.0 V in microvolts, which
    // does not fit in a `u32` - saturating multiplication would give a wrong voltage
    // at the top step.
    let micro_uv = u64::from(raw_v) * u64::from(FG_VBATT_NUMER) / u64::from(FG_VBATT_DENOM);
    // The low byte sits at 0x41A2, the high byte at 0x41A3 (`temp = buf[1] << 8 | buf[0]`
    // in `drivers_power_supply_qcom_fg-util.c:1033`), the sign is bit 15 (same file, `:1036`).
    let raw = i32::from(i16::from_le_bytes(ibatt));
    // The product is computed in `i64`: 32767 steps is 16 A, which does not fit in
    // an `i32` - saturating multiplication here would give a wrong current per step.
    let micro_ua = i64::from(raw) * i64::from(FG_IBATT_NUMER) / i64::from(FG_IBATT_DENOM);
    Some(ChargeRegs {
        fcc_raw,
        charge_enable,
        inhibit,
        chgr_status,
        fv_raw,
        icl_raw,
        usbin_allow,
        // A negative current keeps its sign: the `u32` mark carries it in two's complement.
        ibatt_ua: (micro_ua as i32) as u32,
        // The cell voltage has no sign and is not converted to two's complement.
        fg_vbatt_uv: micro_uv as u32,
    })
}

/// Writes the SMB5 (PM8150B) buck FCC and returns what is **read back** from the
/// `0x1061` register.
///
/// # Why
///
/// Live measurement on 19.09 (build .652, MDY-08-EI): `ChgrFccRaw = 30`, that is
/// 1.50 A - what the firmware left; our driver has never written this register. At
/// the same time `FgIbatUa` ≈ 2.9 A and `SysStsRaw = 0x04` (the pump in 2:1 without
/// the `IIN_LOOP`/`VFLOAT_LOOP` loops, that is giving whatever it is given). The
/// PM8150B buck is the platform's second charging branch, and its 1.5 A sits far
/// below what the tablet vendor DT allows: `qcom,fcc-max-ua = <5900000>`
/// (`arch/arm64/boot/dts/qcom/xiaomi/overlay/nabu/nabu-sm8150.dtsi:68`). Raising the
/// FCC is exactly what Android does when the pump is running, and it is the only
/// register our driver has any right to write at all (see [`SPMI_PERI_CHGR`]).
///
/// # Why without read-modify-write
///
/// The whole register byte is the FCC field: the vendor header declares neither a
/// mask nor a bit for `CHGR_FAST_CHARGE_CURRENT_CFG_REG`
/// (`drivers_power_supply_qcom_smb5-reg.h:79`), unlike the neighbouring
/// `CHGR_CFG2_REG`, which does have `CHARGER_INHIBIT_BIT`. The vendor writer puts
/// exactly `(val_u - min_u) / step_u` into the register as a single byte
/// (`drivers_power_supply_qcom_smb-lib.c:353-373`, `smblib_write` takes a `u8`), and
/// the PM8150B parameter sets `min_u = 0` for this field (`qpnp-smb5.c:128-134`).
/// There is no old value to read before writing: the register has no foreign bits.
///
/// # What it does
///
/// One SUPERUSER session: open, grant CHGR, write, read back, close in [`Drop`]. The
/// read-back is not a formality: a write IOCTL may complete successfully while the
/// register stays as it was (the peripheral is not granted, the chip is in reset),
/// and a silent failure would look like a raised limit. What is returned is the value
/// read, not the value requested, so the caller sees what is really in the register.
///
/// # Errors
///
/// `None` - the open, grant, write or read-back failed. There is no partial success:
/// if we did not read it back, we consider the write never happened.
///
/// # Safety
///
/// `PASSIVE_LEVEL`; `device` is live.
pub unsafe fn write_fcc_raw(device: WDFDEVICE, raw: u8) -> Option<u8> {
    // SAFETY: passive level, the device is created.
    let mut su = unsafe { SuperuserBus::open(device) }.ok()?;
    // The register lives in bank `0x10xx`, so only CHGR is granted: USBIN and
    // `batt_info` are not involved in this write.
    su.grant(SPMI_PERI_CHGR).ok()?;
    su.write_u8(SPMI_SID_USBIN, SPMI_REG_CHGR_FCC, raw).ok()?;
    su.read_u8(SPMI_SID_USBIN, SPMI_REG_CHGR_FCC).ok()
}

/// Number of characters in the path (prefix + 16 digits).

/// Encodes a SUPERUSER address: `(sid << 16) | reg`.
#[must_use]
pub const fn superuser_addr_enc(sid: u8, reg: u16) -> u32 {
    ((sid as u32) << 16) | (reg as u32)
}

/// `\Device\Spmi\SUPERUSER` session: one open -> grant -> R/W -> close.
///
/// The SUPERUSER slot is limited to three concurrent opens. We hold the handle only
/// for the duration of the negotiate and close it in [`Drop`], so as not to block
/// `qcpmic` / `qcpmicext` / `qcpmgpio`.
#[derive(Debug)]
pub struct SuperuserBus {
    target: WDFIOTARGET,
}

impl SuperuserBus {
    /// Opens `\Device\Spmi\SUPERUSER` (share-all, R/W).
    ///
    /// # Errors
    ///
    /// Returns the raw `NTSTATUS` of the open / target creation.
    ///
    /// # Safety
    ///
    /// Passive IRQL; `device` is live.
    pub unsafe fn open(device: WDFDEVICE) -> Result<Self, i32> {
        let mut chars = [0_u16; PROBE_NAME_CHARS];
        let name = "\\Device\\Spmi\\SUPERUSER";
        let mut length = 0_usize;
        for byte in name.bytes() {
            if let Some(slot) = chars.get_mut(length) {
                *slot = u16::from(byte);
                length = length.saturating_add(1);
            }
        }
        let mut io_target: WDFIOTARGET = WDF_NO_HANDLE.cast();
        // SAFETY: the device is created; the handle is local.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfIoTargetCreate,
                device,
                WDF_NO_OBJECT_ATTRIBUTES,
                &raw mut io_target,
            )
        };
        if !nt_ok(status) {
            return Err(status);
        }
        let bytes = u16::try_from(length.saturating_mul(2)).unwrap_or(0);
        let mut params: WDF_IO_TARGET_OPEN_PARAMS = unsafe { core::mem::zeroed() };
        params.Size = size_of_ulong::<WDF_IO_TARGET_OPEN_PARAMS>();
        params.Type = WdfIoTargetOpenByName;
        params.TargetDeviceName = UNICODE_STRING {
            Length: bytes,
            MaximumLength: bytes,
            Buffer: chars.as_mut_ptr(),
        };
        params.DesiredAccess = 0xC000_0000;
        params.ShareAccess = PROBE_SHARE_ALL;
        params.CreateDisposition = wdk_sys::FILE_OPEN;
        params.FileAttributes = wdk_sys::FILE_ATTRIBUTE_NORMAL;
        params.CreateOptions = 0x0000_0040;
        // SAFETY: the target is created, passive level.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(WdfIoTargetOpen, io_target, &raw mut params)
        };
        if !nt_ok(status) {
            unsafe {
                call_unsafe_wdf_function_binding!(WdfObjectDelete, io_target.cast());
            }
            return Err(status);
        }
        Ok(Self { target: io_target })
    }

    /// Peri-grant: `u16 count` + `count × u16` peri ids.
    ///
    /// # Errors
    ///
    /// Raw `NTSTATUS` of the grant IOCTL.
    pub fn grant(&mut self, peri: u16) -> Result<(), i32> {
        let mut grant = [0_u8; 4];
        grant[0] = 1;
        grant[1] = 0;
        grant[2] = (peri & 0xFF) as u8;
        grant[3] = ((peri >> 8) & 0xFF) as u8;
        self.ioctl_in(IOCTL_SPMI_SUPERUSER_GRANT, &grant)
    }

    /// Reads one byte: SID + register (`addr_enc = (sid<<16)|reg`).
    ///
    /// # Errors
    ///
    /// Raw `NTSTATUS` of the read IOCTL.
    pub fn read_u8(&mut self, sid: u8, reg: u16) -> Result<u8, i32> {
        let mut buf = [0_u8; 1];
        self.read_bytes(sid, reg, &mut buf)?;
        Ok(buf[0])
    }

    /// Writes one byte.
    ///
    /// # Errors
    ///
    /// Raw `NTSTATUS` of the write IOCTL.
    pub fn write_u8(&mut self, sid: u8, reg: u16, value: u8) -> Result<(), i32> {
        self.write_bytes(sid, reg, &[value])
    }

    /// Reads `out.len()` bytes starting at `reg`.
    ///
    /// # Errors
    ///
    /// Raw `NTSTATUS`, or `STATUS_INVALID_PARAMETER` if the length is 0 / >255.
    pub fn read_bytes(&mut self, sid: u8, reg: u16, out: &mut [u8]) -> Result<(), i32> {
        let len = out.len();
        if len == 0 || len > 255 {
            return Err(wdk_sys::STATUS_INVALID_PARAMETER);
        }
        let mut header = [0_u8; SPMI_SUPERUSER_HEADER_LEN];
        let enc = superuser_addr_enc(sid, reg);
        header[4] = (enc & 0xFF) as u8;
        header[5] = ((enc >> 8) & 0xFF) as u8;
        header[6] = ((enc >> 16) & 0xFF) as u8;
        header[7] = ((enc >> 24) & 0xFF) as u8;
        header[8] = len as u8;
        self.ioctl_in_out(IOCTL_SPMI_SUPERUSER_READ, &header, out)
    }

    /// Writes the payload starting at `reg`.
    ///
    /// # Errors
    ///
    /// Raw `NTSTATUS`, or `STATUS_INVALID_PARAMETER` if the length is 0 / >255.
    pub fn write_bytes(&mut self, sid: u8, reg: u16, data: &[u8]) -> Result<(), i32> {
        let len = data.len();
        if len == 0 || len > 255 {
            return Err(wdk_sys::STATUS_INVALID_PARAMETER);
        }
        let mut buf = [0_u8; SPMI_SUPERUSER_HEADER_LEN.saturating_add(255)];
        let enc = superuser_addr_enc(sid, reg);
        buf[4] = (enc & 0xFF) as u8;
        buf[5] = ((enc >> 8) & 0xFF) as u8;
        buf[6] = ((enc >> 16) & 0xFF) as u8;
        buf[7] = ((enc >> 24) & 0xFF) as u8;
        buf[8] = len as u8;
        let total = SPMI_SUPERUSER_HEADER_LEN.saturating_add(len);
        if let Some(dst) = buf.get_mut(SPMI_SUPERUSER_HEADER_LEN..total) {
            dst.copy_from_slice(data);
        }
        self.ioctl_in(IOCTL_SPMI_SUPERUSER_WRITE, &buf[..total])
    }

    fn ioctl_in(&mut self, ioctl: u32, input: &[u8]) -> Result<(), i32> {
        let mut empty = [];
        self.ioctl_in_out(ioctl, input, &mut empty)
    }

    fn ioctl_in_out(&mut self, ioctl: u32, input: &[u8], output: &mut [u8]) -> Result<(), i32> {
        let mut request: WDFREQUEST = WDF_NO_HANDLE.cast();
        let mut in_mem: WDFMEMORY = WDF_NO_HANDLE.cast();
        let mut out_mem: WDFMEMORY = WDF_NO_HANDLE.cast();
        let mut in_ptr: *mut core::ffi::c_void = core::ptr::null_mut();
        let mut out_ptr: *mut core::ffi::c_void = core::ptr::null_mut();

        // SAFETY: the target is open; passive level.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfRequestCreate,
                WDF_NO_OBJECT_ATTRIBUTES,
                self.target,
                &raw mut request,
            )
        };
        if !nt_ok(status) {
            return Err(status);
        }

        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfMemoryCreate,
                WDF_NO_OBJECT_ATTRIBUTES,
                wdk_sys::_POOL_TYPE::NonPagedPool,
                0,
                input.len(),
                &raw mut in_mem,
                &raw mut in_ptr,
            )
        };
        if !nt_ok(status) {
            unsafe {
                call_unsafe_wdf_function_binding!(WdfObjectDelete, request.cast());
            }
            return Err(status);
        }
        unsafe {
            core::ptr::copy_nonoverlapping(input.as_ptr(), in_ptr.cast::<u8>(), input.len());
        }

        let out_handle = if output.is_empty() {
            WDF_NO_HANDLE.cast()
        } else {
            let status = unsafe {
                call_unsafe_wdf_function_binding!(
                    WdfMemoryCreate,
                    WDF_NO_OBJECT_ATTRIBUTES,
                    wdk_sys::_POOL_TYPE::NonPagedPool,
                    0,
                    output.len(),
                    &raw mut out_mem,
                    &raw mut out_ptr,
                )
            };
            if !nt_ok(status) {
                unsafe {
                    call_unsafe_wdf_function_binding!(WdfObjectDelete, in_mem.cast());
                    call_unsafe_wdf_function_binding!(WdfObjectDelete, request.cast());
                }
                return Err(status);
            }
            unsafe {
                core::ptr::write_bytes(out_ptr.cast::<u8>(), 0xFF, output.len());
            }
            out_mem
        };

        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfIoTargetFormatRequestForIoctl,
                self.target,
                request,
                ioctl,
                in_mem,
                core::ptr::null_mut(),
                out_handle,
                core::ptr::null_mut(),
            )
        };
        let result = if nt_ok(status) {
            let mut options: WDF_REQUEST_SEND_OPTIONS = unsafe { core::mem::zeroed() };
            options.Size = size_of_ulong::<WDF_REQUEST_SEND_OPTIONS>();
            options.Flags =
                (WDF_REQUEST_SEND_OPTION_SYNCHRONOUS | WDF_REQUEST_SEND_OPTION_TIMEOUT) as ULONG;
            options.Timeout = SPB_TIMEOUT_100NS;
            let sent = unsafe {
                call_unsafe_wdf_function_binding!(
                    WdfRequestSend,
                    request,
                    self.target,
                    &raw mut options,
                )
            };
            if sent == 0 {
                Err(-1)
            } else {
                let st =
                    unsafe { call_unsafe_wdf_function_binding!(WdfRequestGetStatus, request) };
                if nt_ok(st) {
                    if !output.is_empty() && !out_ptr.is_null() {
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                out_ptr.cast::<u8>(),
                                output.as_mut_ptr(),
                                output.len(),
                            );
                        }
                    }
                    Ok(())
                } else {
                    Err(st)
                }
            }
        } else {
            Err(status)
        };

        unsafe {
            if !output.is_empty() {
                call_unsafe_wdf_function_binding!(WdfObjectDelete, out_mem.cast());
            }
            call_unsafe_wdf_function_binding!(WdfObjectDelete, in_mem.cast());
            call_unsafe_wdf_function_binding!(WdfObjectDelete, request.cast());
        }
        result
    }
}

impl Drop for SuperuserBus {
    fn drop(&mut self) {
        if self.target.is_null() {
            return;
        }
        // SAFETY: the target was created in `open`; called at passive level (negotiate).
        unsafe {
            call_unsafe_wdf_function_binding!(WdfIoTargetClose, self.target);
            call_unsafe_wdf_function_binding!(WdfObjectDelete, self.target.cast());
        }
        self.target = WDF_NO_HANDLE.cast();
    }
}

/// Number of characters in the path (prefix + 16 digits).
pub const HUB_PATH_CHARS: usize = RESOURCE_HUB_PREFIX.len() + 16;

/// Path to the Resource Hub node in UTF-16.
#[derive(Debug, Clone, Copy)]
pub struct HubPath {
    chars: [u16; HUB_PATH_CHARS],
}

impl HubPath {
    /// The path as a `UNICODE_STRING` (without a terminating null).
    #[must_use]
    /// Resource Hub node path string: kept for diagnostics.
    #[allow(dead_code)]
    pub fn as_unicode_string(&self) -> UNICODE_STRING {
        let length = u16::try_from(HUB_PATH_CHARS.saturating_mul(2)).unwrap_or(0);
        UNICODE_STRING {
            Length: length,
            MaximumLength: length,
            Buffer: self.chars.as_ptr().cast_mut(),
        }
    }

    /// The path as ASCII bytes (for the journal and tests).
    #[must_use]
    pub fn as_ascii(&self) -> [u8; HUB_PATH_CHARS] {
        let mut out = [0_u8; HUB_PATH_CHARS];
        for (index, symbol) in self.chars.iter().enumerate() {
            if let Some(slot) = out.get_mut(index) {
                *slot = u8::try_from(*symbol).unwrap_or(b'?');
            }
        }
        out
    }
}
