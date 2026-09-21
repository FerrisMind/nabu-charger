# PMIC register access: what reverse engineering confirmed

The question that hung in the air the longest: with which protocol the stock Qualcomm
drivers read the PMIC registers, and how to repeat it from our driver. Below are the method,
the findings with addresses and the conclusion - verifiable: scripts and dumps are here.

## Method

1. **WDK headers.** `shared/reshub.h` (WDK 10.0.26100) was checked: it has
   only the path macros (`RESOURCE_HUB_DEVICE_NAME`,
   `RESOURCE_HUB_CREATE_PATH_FROM_ID`) and the connection parameter structures
   (`RH_I2C_CONNECTION_PARAMETERS` and others). The transaction structures and the
   `IOCTL_RESOURCE_HUB_TRANSACT` code are **not** in the public headers.
2. **Reverse engineering of the binaries** (Ghidra 12.1.3 headless, project `nabu`):
   * `FindTransact.java` - search for the constants `0x32C004`, `0xC004`, `0x8B012`
     in operands and decompilation of the functions found;
   * `DecompAt.java` - pinpoint decompilation by address.

## Finding 1: the server side (`qcspmi8150.sys`)

The function `FUN_140003270` (the only hit on `0x32C004` in the hub server)
itself **sends** this code to the lower bus device:

```c
iVar2 = (*pcVar11)(DAT_14000a368, uVar6, 0, 0x32c004, &uStack_d0, &local_e8, 0, auStack_b8);
```

It then checks the received buffer:

```c
if ((((int)local_b0 != 0x426f6541) || ((int)local_a8 == 0) ||
    ((local_a8._4_2_ != 2) || (local_a8._6_2_ < 0x1a)))) { ... }
```

and for a resource of type 3 (connection) it maps the controller registers into memory:

```c
uVar6 = MmMapIoSpaceEx(*(undefined8 *)(pcVar8 + 4), iVar4, 0x204);
```

That is, `0x32C004` is the **connection step** (obtain and map the controller), not
access to an individual register.

## Finding 2: the client side (`qcpmicEIC8150.sys`)

The function `FUN_1400026f0` (the client) prepares a connection request:

```c
piVar7 = ExAllocatePoolWithTag(0x200, 0x400, 0x31337071);   // 1024 bytes
memcpy_s((void *)((longlong)&local_98 + 4), 4, &DAT_140007178, 4);
local_98 = CONCAT44(local_98._4_4_, 0x42696541);            // low 4 bytes - signature
local_60 = &local_98;  local_58 = 8;                        // input: 8 bytes
local_70 = 0x400;                                           // output: 1024 bytes
iVar3 = (*pcVar11)(DAT_140008608, uVar13, 0, 0x32c004, &local_68, &local_80, 0, &local_88);
```

The received 1024 bytes are parsed into a device context (`FUN_140001618`), after
which **a table of register access functions is installed**, different for controller
versions (`piVar6[0x32] == 1` or `2`) - in the dump two sets of 18
pointers are visible.

## Finding 3: register access is the public SPB interface

The access primitives are `FUN_140003a40` (write) and `FUN_140003778` (read). Inside
`FUN_140003a40`:

```c
iVar2 = (*pcVar4)(DAT_140008608, uVar1, local_128, 0x41808, &local_118, 0,
                  &local_a8, auStack_120);
```

`0x41808` is

```text
CTL_CODE(FILE_DEVICE_CONTROLLER = 0x04, 0x602, METHOD_BUFFERED = 0, FILE_ANY_ACCESS = 0)
= 0x04 << 16 | 0x602 << 2
= 0x0004_1808 = IOCTL_SPB_EXECUTE_SEQUENCE
```

**Conclusion: there is no private register access protocol.** The PMIC registers are
read and written with an ordinary SPB transfer list (`SPB_TRANSFER_LIST`) - the same
public interface that our LN8000 driver uses on the I²C bus. That is exactly why the
search for a "response layout with the register value" yielded nothing: no such
response exists.

For illustration: the function `FUN_140004590` (version 1 initialization) writes `0x40`
to register `0x30`, reads register `0x32`, clears bit 2 in it and writes it back -
a typical controller initialization sequence.

## What this changes in our code

| Before | After |
|---|---|
| The SMB driver sent the hub a guess: an "operation kind" byte, two address bytes, the value | The driver builds a real `SPB_TRANSFER_LIST` and sends `IOCTL_SPB_EXECUTE_SEQUENCE` |
| The SPB ABI was copied inside the LN8000 driver and verified nowhere | A shared crate `crates/spb`: the ABI, the sequence building and the Resource Hub path are covered by host tests |
| A register read returned the failure "layout not confirmed" | The read is implemented as the sequence "address (16 bits), then a byte" |

The `SpmiConfig::address_big_endian` parameter sets the address byte order (by default
the high byte first, per the SPMI specification) and **requires confirmation on
hardware**: it is the only assumption left in the transport, and it is
factored out into the configuration instead of being hard-coded.

## What is not done yet and why

* **The connection step (`0x32C004`) is not performed.** The stock client obtains the
  SPMI bus I/O target through it. Our driver opens the hub device by
  name and sends the sequence to it; on hardware this may not work, and
  then the request honestly returns a transport error.
* **The right solution is to take the SPMI connection from the `_CRS` of our own node**
  (the path is built by the `spb::resource_hub_path` function). That requires a
  resource preparation callback (`EvtDevicePrepareHardware`), which the driver does not
  have right now: the transport is opened directly in `EvtDeviceAdd`. That is separate
  work, and it requires a check on the device, because it depends on which ACPI node
  the driver ends up with.

## Side result: the tests caught a layout bug

The shared crate wrote the first transfer after the list header, whereas per `spb.h` it
lies **inside** the header (`Transfers[1]`), and `sizeof(SPB_TRANSFER_LIST)`
already includes it. The test `area_size_counts_first_entry_in_header` caught this.

In the built LN8000 driver the layout was correct (the first transfer is taken as
`transfers[0]`, the following ones from `header_size()`), so the fix was needed only
in the new crate: `area_size` now computes
`sizeof(list) + (count - 1) * sizeof(entry)`.

## Where the evidence is

| File | What is inside |
|---|---|
| `G:\nabu-tools\re-spmi-hub.txt` | decompilation of `FUN_140003270` of the hub server |
| `G:\nabu-tools\re-spmi-client.txt` | decompilation of `FUN_1400026f0` of the client with the connection request |
| `G:\nabu-tools\re-spmi-ops.txt` | the access function tables and their selection by version |
| `G:\nabu-tools\re-spmi-prim.txt` | the register read and write primitives with the `0x41808` code |
| `G:\nabu-tools\kmdf-build-2.txt` | the SMB driver build on the shared crate for ARM64 |
| `G:\nabu-tools\ghidra-scripts\FindTransact.java`, `DecompAt.java` | the scripts the dumps were produced with |
