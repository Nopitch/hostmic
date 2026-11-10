<#
.SYNOPSIS
    Turns Memory Integrity (hypervisor-protected code integrity, HVCI) on or off in the test
    VM, or shows whether it runs.

.DESCRIPTION
    Runs in the test VM only, from an elevated Windows PowerShell. Memory Integrity is set in
    the registry and starts at the next boot: after "on" or "off", restart the VM from Windows
    (shutdown /r /t 0), then run "status", or campaign.ps1, whose header says whether it runs.

        powershell -ExecutionPolicy Bypass -File C:\hostmic\<name>\memory-integrity.ps1 on

    Modes:
      on      the registry keys of "Enable memory integrity" on Microsoft Learn, recommended
              settings without UEFI lock: virtualization-based security (VBS) on, Secure Boot
              as the required platform feature, Memory Integrity on; not "Mandatory", so a VM
              where the hypervisor or the secure kernel cannot start still boots
      off     Memory Integrity and VBS off
      status  what is configured and what runs (Win32_DeviceGuard), test signing, Secure Boot

    A Hyper-V generation 2 VM can run Memory Integrity without nested virtualization ("Memory
    integrity deployment in virtual machines"). The registry says what is asked for, not what
    runs: only Win32_DeviceGuard's SecurityServicesRunning containing 2 means Memory Integrity
    runs. In the test VM it does not: VBS requires Secure Boot (the only documented settings
    are Secure Boot, and Secure Boot with DMA protection), which test signing needs off; the
    "platform" line names what is missing. The run with Memory Integrity on is M7's, with a
    signed driver and Secure Boot on (PLAN.md).
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory, Position = 0)]
    [ValidateSet('on', 'off', 'status')]
    [string]$Mode
)

# Windows PowerShell 5.1, ASCII only (see campaign.ps1).
Set-StrictMode -Version 3
$ErrorActionPreference = 'Stop'

$principal = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    [Console]::Error.WriteLine('run memory-integrity.ps1 from an elevated PowerShell')
    exit 255
}

$DeviceGuard = 'HKLM:\SYSTEM\CurrentControlSet\Control\DeviceGuard'
$Hvci = Join-Path $DeviceGuard 'Scenarios\HypervisorEnforcedCodeIntegrity'

function Set-Value([string]$Key, [string]$Name, [int]$Value) {
    if (-not (Test-Path $Key)) { $null = New-Item -Path $Key -Force }
    Set-ItemProperty -Path $Key -Name $Name -Value $Value -Type DWord
}

function Get-Value([string]$Key, [string]$Name) {
    $item = Get-ItemProperty -Path $Key -Name $Name -ErrorAction SilentlyContinue
    if ($item) { $item.$Name } else { 'unset' }
}

# Win32_DeviceGuard's lists hold numbers: 2 is Memory Integrity in SecurityServicesConfigured
# and SecurityServicesRunning; VirtualizationBasedSecurityStatus is 0 off, 1 enabled but not
# running, 2 running.
function Get-Status {
    $guard = Get-CimInstance -ClassName Win32_DeviceGuard -Namespace root\Microsoft\Windows\DeviceGuard
    $vbs = switch ([int]$guard.VirtualizationBasedSecurityStatus) {
        0 { 'off' }
        1 { 'enabled but not running' }
        2 { 'running' }
        default { "status $_" }
    }
    $configured = @($guard.SecurityServicesConfigured) -contains 2
    $running = @($guard.SecurityServicesRunning) -contains 2
    # Platform security properties: 1 hypervisor support, 2 Secure Boot, 3 DMA protection,
    # 4 secure memory overwrite, 5 NX protections, 6 SMM mitigations, 7 MBEC or GMET, 8 APIC
    # virtualization. VBS does not start while a required one is missing.
    $names = @{ 1 = 'hypervisor support'; 2 = 'secure boot'; 3 = 'DMA protection'; 4 = 'secure memory overwrite';
        5 = 'NX protections'; 6 = 'SMM mitigations'; 7 = 'MBEC or GMET'; 8 = 'APIC virtualization' }
    $available = @($guard.AvailableSecurityProperties | ForEach-Object { [int]$_ })
    $required = @($guard.RequiredSecurityProperties | ForEach-Object { [int]$_ } | Where-Object { $_ -ne 0 })
    $missing = @($required | Where-Object { $available -notcontains $_ } | ForEach-Object { if ($names.ContainsKey($_)) { $names[$_] } else { "property $_" } })
    # The options this boot started with, in any display language (bcdedit translates "Yes").
    $options = (Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control' -Name SystemStartOptions).SystemStartOptions
    $testSigning = if ($options -match '\bTESTSIGNING\b') { 'on' } else { 'off' }
    $ErrorActionPreference = 'Continue'
    $secureBoot = try { if (Confirm-SecureBootUEFI) { 'on' } else { 'off' } } catch { 'off' }
    "registry          VBS $(Get-Value $DeviceGuard 'EnableVirtualizationBasedSecurity'), required platform features $(Get-Value $DeviceGuard 'RequirePlatformSecurityFeatures'), memory integrity $(Get-Value $Hvci 'Enabled')"
    "device guard      VBS $vbs; memory integrity configured $configured, running $running"
    "platform          available $($available -join ' '), required $($required -join ' ')$(if ($missing -and $vbs -ne 'off') { '; missing: ' + ($missing -join ', ') })"
    "boot              test signing $testSigning, secure boot $secureBoot"
    if ($running) { 'memory integrity RUNNING' } else { 'memory integrity NOT RUNNING' }
}

switch ($Mode) {
    'on' {
        Set-Value $DeviceGuard 'EnableVirtualizationBasedSecurity' 1
        Set-Value $DeviceGuard 'RequirePlatformSecurityFeatures' 1
        Set-Value $DeviceGuard 'Locked' 0
        Set-Value $Hvci 'Enabled' 1
        Set-Value $Hvci 'Locked' 0
    }
    'off' {
        Set-Value $Hvci 'Enabled' 0
        Set-Value $DeviceGuard 'EnableVirtualizationBasedSecurity' 0
    }
    'status' {
        Get-Status
        return
    }
}
Get-Status
''
"mode       ${Mode}: restart the VM for it to take effect (shutdown /r /t 0), then run memory-integrity.ps1 status"
