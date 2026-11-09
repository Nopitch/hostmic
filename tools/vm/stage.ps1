<#
.SYNOPSIS
    Copies a driver package, the release tools, devcon, campaign.ps1 and verifier.ps1 into the
    test VM, in a folder of their own, and prints the command that runs the campaign there.

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
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$Package,
    [string]$Tools = (Join-Path (Split-Path (Split-Path $PSScriptRoot)) 'target\release'),
    [ValidatePattern('^[A-Za-z0-9._-]+$')]
    [string]$Name = (Get-Date -Format 'yyyyMMdd-HHmmss'),
    [string]$VMName = 'hostmic-test',
    [string]$Devcon
)

Set-StrictMode -Version 3
$ErrorActionPreference = 'Stop'

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
$files += $Devcon, (Join-Path $PSScriptRoot 'campaign.ps1'), (Join-Path $PSScriptRoot 'verifier.ps1')
foreach ($file in $files) {
    if (-not (Test-Path -LiteralPath $file -PathType Leaf)) { throw "missing $file" }
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
