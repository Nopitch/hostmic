<#
.SYNOPSIS
    Copies a driver package, the release tools, devcon, campaign.ps1, verifier.ps1,
    memory-integrity.ps1 and devfund.ps1 into the test VM, in a folder of their own, and prints
    the command that runs the campaign there.

.DESCRIPTION
    Runs on the host, as a member of Hyper-V Administrators (Copy-VMFile needs the VM's Guest
    Service Interface). The package is either a local build
    (target\debug\hostmic_driver_package, target\release\hostmic_driver_package) or one the
    driver CI job published (gh run download <run id> --repo Nopitch/hostmic). The tools are
    built with:

        cargo build --release -p hostmic-test-host -p hostmic-capture-check -p hostmic-measure -p hostmic-handle-check -p hostmic-ctl

.PARAMETER Package
    The folder holding hostmic_driver.sys, .inf, .cat and WDRLocalTestCert.cer.

.PARAMETER Tools
    The folder holding the release tools (default: target\release of this repository).

.PARAMETER Name
    The folder's name under C:\hostmic in the VM (default: the date and time).

.PARAMETER VMName
    The test VM (default hostmic-test).

.PARAMETER Devcon
    devcon.exe (default: the newest x64 one of the installed WDK).

.PARAMETER DevFund
    Also copies the WDK's Device Fundamentals tests, TAEF and the WDTF installers into
    C:\hostmic\devfund in the VM (devfund.ps1 uses them): once per VM, after which packages are
    staged without it. Copying them again while TAEF's service runs would meet locked files.

.PARAMETER Kit
    The WDK's Testing folder (default: the installed WDK's).
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$Package,
    [string]$Tools = (Join-Path (Split-Path (Split-Path $PSScriptRoot)) 'target\release'),
    [ValidatePattern('^[A-Za-z0-9._-]+$')]
    [string]$Name = (Get-Date -Format 'yyyyMMdd-HHmmss'),
    [string]$VMName = 'hostmic-test',
    [string]$Devcon,
    [switch]$DevFund,
    [string]$Kit = 'C:\Program Files (x86)\Windows Kits\10\Testing'
)

Set-StrictMode -Version 3
$ErrorActionPreference = 'Stop'

# Copies every file under $Source into the VM under $Destination, keeping the subfolders.
function Copy-Tree([string]$Source, [string]$Destination, [string]$Filter = '*') {
    $root = (Get-Item -LiteralPath $Source).FullName.TrimEnd('\')
    $count = 0
    foreach ($item in Get-ChildItem -LiteralPath $root -Recurse -File -Filter $Filter) {
        $relative = $item.FullName.Substring($root.Length + 1)
        Copy-VMFile -Name $VMName -SourcePath $item.FullName -DestinationPath (Join-Path $Destination $relative) `
            -FileSource Host -CreateFullPath -Force
        $count++
    }
    $count
}

if (-not $Devcon) {
    $Devcon = Get-ChildItem 'C:\Program Files (x86)\Windows Kits\10\Tools\*\x64\devcon.exe' -ErrorAction SilentlyContinue |
        Sort-Object FullName | Select-Object -Last 1 -ExpandProperty FullName
    if (-not $Devcon) { throw 'devcon.exe not found in the WDK: pass -Devcon' }
}

$files = @()
foreach ($file in 'hostmic_driver.sys', 'hostmic_driver.inf', 'hostmic_driver.cat', 'WDRLocalTestCert.cer') {
    $files += Join-Path $Package $file
}
foreach ($tool in 'test-host', 'capture-check', 'measure', 'handle-check', 'hostmic-ctl') {
    $files += Join-Path $Tools "$tool.exe"
}
$files += $Devcon, (Join-Path $PSScriptRoot 'campaign.ps1'), (Join-Path $PSScriptRoot 'verifier.ps1'),
(Join-Path $PSScriptRoot 'memory-integrity.ps1'), (Join-Path $PSScriptRoot 'devfund.ps1')
foreach ($file in $files) {
    if (-not (Test-Path -LiteralPath $file -PathType Leaf)) { throw "missing $file" }
}

if ($DevFund) {
    # After the package check: the kit is long to copy. TAEF runs from its folder; the WDTF
    # installers need the runtime cabinets beside them.
    $copied = Copy-Tree (Join-Path $Kit 'Runtimes\TAEF\x64') 'C:\hostmic\devfund\taef'
    foreach ($item in Get-ChildItem -LiteralPath (Join-Path $Kit 'Runtimes') -File |
        Where-Object { $_.Extension -eq '.cab' -or $_.Name -match 'x64.*\.msi$' }) {
        Copy-VMFile -Name $VMName -SourcePath $item.FullName -DestinationPath "C:\hostmic\devfund\runtimes\$($item.Name)" `
            -FileSource Host -CreateFullPath -Force
        $copied++
    }
    $copied += Copy-Tree (Join-Path $Kit 'Tests\Additional Tests\x64\DevFund') 'C:\hostmic\devfund\tests'
    "copied     $copied Device Fundamentals files to $VMName`:C:\hostmic\devfund"
}

$destination = "C:\hostmic\$Name"
foreach ($file in $files) {
    $item = Get-Item -LiteralPath $file
    Copy-VMFile -Name $VMName -SourcePath $item.FullName -DestinationPath (Join-Path $destination $item.Name) `
        -FileSource Host -CreateFullPath -Force
    # When each file was built: a stale tool or package shows here.
    '{0,-22} {1:yyyy-MM-dd HH:mm}' -f $item.Name, $item.LastWriteTime
}
"copied     $($files.Count) files to $VMName`:$destination"
"run, in the VM's elevated PowerShell:"
"powershell -ExecutionPolicy Bypass -File $destination\campaign.ps1"
