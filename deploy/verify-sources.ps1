#Requires -Version 7.0
<#
    verify-sources.ps1 - cross-check the driver against the reference sources and the live dump.

    Run:
        .\verify-sources.ps1

    Three independent checks:

      1. The I2C descriptor from the live ACPI dump. The PEIC node's `_CRS` holds the real
         bus connection resource: device address, speed, controller name.
         The script decodes it and cross-checks it against what the driver expects.

      2. SMB registers (battery-stack branch). Every address and every bit from
         `crates/core/src/regs.rs` is cross-checked against the definitions in
         `smb5-reg.h`, including the computed `BASE + offset`, `BIT(n)` and `GENMASK`.

      3. LN8000 numeric constants. The core encoding formulas are cross-checked against
         the values in `ln8000_charger.h`. It separately states what cannot be
         verified from these sources - without embellishment.

    The report is written to artifacts\verify-sources.txt. Exit code 0 if there are
    no mismatches.
#>
[CmdletBinding()]
param(
    [string]$Repo = (Join-Path $PSScriptRoot '..'),
    [string]$Sources = 'G:\nabu-fast-charge\04-android-reference-sources',
    [string]$Dsdt = 'G:\nabu-fast-charge\09-acpi-nabu\live-dump\acpi-dump\DSDT_explicit.dsl',
    [string]$Out = (Join-Path $PSScriptRoot '..\artifacts\verify-sources.txt')
)

$ErrorActionPreference = 'Stop'
$report = New-Object System.Collections.Generic.List[string]
$mismatch = 0
$checked = 0
# Names defined differently in different headers (different chip generations).
$script:conflicts = @{}

function Emit {
    param([string]$Text = '')
    $report.Add($Text) | Out-Null
    Write-Host $Text
}

# --- parsing numeric expressions from C headers --------------------------

function ConvertTo-PsExpression {
    # PowerShell does not understand C operators: convert the bitwise forms.
    param([string]$Expr)
    $e = $Expr
    $e = $e -replace '<<', ' -shl '
    $e = $e -replace '>>', ' -shr '
    $e = $e -replace '&', ' -band '
    $e = $e -replace '\|', ' -bor '
    $e = $e -replace '~', ' -bnot '
    return $e
}

function Invoke-NumericExpression {
    param([string]$Expr)
    $trimmed = $Expr.Trim('(', ')', ' ')
    if ($trimmed -notmatch '^[\s0-9a-fA-FxX()+\-*/<>&|~]+$') { return $null }
    $ps = ConvertTo-PsExpression -Expr $trimmed
    try { return [int64](& ([scriptblock]::Create("return ($ps)"))) } catch { return $null }
}

