#Requires -Version 7.0
<#
    check-all.ps1 - one run of every project check with a single command.

    Run:
        .\check-all.ps1
        .\check-all.ps1 -SkipBuilds     # without building the drivers (fast)

    What is run and where the result is written:
      1. formatting            cargo fmt --check
      2. linter                cargo clippy --workspace -- -D warnings
      3. tests                 cargo test --workspace
      4. no-std build          both cores
      5. ARM64 drivers         cargo wdk build (both) + SHA-256
      6. driver package        deploy/verify-package.ps1
      7. sources and dump      deploy/verify-sources.ps1
      8. build                 deploy/build-arm64.ps1 (reproducibility check)

    Report: artifacts\check-all.txt, the verdict is at the end. Exit code 0 if
    everything matched. The script is meant to be run by a third-party engineer
    taking over the work: it needs neither a tablet nor access to the hardware.
#>
[CmdletBinding()]
param(
    [string]$Repo = (Join-Path $PSScriptRoot '..'),
    [string]$Out = (Join-Path $PSScriptRoot '..\artifacts\check-all.txt'),
    [switch]$SkipBuilds
)

$ErrorActionPreference = 'Continue'
$lines = New-Object System.Collections.Generic.List[string]
$failures = New-Object System.Collections.Generic.List[string]
$passed = 0

function Say {
    param([string]$Text = '')
    $lines.Add($Text) | Out-Null
    Write-Host $Text
}

function Step {
    param([string]$Title, [scriptblock]$Action)
    Say ''
    Say ('--- ' + $Title)
    $watch = [Diagnostics.Stopwatch]::StartNew()
    try {
        $result = & $Action
        $watch.Stop()
        if ($result -eq $true) {
            $script:passed++
            Say ('    result: ok (' + [math]::Round($watch.Elapsed.TotalSeconds, 1) + ' s)')
        } else {
            $failures.Add($Title) | Out-Null
            Say ('    result: ERROR (' + [math]::Round($watch.Elapsed.TotalSeconds, 1) + ' s)')
        }
    } catch {
        $watch.Stop()
        $failures.Add($Title + ': ' + $_.Exception.Message) | Out-Null
        Say ('    result: EXCEPTION: ' + $_.Exception.Message)
    }
}

$env:CARGO_TERM_COLOR = 'never'
$env:LIBCLANG_PATH = if ($env:LIBCLANG_PATH) { $env:LIBCLANG_PATH } else { 'C:\Program Files\LLVM\bin' }

Say '========================================================================='
Say ' Full run of the nabu project checks (without hardware)'
Say (' Time: ' + (Get-Date -Format 'yyyy-MM-dd HH:mm:ss'))
Say (' Directory: ' + (Resolve-Path -LiteralPath $Repo))
Say '========================================================================='

Push-Location $Repo
try {
    Step 'Formatting (cargo fmt --check)' {
        $out = & cargo fmt --all --check 2>&1
        if ($out) { $out | Select-Object -First 10 | ForEach-Object { Say ('    ' + $_) } }
        $LASTEXITCODE -eq 0
    }

    Step 'Linter (cargo clippy -D warnings)' {
        $out = & cargo clippy --workspace --all-targets -- -D warnings 2>&1
        $bad = $out | Select-String -Pattern '^error|^warning:'
        if ($bad) { $bad | Select-Object -First 10 | ForEach-Object { Say ('    ' + $_.Line) } }
        $LASTEXITCODE -eq 0
    }

    Step 'Tests (cargo test --workspace)' {
        $out = & cargo test --workspace 2>&1
        $total = 0
        $failed = 0
        foreach ($line in $out) {
            if ($line -match 'test result: ok\. (\d+) passed') { $total += [int]$Matches[1] }
            if ($line -match 'test result: FAILED') { $failed++ }
        }
        Say ('    tests passed: ' + $total + ', failures: ' + $failed)
        ($LASTEXITCODE -eq 0) -and ($failed -eq 0)
    }

    Step 'Building the cores without the standard library' {
        $core = & cargo build -p charger-core --no-default-features 2>&1; $c1 = $LASTEXITCODE
        $ln = & cargo build -p ln8000 --no-default-features 2>&1; $c2 = $LASTEXITCODE
        Say ('    charger-core: ' + $c1 + ', ln8000: ' + $c2)
        ($c1 -eq 0) -and ($c2 -eq 0)
    }
} finally {
    Pop-Location
}

if (-not $SkipBuilds) {
    Step 'Building the ARM64 drivers and the checksums' {
        $out = & (Join-Path $PSScriptRoot 'build-arm64.ps1') 2>&1
        $out | Select-String -Pattern 'Finished building|reproducibility|note:|ERROR' |
            ForEach-Object { Say ('    ' + $_.Line.Trim()) }
        $LASTEXITCODE -eq 0
    }
}

Step 'Standalone driver package check' {
    $out = & (Join-Path $PSScriptRoot 'verify-package.ps1') 2>&1
    $out | Select-String -Pattern 'RESULT|MISMATCH|no checksum' | ForEach-Object { Say ('    ' + $_.Line.Trim()) }
    $LASTEXITCODE -eq 0
}

Step 'Cross-check against the sources and the live dump' {
    $out = & (Join-Path $PSScriptRoot 'verify-sources.ps1') 2>&1
    $out | Select-String -Pattern 'Result|MISMATCH' | ForEach-Object { Say ('    ' + $_.Line.Trim()) }
    $LASTEXITCODE -eq 0
}

Say ''
Say '========================================================================='
if ($failures.Count -eq 0) {
    Say (' VERDICT: everything matched. Steps passed: ' + $passed)
} else {
    Say (' VERDICT: problems ' + $failures.Count + ' out of ' + ($passed + $failures.Count))
    foreach ($item in $failures) { Say ('   - ' + $item) }
}
Say (' Time: ' + (Get-Date -Format 'yyyy-MM-dd HH:mm:ss'))
Say '========================================================================='

$outPath = [IO.Path]::GetFullPath($Out)
New-Item -ItemType Directory -Path (Split-Path $outPath) -Force | Out-Null
$lines | Set-Content -LiteralPath $outPath -Encoding utf8
Write-Host ''
Write-Host ('Report: ' + $outPath) -ForegroundColor Green

if ($failures.Count -gt 0) { exit 1 }
exit 0
