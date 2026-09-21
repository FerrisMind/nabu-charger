//! SPB structures declared in the WDK header.
//!
//! `wdk-sys` does not generate these types (the `spb` feature is empty), so they
//! are declared here verbatim from `shared/spb.h` in the installed WDK 10.0.26100:
//!
//! ```c
//! typedef enum SPB_TRANSFER_DIRECTION {
//!     SpbTransferDirectionNone,          // 0
//!     SpbTransferDirectionFromDevice,    // 1 - read from device
//!     SpbTransferDirectionToDevice,      // 2 - write to device
//!     SpbTransferDirectionMax
//! } SPB_TRANSFER_DIRECTION;
//!
//! typedef enum SPB_TRANSFER_BUFFER_FORMAT {
//!     SpbTransferBufferFormatInvalid,        // 0
//!     SpbTransferBufferFormatSimple,         // 1
//!     ... List = 2, SimpleNonPaged = 3, Mdl = 4
//! } SPB_TRANSFER_BUFFER_FORMAT;
//!
//! typedef struct SPB_TRANSFER_BUFFER_LIST_ENTRY { PVOID Buffer; ULONG BufferCb; };
//!
//! typedef struct SPB_TRANSFER_BUFFER {
//!     SPB_TRANSFER_BUFFER_FORMAT Format;
//!     union { SPB_TRANSFER_BUFFER_LIST_ENTRY Simple;
//!             struct { PSPB_TRANSFER_BUFFER_LIST_ENTRY List; ULONG ListCe; } BufferList;
//!             PMDL Mdl; };
//! };
//!
//! typedef struct SPB_TRANSFER_LIST_ENTRY {
//!     SPB_TRANSFER_DIRECTION Direction; ULONG DelayInUs; SPB_TRANSFER_BUFFER Buffer;
//! };
//!
//! typedef struct SPB_TRANSFER_LIST {
//!     ULONG Size; ULONG Reserved; ULONG TransferCount;
//!     SPB_TRANSFER_LIST_ENTRY Transfers[1];
//! };
//! ```

use core::ffi::c_void;

/// Controller device type (`FILE_DEVICE_CONTROLLER` from `wdm.h`).
///
/// Given as part of the derivation of the SPB control code: to show where the
/// value of [`IOCTL_SPB_EXECUTE_SEQUENCE`] comes from.
#[allow(dead_code)]
pub const FILE_DEVICE_CONTROLLER: u32 = 0x0000_0004;

/// Control code that executes a sequence of transfers.
///
/// `CTL_CODE(FILE_DEVICE_CONTROLLER, 0x602, METHOD_BUFFERED, FILE_ANY_ACCESS)`
/// from `shared/spb.h`.
pub const IOCTL_SPB_EXECUTE_SEQUENCE: u32 = 0x0004_1808;

/// `CTL_CODE(FILE_DEVICE_CONTROLLER, 0x603, METHOD_BUFFERED, FILE_ANY_ACCESS)`.
pub const IOCTL_SPB_LOCK_CONNECTION: u32 = 0x0004_180C;

/// `CTL_CODE(FILE_DEVICE_CONTROLLER, 0x604, METHOD_BUFFERED, FILE_ANY_ACCESS)`.
#[allow(dead_code)]
pub const IOCTL_SPB_UNLOCK_CONNECTION: u32 = 0x0004_1810;

/// Transfer direction: read from the device.
/// `None` direction: terminating entry of the transfer list.
pub const SPB_DIRECTION_NONE: u32 = 0;

/// `CTL_CODE(FILE_DEVICE_CONTROLLER, 0x600, METHOD_BUFFERED, FILE_ANY_ACCESS)`.
pub const IOCTL_SPB_LOCK_CONTROLLER: u32 = 0x0004_1800;

/// `CTL_CODE(FILE_DEVICE_CONTROLLER, 0x601, METHOD_BUFFERED, FILE_ANY_ACCESS)`.
pub const IOCTL_SPB_UNLOCK_CONTROLLER: u32 = 0x0004_1804;

/// `CTL_CODE(FILE_DEVICE_CONTROLLER, 0x605, METHOD_BUFFERED, FILE_ANY_ACCESS)`.
pub const IOCTL_SPB_FULL_DUPLEX: u32 = 0x0004_1814;

/// `CTL_CODE(FILE_DEVICE_CONTROLLER, 0x606, METHOD_BUFFERED, FILE_ANY_ACCESS)`.
pub const IOCTL_SPB_MULTI_SPI_TRANSFER: u32 = 0x0004_1818;

/// Peripheral connection request: the reference Qualcomm client sends it to the
/// node before any register access (`0x32C004` from the `qcpmicEIC8150.sys` analysis).
pub const IOCTL_ATTACH: u32 = 0x0032_C004;

/// SPMI SUPERUSER: byte read (`CTL_CODE(0x85B5, 0x903, METHOD_BUFFERED, ANY)`).
pub const IOCTL_SPMI_SUPERUSER_READ: u32 = 0x85B5_240C;

