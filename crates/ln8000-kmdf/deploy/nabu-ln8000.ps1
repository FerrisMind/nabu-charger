# nabu-ln8000.ps1 - LN8000 charge pump driver diagnostics
#
# Opens the \\.\nabu_ln8000 device (a symbolic link created by the driver) and
# talks to it through DeviceIoControl. The control codes are computed by the same
# formula as in crates/ln8000-kmdf/src/ioctl.rs:
#     CTL_CODE(0x22, function, METHOD_BUFFERED, FILE_ANY_ACCESS)
#
# Commands:
#     .\nabu-ln8000.ps1 status            - mode, faults, telemetry, counters
#     .\nabu-ln8000.ps1 sessions          - charge sessions (current and last)
#     .\nabu-ln8000.ps1 read <reg>        - read an LN8000 register (hex)
#     .\nabu-ln8000.ps1 write <reg> <val> - write a register (hex), diagnostics
#     .\nabu-ln8000.ps1 journal <file>    - export the session journal to a JSONL file
#     .\nabu-ln8000.ps1 limits <mA> [mV]  - set limits: current in mA, voltage in mV
#     .\nabu-ln8000.ps1 mode <1|2|3>      - mode: 1 standby, 2 bypass, 3 switching

[CmdletBinding()]
param(
  [Parameter(Position = 0)][string]$Command = 'status',
  [Parameter(Position = 1)][string]$Arg1,
  [Parameter(Position = 2)][string]$Arg2,
  [string]$DevicePath = '\\.\nabu_ln8000'
)

$ErrorActionPreference = 'Stop'

# --- driver contract (see crates/ln8000-kmdf/src/ioctl.rs) ---

function Get-CtlCode {
  param([int]$Function, [int]$Method = 0, [int]$Access = 0)
  return ((0x22 -shl 16) -bor ($Access -shl 14) -bor ($Function -shl 2) -bor $Method)
}

$IOCTL = @{
  GET_STATUS   = Get-CtlCode 0x810
  READ_REG     = Get-CtlCode 0x811
  WRITE_REG    = Get-CtlCode 0x812
  SET_LIMITS   = Get-CtlCode 0x813
  SET_MODE     = Get-CtlCode 0x814
  GET_SESSIONS = Get-CtlCode 0x815
  GET_SAMPLES  = Get-CtlCode 0x816
}

$STATUS_MAGIC = 0x4C4E3830  # "LN80"

# --- native API P/Invoke ---

