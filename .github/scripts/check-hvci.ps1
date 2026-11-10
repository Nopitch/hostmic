<#
.SYNOPSIS
    Checks driver binaries' PE headers against Memory Integrity's (HVCI's) requirements that a
    file shows: no section both writable and executable, sections aligned on pages, and
    the image marked NX compatible.

.DESCRIPTION
    From "Driver compatibility with memory integrity and VBS" on Microsoft Learn ("How to build
    compatible drivers": section alignment a multiple of the page size). What the driver does at run time (executable pool, mappings) is
    Driver Verifier's code integrity checking and a run with Memory Integrity on, in the test
    VM (docs/test-vm.md). Reads the headers itself, without dumpbin, so that it needs no
    Visual Studio. Exits 1 when a file fails, 2 when one cannot be read.

        pwsh .github/scripts/check-hvci.ps1 target/release/hostmic_driver_package/hostmic_driver.sys
#>
param(
    [Parameter(Mandatory, ValueFromRemainingArguments)]
    [string[]]$Path
)

Set-StrictMode -Version 3
$ErrorActionPreference = 'Stop'

$ImageScnMemExecute = [uint32]'0x20000000'
$ImageScnMemWrite = [uint32]'0x80000000'
$NxCompat = 0x0100

$failed = $false
foreach ($file in $Path) {
    try {
        $bytes = [System.IO.File]::ReadAllBytes((Resolve-Path -LiteralPath $file))
        if ($bytes.Length -lt 0x40 -or [System.BitConverter]::ToUInt16($bytes, 0) -ne 0x5A4D) { throw 'no MZ header' }
        $pe = [System.BitConverter]::ToInt32($bytes, 0x3C)
        if ($pe -lt 0 -or $pe + 24 -gt $bytes.Length -or [System.BitConverter]::ToUInt32($bytes, $pe) -ne 0x00004550) {
            throw 'no PE signature'
        }
        $sections = [System.BitConverter]::ToUInt16($bytes, $pe + 6)
        $optionalSize = [System.BitConverter]::ToUInt16($bytes, $pe + 20)
        if ($optionalSize -lt 72) { throw "optional header of $optionalSize bytes" }
        $optional = $pe + 24
        if ($optional + 72 -gt $bytes.Length) { throw 'truncated optional header' }
        if ([System.BitConverter]::ToUInt16($bytes, $optional) -ne 0x20B) { throw 'not a PE32+ image' }
        $alignment = [System.BitConverter]::ToUInt32($bytes, $optional + 32)
        $dllCharacteristics = [System.BitConverter]::ToUInt16($bytes, $optional + 70)
        $table = $optional + $optionalSize
        if ($table + 40 * $sections -gt $bytes.Length) { throw 'truncated section table' }

        $problems = @()
        if ($alignment -eq 0 -or $alignment % 0x1000 -ne 0) {
            $problems += ('section alignment 0x{0:X}, not a multiple of 0x1000' -f $alignment)
        }
        if (($dllCharacteristics -band $NxCompat) -eq 0) { $problems += 'not marked NX compatible' }
        $names = @()
        for ($i = 0; $i -lt $sections; $i++) {
            $entry = $table + 40 * $i
            $name = [System.Text.Encoding]::ASCII.GetString($bytes, $entry, 8).TrimEnd([char]0)
            $characteristics = [System.BitConverter]::ToUInt32($bytes, $entry + 36)
            $execute = ($characteristics -band $ImageScnMemExecute) -ne 0
            $write = ($characteristics -band $ImageScnMemWrite) -ne 0
            if ($execute -and $write) { $problems += "section $name is writable and executable" }
            $names += $name + $(if ($execute) { ' (x)' } elseif ($write) { ' (w)' } else { '' })
        }
        if ($problems) {
            $failed = $true
            Write-Host "FAIL $file`: $($problems -join '; ')"
        } else {
            Write-Host "PASS $file`: page-aligned sections, NX compatible, no writable and executable section among $sections ($($names -join ', '))"
        }
    } catch {
        Write-Host "ERROR $file`: $($_.Exception.Message)"
        exit 2
    }
}
if ($failed) { exit 1 }