/// SPMI SUPERUSER: byte write.
pub const IOCTL_SPMI_SUPERUSER_WRITE: u32 = 0x85B5_2410;

/// SPMI SUPERUSER: bit operation (length 1).
#[allow(dead_code)]
pub const IOCTL_SPMI_SUPERUSER_BITOP: u32 = 0x85B5_2414;

/// SPMI SUPERUSER: grant of a peripheral list (`u16 count` + `count × u16`).
pub const IOCTL_SPMI_SUPERUSER_GRANT: u32 = 0x85B5_2418;

/// R/W SUPERUSER header length: `{u32 flags, u32 addr_enc, u32 len}`.
pub const SPMI_SUPERUSER_HEADER_LEN: usize = 12;

/// Magic in the connection request input (`0x42696541`, that is `AeiB`).
pub const ATTACH_MAGIC: u32 = 0x4269_6541;

/// Length of the node reply to a connection request.
pub const ATTACH_REPLY_LEN: usize = 1024;
pub const SPB_DIRECTION_FROM_DEVICE: u32 = 1;/// Transfer direction: write to the device.
pub const SPB_DIRECTION_TO_DEVICE: u32 = 2;

/// Buffer format: simple buffer region.
pub const SPB_FORMAT_SIMPLE: u32 = 1;

/// "Buffer list" format: the buffer is described by an array of entries.
#[allow(dead_code)]
pub const SPB_FORMAT_LIST: u32 = 2;

/// MDL format: the buffer is described through an MDL.
#[allow(dead_code)]
pub const SPB_FORMAT_MDL: u32 = 4;

/// Buffer list entry.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SpbTransferBufferListEntry {
    /// Pointer to the data.
    pub buffer: *mut c_void,
    /// Data length in bytes.
    pub buffer_cb: u32,
}

/// Transfer buffer (the `Simple` variant is used).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SpbTransferBuffer {
    /// Buffer format.
    pub format: u32,
    /// Simple buffer region.
    pub simple: SpbTransferBufferListEntry,
}

/// One transfer in the sequence.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SpbTransferListEntry {
    /// Transfer direction.
    pub direction: u32,
    /// Delay before the transfer, us.
    pub delay_in_us: u32,
    /// Transfer buffer.
    pub buffer: SpbTransferBuffer,
}

/// Transfer list: header plus entries.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SpbTransferList {
    /// Structure size (`sizeof(SPB_TRANSFER_LIST)`).
    pub size: u32,
    /// Reserved, must be zero.
    pub reserved: u32,
    /// Number of transfers.
    pub transfer_count: u32,
    /// First entry (the remaining ones follow it in memory).
    pub transfers: [SpbTransferListEntry; 1],
}

impl SpbTransferList {
    /// `sizeof(SPB_TRANSFER_LIST)` is exactly the value that must sit in the
    /// `size` field (header together with ONE entry), however many transfers
    /// there are.
    #[must_use]
    pub const fn header_size() -> usize {
        core::mem::size_of::<Self>()
    }

    /// Size of one entry.
    #[must_use]
    pub const fn entry_size() -> usize {
        core::mem::size_of::<SpbTransferListEntry>()
    }

    /// Total size of the memory area for the list of `count` transfers:
    /// `sizeof(SPB_TRANSFER_LIST) + sizeof(entry) * (count - 1)`, exactly as
    /// `SPB_TRANSFER_LIST_AND_ENTRIES(count)` is defined in the WDK.
    #[must_use]
    pub const fn area_size(count: usize) -> usize {
        if count <= 1 {
            return Self::header_size();
        }
        Self::header_size().saturating_add(Self::entry_size().saturating_mul(count - 1))
    }
}

/// `SPB_TRANSFER_LIST` layout, checked by the compiler rather than by eye.
///
/// `sizeof(SPB_TRANSFER_LIST)` = 48 (header 16 + one entry 32),
/// `sizeof(SPB_TRANSFER_LIST_ENTRY)` = 32, the area for `n` transfers is
/// 48/80/112. Any discrepancy breaks the build.
const _: () = assert!(SpbTransferList::header_size() == 48);
const _: () = assert!(SpbTransferList::entry_size() == 32);
const _: () = assert!(SpbTransferList::area_size(1) == 48);
const _: () = assert!(SpbTransferList::area_size(2) == 80);
const _: () = assert!(SpbTransferList::area_size(3) == 112);

/// Initializes a "simple buffer" transfer list entry.
pub fn entry_init(
    direction: u32,
    buffer: *mut c_void,
    buffer_cb: u32,
) -> SpbTransferListEntry {
    SpbTransferListEntry {
        direction,
        delay_in_us: 0,
        buffer: SpbTransferBuffer {
            format: SPB_FORMAT_SIMPLE,
            simple: SpbTransferBufferListEntry { buffer, buffer_cb },
        },
    }
}