if (-not ('NabuNative' -as [type])) {
  Add-Type -Namespace '' -Name 'NabuNative' -MemberDefinition @'
[DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
public static extern IntPtr CreateFileW(string name, uint access, uint share, IntPtr security,
                                        uint disposition, uint flags, IntPtr template);
[DllImport("kernel32.dll", SetLastError = true)]
public static extern bool DeviceIoControl(IntPtr handle, uint code, byte[] input, uint inputSize,
                                          byte[] output, uint outputSize, out uint returned, IntPtr overlapped);
[DllImport("kernel32.dll", SetLastError = true)]
public static extern bool CloseHandle(IntPtr handle);
'@
}

function Open-Device {
  # 0xC0000000 = GENERIC_READ | GENERIC_WRITE, 3 = OPEN_EXISTING, share = READ|WRITE.
  # The values are passed as UInt32: otherwise PowerShell passes a negative Int32
  # and overload resolution fails.
  $desiredAccess = [uint32]3221225472
  $handle = [NabuNative]::CreateFileW($DevicePath, $desiredAccess, [uint32]3, [IntPtr]::Zero,
                                      [uint32]3, [uint32]0, [IntPtr]::Zero)
  if ($handle -eq [IntPtr]::new(-1)) {
    throw ("failed to open $DevicePath (code " + [Runtime.InteropServices.Marshal]::GetLastWin32Error() +
           "). Is the driver installed and the PEIC device available?")
  }
  return $handle
}

# The buffer parameter must not be named `Input`: PowerShell has an automatic
# variable `$Input` (the enumerator of the incoming pipeline), and binding
# `-Input <byte[]>` fails with "cannot convert ...ArrayListEnumeratorSimple... to
# System.Byte[]" before the body ever runs. Measured on the tablet: every command
# that sends a buffer died on this line.
function Invoke-DeviceIoControl {
  param([IntPtr]$Handle, [uint32]$Code, [byte[]]$Buffer, [uint32]$OutputSize)
  $output = New-Object byte[] $OutputSize
  $returned = 0
  $ok = [NabuNative]::DeviceIoControl($Handle, $Code, $Buffer, [uint32]($Buffer.Length), $output, $OutputSize, [ref]$returned, [IntPtr]::Zero)
  return [pscustomobject]@{ Ok = $ok; Bytes = $output; Returned = $returned; Error = [Runtime.InteropServices.Marshal]::GetLastWin32Error() }
}

function Read-Struct {
  param([byte[]]$Bytes, [Type]$Type)
  # `Marshal::SizeOf($Type)` cannot be used from PowerShell: the binder picks the
  # `SizeOf(object)` overload and then fails to marshal the RuntimeType itself
  # ("cannot marshal System.RuntimeType as an unmanaged structure"). An instance of
  # the struct takes that same overload with a real value and returns the size of
  # the struct. Measured on the tablet: `status` died here.
  $size = [Runtime.InteropServices.Marshal]::SizeOf([Activator]::CreateInstance($Type))
  $buffer = New-Object byte[] $size
  [Array]::Copy($Bytes, $buffer, [Math]::Min($Bytes.Length, $size))
  $handle = [Runtime.InteropServices.GCHandle]::Alloc($buffer, 'Pinned')
  try {
    # `PtrToStructure($ptr, $Type)` has the same problem as `SizeOf($Type)`: the
    # binder prefers the `object` overload and tries to marshal the RuntimeType
    # itself. Only the generic overload does the right thing, and it has to be
    # reached by reflection because PowerShell cannot pick `PtrToStructure[T]`.
    $method = [Runtime.InteropServices.Marshal].GetMethods() |
      Where-Object { $_.Name -eq 'PtrToStructure' -and $_.IsGenericMethod -and $_.GetParameters().Count -eq 1 } |
      Select-Object -First 1
    return $method.MakeGenericMethod($Type).Invoke($null, @($handle.AddrOfPinnedObject()))
  } finally {
    $handle.Free()
  }
}

# --- response structures (field order matches Rust) ---

if (-not ('Ln8000Status' -as [type])) {
  Add-Type -TypeDefinition @'
using System.Runtime.InteropServices;
[StructLayout(LayoutKind.Sequential)]
public struct Ln8000Status {
    public uint Magic; public ushort Version; public byte OpMode; public byte State;
    public byte SysSts; public byte Fault1Sts; public byte Fault2Sts; public byte SafetySts;
    public byte CriticalFault; public byte Reserved0; public byte Reserved1; public byte Reserved2;
    public uint IinUa; public uint VbatUv; public uint VbusUv; public int DieTempDc;
    public ulong Sessions; public ulong Samples; public uint Writes; public uint Reads;
    public int LastError;
}
[StructLayout(LayoutKind.Sequential)]
public struct Ln8000Sessions {
    public ulong Total; public ulong CurrentMs; public uint CurrentPeakIinUa;
    public byte CurrentFast; public byte R0; public byte R1; public byte R2;
    public ulong LastMs; public uint LastPeakIinUa; public int LastPeakTempDc;
    public byte LastFast; public byte S0; public byte S1; public byte S2;
}
[StructLayout(LayoutKind.Sequential)]
public struct Ln8000Reg {
    public byte Addr; public byte Value; public byte R0; public byte R1; public int ErrorCode;
}
'@
}

function Invoke-Status {
  $handle = Open-Device
  try {
    $result = Invoke-DeviceIoControl -Handle $handle -Code $IOCTL.GET_STATUS -Buffer (New-Object byte[] 0) -OutputSize 64
    if (-not $result.Ok) { throw ("DeviceIoControl GET_STATUS: error " + $result.Error) }
    $status = Read-Struct $result.Bytes ([Ln8000Status])
    if ($status.Magic -ne $STATUS_MAGIC) {
      Write-Host ("  WARNING: unknown magic 0x{0:X8}" -f $status.Magic) -ForegroundColor Yellow
    }
    $modeName = switch ($status.OpMode) { 0 {'UNKNOWN'} 1 {'STANDBY'} 2 {'BYPASS 1:1'} 3 {'SWITCHING 2:1'} default {'?'} }
    $stateName = switch ($status.State) { 0 {'closed'} 1 {'identified'} 2 {'configured'} 3 {'switching'} 4 {'FAULT'} default {'?'} }
    Write-Host '=== LN8000: state ===' -ForegroundColor Cyan
    Write-Host ("  mode             : " + $modeName + " (" + $status.OpMode + ")")
    Write-Host ("  session state    : " + $stateName)
    Write-Host ("  SYS_STS          : 0x{0:X2}" -f $status.SysSts)
    Write-Host ("  FAULT1/FAULT2    : 0x{0:X2} / 0x{1:X2}" -f $status.Fault1Sts, $status.Fault2Sts)
    Write-Host ("  SAFETY_STS       : 0x{0:X2}" -f $status.SafetySts)
    Write-Host ("  critical fault   : " + $(if ($status.CriticalFault -ne 0) { 'YES' } else { 'no' })) `
      -ForegroundColor $(if ($status.CriticalFault -ne 0) { 'Red' } else { 'Gray' })
    Write-Host ("  input current    : {0} µA ({1:N2} A)" -f $status.IinUa, ($status.IinUa / 1e6))
    Write-Host ("  battery voltage  : {0} µV ({1:N3} V)" -f $status.VbatUv, ($status.VbatUv / 1e6))
    Write-Host ("  input voltage    : {0} µV ({1:N3} V)" -f $status.VbusUv, ($status.VbusUv / 1e6))
    Write-Host ("  die temperature  : {0} (0.1 °C)" -f $status.DieTempDc)
    Write-Host ("  sessions/samples : {0} / {1}" -f $status.Sessions, $status.Samples)
    Write-Host ("  writes/reads     : {0} / {1}" -f $status.Writes, $status.Reads)
    if ($status.LastError -ne 0) {
      Write-Host ("  last error       : " + $status.LastError) -ForegroundColor Yellow
    }
    return $status
  } finally {
    [NabuNative]::CloseHandle($handle) | Out-Null
  }
}

function Invoke-Sessions {
  $handle = Open-Device
  try {
    $result = Invoke-DeviceIoControl -Handle $handle -Code $IOCTL.GET_SESSIONS -Buffer (New-Object byte[] 0) -OutputSize 48
    if (-not $result.Ok) { throw ("DeviceIoControl GET_SESSIONS: error " + $result.Error) }
    $s = Read-Struct $result.Bytes ([Ln8000Sessions])
    Write-Host '=== LN8000: charge sessions ===' -ForegroundColor Cyan
    Write-Host ("  total sessions   : " + $s.Total)
    if ($s.CurrentMs -gt 0) {
      Write-Host ("  current          : {0:N1} s, peak {1:N2} A, fast mode: {2}" -f ($s.CurrentMs / 1000), ($s.CurrentPeakIinUa / 1e6), $(if ($s.CurrentFast -ne 0) { 'yes' } else { 'no' }))
    } else {
      Write-Host '  current          : no power'
    }
    if ($s.LastMs -gt 0) {
      Write-Host ("  last             : {0:N1} s, peak {1:N2} A, peak temp {2} (0.1 °C), fast mode: {3}" -f ($s.LastMs / 1000), ($s.LastPeakIinUa / 1e6), $s.LastPeakTempDc, $(if ($s.LastFast -ne 0) { 'yes' } else { 'no' }))
    }
    return $s
  } finally {
    [NabuNative]::CloseHandle($handle) | Out-Null
  }
}

function Invoke-ReadReg {
  param([int]$Address)
  $handle = Open-Device
  try {
    $input = New-Object byte[] 8
    $input[0] = [byte]$Address
    $result = Invoke-DeviceIoControl -Handle $handle -Code $IOCTL.READ_REG -Buffer $input -OutputSize 8
    if (-not $result.Ok) { throw ("DeviceIoControl READ_REG: error " + $result.Error) }
    $reg = Read-Struct $result.Bytes ([Ln8000Reg])
    if ($reg.ErrorCode -ne 0) { throw ("the driver returned error code " + $reg.ErrorCode) }
    Write-Host ("  register 0x{0:X2} = 0x{1:X2}" -f $reg.Addr, $reg.Value)
    return $reg.Value
  } finally {
    [NabuNative]::CloseHandle($handle) | Out-Null
  }
}

function Invoke-WriteReg {
  param([int]$Address, [int]$Value)
  $handle = Open-Device
  try {
    $input = New-Object byte[] 8
    $input[0] = [byte]$Address
    $input[1] = [byte]$Value
    $result = Invoke-DeviceIoControl -Handle $handle -Code $IOCTL.WRITE_REG -Buffer $input -OutputSize 8
    if (-not $result.Ok) { throw ("DeviceIoControl WRITE_REG: error " + $result.Error) }
    Write-Host ("  wrote 0x{0:X2} to 0x{1:X2}" -f $Value, $Address)
  } finally {
    [NabuNative]::CloseHandle($handle) | Out-Null
  }
}

function Invoke-Journal {
  param([string]$Path, [int]$Pd = 0)
  Write-Host '=== session journal ===' -ForegroundColor Cyan
  $status = Invoke-Status
  $sessions = Invoke-Sessions

  # The pump cannot know the state of charge or the power state - the OS provides them.
  $battery = Get-CimInstance Win32_Battery -ErrorAction SilentlyContinue | Select-Object -First 1
  $soc = -1
  $batteryStatus = 0
  if ($battery -and $null -ne $battery.EstimatedChargeRemaining) {
    $soc = [int]$battery.EstimatedChargeRemaining
  }
  if ($battery -and $null -ne $battery.BatteryStatus) {
    $batteryStatus = [int]$battery.BatteryStatus
  }
  $pdLabel = switch ($Pd) {
    0 { 'unknown' }
    1 { '5 V / regular brick' }
    2 { 'raised voltage 9 V and above' }
    3 { 'QC negotiated' }
    default { '?' }
  }

  $record = [ordered]@{
    exported_at     = (Get-Date).ToString('o')
    host            = $env:COMPUTERNAME
    soc_percent     = $soc
    battery_status  = $batteryStatus
    pd_status       = $Pd
    pd_status_label = $pdLabel
    mode            = $status.OpMode
    state           = $status.State
    sys_sts         = $status.SysSts
    fault1_sts      = $status.Fault1Sts
    fault2_sts      = $status.Fault2Sts
    safety_sts      = $status.SafetySts
    critical        = $status.CriticalFault
    iin_ua          = $status.IinUa
    vbat_uv         = $status.VbatUv
    vbus_uv         = $status.VbusUv
    die_temp_dc     = $status.DieTempDc
    sessions        = $sessions.Total
    samples         = $status.Samples
  }
  $json = $record | ConvertTo-Json -Compress
  Add-Content -LiteralPath $Path -Value $json -Encoding UTF8
  Write-Host ("  written to " + $Path) -ForegroundColor Green
  Write-Host ("  charge by OS data: " + $soc + " %; negotiation: " + $pdLabel)
}

switch ($Command.ToLower()) {
  'status' { Invoke-Status | Out-Null }
  'sessions' { Invoke-Sessions | Out-Null }
  'read' {
    if (-not $Arg1) { throw 'specify the register address: read 1E' }
    Invoke-ReadReg ([Convert]::ToInt32($Arg1, 16)) | Out-Null
  }
  'write' {
    if (-not $Arg1 -or -not $Arg2) { throw 'specify the address and the value: write 1E 00' }
    Invoke-WriteReg ([Convert]::ToInt32($Arg1, 16)) ([Convert]::ToInt32($Arg2, 16))
  }
  'journal' {
    if (-not $Arg1) { throw 'specify the file: journal out.jsonl' }
    $pdCode = 0
    if ($Arg2) { $pdCode = [int]$Arg2 }
    Invoke-Journal $Arg1 $pdCode
  }
  default {
    Write-Host 'Commands: status | sessions | read <hex> | write <hex> <hex> | journal <file>'
    Write-Host ("Control codes: GET_STATUS=0x{0:X6} READ=0x{1:X6} WRITE=0x{2:X6} SESSIONS=0x{3:X6}" -f `
      $IOCTL.GET_STATUS, $IOCTL.READ_REG, $IOCTL.WRITE_REG, $IOCTL.GET_SESSIONS)
  }
}