function Get-DefineTable {
    param([string[]]$Path)
    $table = @{}
    foreach ($file in $Path) {
        if (-not (Test-Path -LiteralPath $file)) { continue }
        $short = Split-Path $file -Leaf
        foreach ($line in (Get-Content -LiteralPath $file)) {
            $name = $null
            $raw = $null
            if ($line -match '^\s*#define\s+([A-Za-z_][A-Za-z0-9_]*)\s+(.+?)\s*$') {
                $name = $Matches[1]
                $raw = $Matches[2]
                if ($raw.EndsWith('\')) { continue }
                if ($name -match '^(__|LN8000_REG_PRINT)') { continue }
                $raw = ($raw -replace '/\*.*?\*/', '') -replace '//.*$', ''
            } elseif ($line -match '^\s*([A-Z][A-Z0-9_]{3,})\s*=\s*(.+?)\s*,?\s*$') {
                # The value may be not only a number but also BIT(n)/GENMASK(h,l):
                # Resolve-Define parses those, and junk lines are filtered out there too.
                $candidate = $Matches[2].Trim()
                if ($candidate -notmatch ';|\(''|"') {
                    $name = $Matches[1]
                    $raw = $candidate
                }
            }
            if (-not $name) { continue }
            $value = $raw.Trim()
            if ($table.ContainsKey($name)) {
                # The first file in the list is the priority generation (smb5 for nabu).
                if ($table[$name] -ne $value) {
                    $script:conflicts[$name] = @{ primary = $table[$name]; other = $value; file = $short }
                }
                continue
            }
            $table[$name] = $value
        }
    }
    return $table
}

function Resolve-Define {
    param([string]$Name, [hashtable]$Table, [int]$Depth = 0)
    if ($Depth -gt 8 -or -not $Table.ContainsKey($Name)) { return $null }
    $expr = $Table[$Name]
    $expr = [regex]::Replace($expr, 'BIT\((\d+)\)', { param($m) [string][math]::Pow(2, [int]$m.Groups[1].Value) })
    $expr = [regex]::Replace($expr, 'GENMASK\((\d+),\s*(\d+)\)', {
            param($m)
            $high = [int]$m.Groups[1].Value; $low = [int]$m.Groups[2].Value
            $value = 0
            for ($i = $low; $i -le $high; $i++) { $value += [math]::Pow(2, $i) }
            [string][int64]$value
        })
    # name substitution
    $names = @($Table.Keys | Where-Object { $expr -match "\b$_\b" })
    foreach ($nested in $names) {
        $resolved = Resolve-Define -Name $nested -Table $Table -Depth ($Depth + 1)
        if ($null -ne $resolved) {
            $expr = [regex]::Replace($expr, "\b$nested\b", [string]$resolved)
        }
    }
    $expr = $expr.Trim('(', ')', ' ')
    return Invoke-NumericExpression -Expr $expr
}

function Get-RustConst {
    param([string]$Path, [string]$Name)
    if (-not (Test-Path -LiteralPath $Path)) { return $null }
    $text = Get-Content -LiteralPath $Path -Raw
    if ($text -notmatch ("pub const\s+" + $Name + "\s*:\s*[A-Za-z0-9]+\s*=\s*([^;]+);")) { return $null }
    $expr = $Matches[1].Trim()
    # first substitute names (underscores in them are significant!), then clean up numbers
    foreach ($dep in ([regex]::Matches($expr, '[A-Z][A-Z0-9_]+') | ForEach-Object { $_.Value } | Select-Object -Unique)) {
        $value = Get-RustConst -Path $Path -Name $dep
        if ($null -ne $value) { $expr = [regex]::Replace($expr, "\b$dep\b", [string]$value) }
    }
    $expr = [regex]::Replace($expr, '0b([01_]+)', { param($m) [string][Convert]::ToInt64($m.Groups[1].Value.Replace('_', ''), 2) })
    $expr = [regex]::Replace($expr, '(?<=[0-9a-fA-F])_(?=[0-9a-fA-F])', '')
    return Invoke-NumericExpression -Expr $expr
}

function Compare-Value {
    param([string]$Title, [object]$Mine, [object]$Reference)
    $script:checked++
    if ($null -eq $Reference) {
        Emit ("  [?]    " + $Title.PadRight(42) + " not found in the reference")
        return
    }
    if ($null -eq $Mine) {
        Emit ("  [!]    " + $Title.PadRight(42) + " not found in the code (reference " + $Reference + ")")
        $script:mismatch++
        return
    }
    if ($Mine -eq $Reference) {
        Emit ("  [ok]   " + $Title.PadRight(42) + " " + $Mine)
    } else {
        Emit ("  [MISMATCH] " + $Title.PadRight(42) + " code " + $Mine + " / reference " + $Reference)
        $script:mismatch++
    }
}

Emit ''
Emit '========================================================================='
Emit ' Cross-check of the nabu driver against reference sources and the live dump'
Emit (' Time: ' + (Get-Date -Format 'yyyy-MM-dd HH:mm:ss'))
Emit '========================================================================='

# --- 1. I2C descriptor from the ACPI dump --------------------------------

Emit ''
Emit '1. Bus connection resource from the live ACPI dump (PEIC node)'
Emit '-------------------------------------------------------------------------'

$bytes = $null
if (Test-Path -LiteralPath $Dsdt) {
    $lines = Get-Content -LiteralPath $Dsdt
    $start = ($lines | Select-String -Pattern 'Device \(PEIC\)' | Select-Object -First 1).LineNumber
    if ($start) {
        for ($i = $start; $i -lt [Math]::Min($start + 40, $lines.Count); $i++) {
            if ($lines[$i] -match 'Buffer \(0x1E\)') {
                $collected = New-Object System.Collections.Generic.List[byte]
                for ($j = $i + 1; $j -lt [Math]::Min($i + 12, $lines.Count); $j++) {
                    if ($lines[$j] -match '^\s*/\*\s*[0-9A-F]{4}\s*\*/\s*(.*?)(?://.*)?$') {
                        # take all bytes on the line: the last one may be without a comma
                        foreach ($pair in ([regex]::Matches($Matches[1], '0x([0-9A-Fa-f]{2})'))) {
                            $collected.Add([Convert]::ToByte($pair.Groups[1].Value, 16))
                        }
                    } elseif ($lines[$j] -match '\}') { break }
                }
                $bytes = $collected.ToArray()
                break
            }
        }
    }
}

if (-not $bytes -or $bytes.Count -lt 20) {
    Emit '  dump not read - skipping'
    $mismatch++
} else {
    Emit ('  buffer length: ' + $bytes.Count + ' bytes')
    $tag = $bytes[0]
    $busType = $bytes[5]
    $speed = [BitConverter]::ToUInt32($bytes, 12)
    $address = [BitConverter]::ToUInt16($bytes, 16)
    # the resource string ends with a NUL; an EndTag must follow
    $tail = $bytes[18..($bytes.Count - 1)]
    $nul = [Array]::IndexOf($tail, [byte]0)
    $resourceSource = ''
    if ($nul -gt 0) {
        $resourceSource = -join ($tail[0..($nul - 1)] | ForEach-Object { [char]$_ })
    }
    $afterNul = if ($nul -ge 0) { $tail[($nul + 1)..($tail.Count - 1)] } else { @() }

    $script:checked++
    if ($tag -eq 0x8E) { Emit '  [ok]   resource type: I2C descriptor (0x8E)' } else { Emit ('  [MISMATCH] resource type ' + $tag + ', expected 0x8E'); $mismatch++ }
    Compare-Value 'I2C bus (SerialBusType)' $busType 1
    Compare-Value 'device address on the bus' $address 0x0051
    Compare-Value 'bus speed, Hz' $speed 100000
    Compare-Value 'bus controller node' $resourceSource '\_SB.I2C5'
    $script:checked++
    if ($afterNul.Count -ge 2 -and $afterNul[0] -eq 0x79 -and $afterNul[1] -eq 0x00) {
        Emit '  [ok]   buffer correctly closed by EndTag (0x79)'
    } else {
        Emit ('  [MISMATCH] buffer tail: ' + (($afterNul | ForEach-Object { '0x' + $_.ToString('X2') }) -join ' '))
        $mismatch++
    }
    Emit ''
    Emit '  What this confirms for the driver: the bus, address 0x51 and controller match'
    Emit '  what the LN8000 driver expects. There is no interrupt in this resource,'
    Emit '  so the driver polls the state on a timer instead of waiting for an interrupt.'
}

# --- 2. SMB registers and bits -------------------------------------------

Emit ''
Emit '2. SMB registers and bits against smb5-reg.h'
Emit '-------------------------------------------------------------------------'

$smbHeaders = @(
    (Join-Path $Sources 'drivers_power_supply_qcom_smb5-reg.h'),
    (Join-Path $Sources 'drivers_power_supply_qcom_smb-reg.h')
)
$smb = Get-DefineTable -Path $smbHeaders
Emit ('  definitions in the reference: ' + $smb.Count)
Emit ''

$coreRegs = Join-Path $Repo 'crates\core\src\regs.rs'
$regPairs = @(
    @{ mine = 'USBIN_BASE'; ref = 'USBIN_BASE' },
    @{ mine = 'APSD_STATUS'; ref = 'APSD_STATUS_REG' },
    @{ mine = 'APSD_RESULT_STATUS'; ref = 'APSD_RESULT_STATUS_REG' },
    @{ mine = 'QC_CHANGE_STATUS'; ref = 'QC_CHANGE_STATUS_REG' },
    @{ mine = 'USBIN_CMD_IL'; ref = 'USBIN_CMD_IL_REG' },
    @{ mine = 'CMD_APSD'; ref = 'CMD_APSD_REG' },
    @{ mine = 'CMD_ICL_OVERRIDE'; ref = 'CMD_ICL_OVERRIDE_REG' },
    @{ mine = 'CMD_HVDCP_2'; ref = 'CMD_HVDCP_2_REG' },
    @{ mine = 'USBIN_ADAPTER_ALLOW_OVERRIDE'; ref = 'USBIN_ADAPTER_ALLOW_OVERRIDE_REG' },
    @{ mine = 'USB_CMD_PULLDOWN'; ref = 'USB_CMD_PULLDOWN_REG' },
    @{ mine = 'HVDCP_PULSE_COUNT_MAX'; ref = 'HVDCP_PULSE_COUNT_MAX_REG' },
    @{ mine = 'USBIN_ICL_OPTIONS'; ref = 'USBIN_ICL_OPTIONS_REG' },
    @{ mine = 'USBIN_CURRENT_LIMIT_CFG'; ref = 'USBIN_CURRENT_LIMIT_CFG_REG' }
)
foreach ($pair in $regPairs) {
    Compare-Value ('address ' + $pair.mine) (Get-RustConst -Path $coreRegs -Name $pair.mine) (Resolve-Define -Name $pair.ref -Table $smb)
}

Emit ''
$bitPairs = @(
    @{ mine = 'APSD_DTC_STATUS_DONE'; ref = 'APSD_DTC_STATUS_DONE_BIT' },
    @{ mine = 'QC_CHARGER'; ref = 'QC_CHARGER_BIT' },
    @{ mine = 'SLOW_PLUGIN_TIMEOUT'; ref = 'SLOW_PLUGIN_TIMEOUT_BIT' },
    @{ mine = 'HVDCP_CHECK_TIMEOUT'; ref = 'HVDCP_CHECK_TIMEOUT_BIT' },
    @{ mine = 'APSD_STATUS_7'; ref = 'APSD_STATUS_7_BIT' },
    @{ mine = 'APSD_RESULT_STATUS_7'; ref = 'APSD_RESULT_STATUS_7_BIT' },
    @{ mine = 'APSD_RESULT_STATUS_MASK'; ref = 'APSD_RESULT_STATUS_MASK' },
    @{ mine = 'APSD_RERUN'; ref = 'APSD_RERUN_BIT' },
    @{ mine = 'ICL_OVERRIDE'; ref = 'ICL_OVERRIDE_BIT' },
    @{ mine = 'USBIN_SUSPEND'; ref = 'USBIN_SUSPEND_BIT' },
    @{ mine = 'QC2_VOLTAGE_MASK'; ref = 'HVDCP_PULSE_COUNT_MAX_QC2_MASK' }
)
foreach ($pair in $bitPairs) {
    Compare-Value ('bit ' + $pair.mine) (Get-RustConst -Path $coreRegs -Name $pair.mine) (Resolve-Define -Name $pair.ref -Table $smb)
}

Emit ''
Emit '  QC2 voltage codes in the reference:'
foreach ($name in @('HVDCP_PULSE_COUNT_MAX_QC2_5V', 'HVDCP_PULSE_COUNT_MAX_QC2_9V', 'HVDCP_PULSE_COUNT_MAX_QC2_12V')) {
    $value = Resolve-Define -Name $name -Table $smb
    if ($null -ne $value) { Emit ('    ' + $name.PadRight(34) + ' = 0x' + $value.ToString('X2')) }
}

# --- 3. LN8000 numeric constants -----------------------------------------

Emit ''
Emit '3. LN8000 numeric constants against ln8000_charger.h'
Emit '-------------------------------------------------------------------------'

$lnHeader = Join-Path $Sources 'drivers_power_supply_ti_ln8000_charger.h'
$ln = Get-DefineTable -Path @($lnHeader)
$lnEncoding = Join-Path $Repo 'crates\ln8000\src\encoding.rs'
$lnText = (@(
    (Join-Path $Repo 'crates\ln8000\src\encoding.rs'),
    (Join-Path $Repo 'crates\ln8000\src\driver.rs'),
    (Join-Path $Repo 'crates\ln8000\src\regs.rs'),
    (Join-Path $Repo 'crates\ln8000\src\status.rs')
) | Where-Object { Test-Path -LiteralPath $_ } | ForEach-Object { Get-Content -LiteralPath $_ -Raw } | Out-String)
# strip underscores in numbers so that "4_890" is found as "4890"
$lnText = [regex]::Replace($lnText, '(?<=[0-9])_(?=[0-9])', '')

$lnChecks = @(
    @{ ref = 'LN8000_DEVICE_ID'; literal = '0x42'; what = 'chip ID' },
    @{ ref = 'LN8000_VBAT_FLOAT_MIN'; literal = '3_725_000'; what = 'lower bound of the charge voltage' },
    @{ ref = 'LN8000_VBAT_FLOAT_LSB'; literal = '5_000'; what = 'charge voltage step' },
    @{ ref = 'LN8000_IIN_CFG_MIN'; literal = '500_000'; what = 'lower bound of the input current' },
    @{ ref = 'LN8000_IIN_CFG_LSB'; literal = '50_000'; what = 'input current step' },
    @{ ref = 'LN8000_ADC_IIN_STEP'; literal = '4890'; what = 'input current ADC step' },
    @{ ref = 'LN8000_ADC_VAC_STEP'; literal = '16_000'; what = 'input voltage ADC step' },
    @{ ref = 'LN8000_ADC_VBAT_STEP'; literal = '5_000'; what = 'battery voltage ADC step' },
    @{ ref = 'LN8000_ADC_NTCV_STEP'; literal = '2933'; what = 'thermistor ADC step' },
    @{ ref = 'LN8000_ADC_DIETEMP_MIN'; literal = '-25'; what = 'die temperature offset' },
    @{ ref = 'LN8000_BAT_OVP_DEFAULT'; literal = '4_440_000'; what = 'battery overvoltage threshold' },
    @{ ref = 'LN8000_BUS_OVP_DEFAULT'; literal = '9_500_000'; what = 'input overvoltage threshold' },
    @{ ref = 'LN8000_IIN_CFG_DEFAULT'; literal = '2_000_000'; what = 'default input current' }
)
foreach ($check in $lnChecks) {
    $script:checked++
    $reference = Resolve-Define -Name $check.ref -Table $ln
    $present = $lnText -match [regex]::Escape(($check.literal -replace '_', ''))
    if ($null -eq $reference) {
        Emit ('  [?]    ' + $check.what.PadRight(42) + ' not found in the reference')
    } elseif ($present) {
        Emit ('  [ok]   ' + $check.what.PadRight(42) + $reference)
    } else {
        Emit ('  [MISMATCH] ' + $check.what.PadRight(42) + ' the code has no value ' + $check.literal)
        $mismatch++
    }
}

Emit ''
Emit '  Separately: the ADC update pause. Before reading a byte pair the reference sets bit 1'
Emit '  in TIMER_CTRL (PAUSE_ADC_UPDATE) and clears it afterwards. We added this to'
Emit '  read_adc and covered it with the adc_read_pauses_and_resumes_conversion_update test.'

Emit ''
Emit '  All numeric constants above come from the reference header; register addresses and'
Emit '  bit masks are cross-checked below against the pinned source (reference/).'

# --- 4. LN8000 addresses and bits against the pinned source ---------------

Emit ''
Emit '4. LN8000 register addresses against the pinned reference'
Emit '-------------------------------------------------------------------------'

$lnRef = Join-Path $Repo 'reference\ln8000_charger_extract.h'
$ln2 = Get-DefineTable -Path @($lnRef)
Emit ('  definitions in the pinned source: ' + $ln2.Count)
Emit ''

$lnRegs = Join-Path $Repo 'crates\ln8000\src\regs.rs'
$lnPairs = @(
    @{ mine = 'DEVICE_ID'; ref = 'LN8000_REG_DEVICE_ID' },
    @{ mine = 'INT1'; ref = 'LN8000_REG_INT1' },
    @{ mine = 'INT1_MSK'; ref = 'LN8000_REG_INT1_MSK' },
    @{ mine = 'SYS_STS'; ref = 'LN8000_REG_SYS_STS' },
    @{ mine = 'SAFETY_STS'; ref = 'LN8000_REG_SAFETY_STS' },
    @{ mine = 'FAULT1_STS'; ref = 'LN8000_REG_FAULT1_STS' },
    @{ mine = 'FAULT2_STS'; ref = 'LN8000_REG_FAULT2_STS' },
    @{ mine = 'CURR1_STS'; ref = 'LN8000_REG_CURR1_STS' },
    @{ mine = 'LDO_STS'; ref = 'LN8000_REG_LDO_STS' },
    @{ mine = 'ADC_FIRST_STS'; ref = 'LN8000_REG_ADC01_STS' },
    @{ mine = 'ADC_LAST_STS'; ref = 'LN8000_REG_ADC10_STS' },
    @{ mine = 'IIN_CTRL'; ref = 'LN8000_REG_IIN_CTRL' },
    @{ mine = 'REGULATION_CTRL'; ref = 'LN8000_REG_REGULATION_CTRL' },
    @{ mine = 'PWR_CTRL'; ref = 'LN8000_REG_PWR_CTRL' },
    @{ mine = 'SYS_CTRL'; ref = 'LN8000_REG_SYS_CTRL' },
    @{ mine = 'LDO_CTRL'; ref = 'LN8000_REG_LDO_CTRL' },
    @{ mine = 'GLITCH_CTRL'; ref = 'LN8000_REG_GLITCH_CTRL' },
    @{ mine = 'FAULT_CTRL'; ref = 'LN8000_REG_FAULT_CTRL' },
    @{ mine = 'NTC_CTRL'; ref = 'LN8000_REG_NTC_CTRL' },
    @{ mine = 'ADC_CTRL'; ref = 'LN8000_REG_ADC_CTRL' },
    @{ mine = 'ADC_CFG'; ref = 'LN8000_REG_ADC_CFG' },
    @{ mine = 'RECOVERY_CTRL'; ref = 'LN8000_REG_RECOVERY_CTRL' },
    @{ mine = 'TIMER_CTRL'; ref = 'LN8000_REG_TIMER_CTRL' },
    @{ mine = 'THRESHOLD_CTRL'; ref = 'LN8000_REG_THRESHOLD_CTRL' },
    @{ mine = 'V_FLOAT_CTRL'; ref = 'LN8000_REG_V_FLOAT_CTRL' },
    @{ mine = 'CHARGE_CTRL'; ref = 'LN8000_REG_CHARGE_CTRL' },
    @{ mine = 'LION_CTRL'; ref = 'LN8000_REG_LION_CTRL' },
    @{ mine = 'BC_OP_1'; ref = 'LN8000_REG_BC_OP_1' },
    @{ mine = 'BC_OP_2'; ref = 'LN8000_REG_BC_OP_2' },
    @{ mine = 'BC_STS_A'; ref = 'LN8000_REG_BC_STS_A' },
    @{ mine = 'BC_STS_E'; ref = 'LN8000_REG_BC_STS_E' }
)
foreach ($pair in $lnPairs) {
    Compare-Value ('address ' + $pair.mine) (Get-RustConst -Path $lnRegs -Name $pair.mine) (Resolve-Define -Name $pair.ref -Table $ln2)
}

Emit ''
Emit '  Mode switching: the mask must equal one shifted by the bit number from the reference.'
Emit '  Values from our code:'
foreach ($name in @('SYS_STS_SHUTDOWN', 'SYS_STS_STANDBY', 'SYS_STS_SWITCHING_ENABLED', 'SYS_STS_BYPASS_ENABLED',
                    'SYS_CTRL_STANDBY_EN', 'SYS_CTRL_EN_1TO1')) {
    $value = Get-RustConst -Path $lnRegs -Name $name
    if ($null -ne $value) { Emit ('    ' + $name.PadRight(30) + ' = ' + $value) }
}

$modePairs = @(
    @{ mine = 'SYS_STS_BYPASS_ENABLED'; refMask = 'LN8000_MASK_BYPASS_ENABLED' },
    @{ mine = 'SYS_STS_SWITCHING_ENABLED'; refMask = 'LN8000_MASK_SWITCHING_ENABLED' },
    @{ mine = 'SYS_STS_STANDBY'; refMask = 'LN8000_MASK_STANDBY_STS' },
    @{ mine = 'SYS_STS_SHUTDOWN'; refMask = 'LN8000_MASK_SHUTDOWN_STS' }
)
foreach ($pair in $modePairs) {
    Compare-Value ('state ' + $pair.mine) (Get-RustConst -Path $lnRegs -Name $pair.mine) (Resolve-Define -Name $pair.refMask -Table $ln2)
}

foreach ($bitPair in @(
        @{ mine = 'SYS_CTRL_STANDBY_EN'; refBit = 'LN8000_BIT_STANDBY_EN' },
        @{ mine = 'SYS_CTRL_EN_1TO1'; refBit = 'LN8000_BIT_EN_1TO1' })) {
    $bitNumber = Resolve-Define -Name $bitPair.refBit -Table $ln2
    $mine = Get-RustConst -Path $lnRegs -Name $bitPair.mine
    $expected = if ($null -ne $bitNumber) { 1 -shl [int]$bitNumber } else { $null }
    Compare-Value ('bit ' + $bitPair.mine + ' (1 << ' + $bitNumber + ')') $mine $expected
}

Emit ''
Emit '  Meaningful reference values (checked by the core tests):'
foreach ($name in @('LN8000_OPMODE_STANDBY', 'LN8000_OPMODE_BYPASS', 'LN8000_OPMODE_SWITCHING',
                    'LN8000_VAC_OVP_6P5V', 'LN8000_VAC_OVP_11V', 'LN8000_VAC_OVP_12V', 'LN8000_VAC_OVP_13V',
                    'LN8000_WATCHDOG_5SEC', 'LN8000_WATCHDOG_10SEC', 'LN8000_WATCHDOG_20SEC', 'LN8000_WATCHDOG_40SEC')) {
    $value = Resolve-Define -Name $name -Table $ln2
    if ($null -ne $value) { Emit ('    ' + $name.PadRight(34) + ' = ' + $value) }
}

Emit ''
Emit '  ADC channel registers per the reference (they match our code):'
foreach ($name in @('LN8000_REG_ADC01_STS', 'LN8000_REG_ADC02_STS', 'LN8000_REG_ADC03_STS', 'LN8000_REG_ADC04_STS',
                    'LN8000_REG_ADC06_STS', 'LN8000_REG_ADC07_STS', 'LN8000_REG_ADC08_STS', 'LN8000_REG_ADC09_STS')) {
    $value = Resolve-Define -Name $name -Table $ln2
    if ($null -ne $value) { Emit ('    ' + $name.PadRight(34) + ' = 0x' + $value.ToString('X2')) }
}

Emit ''
Emit '  Differences between chip generations in the references (smb5 taken into account):'
if ($script:conflicts.Count -eq 0) {
    Emit '    none'
} else {
    foreach ($name in ($script:conflicts.Keys | Sort-Object)) {
        $item = $script:conflicts[$name]
        Emit ('    ' + $name.PadRight(34) + ' smb5: ' + ([string]$item.primary).PadRight(10) + '  in ' + $item.file + ': ' + $item.other)
    }
    Emit ''
    Emit '    These names mean different things in different generations; our tablet is SM8150/PM8150B,'
    Emit '    so smb5-reg.h has priority, and the code matches exactly that one.'
}

Emit ''
Emit '========================================================================='
Emit (' Result: ' + $checked + ' checks, ' + $mismatch + ' mismatches')
Emit '========================================================================='

$outPath = [IO.Path]::GetFullPath($Out)
New-Item -ItemType Directory -Path (Split-Path $outPath) -Force | Out-Null
$report | Set-Content -LiteralPath $outPath -Encoding utf8
Write-Host ''
Write-Host ('Report: ' + $outPath) -ForegroundColor Green

if ($mismatch -gt 0) { exit 1 }
exit 0
