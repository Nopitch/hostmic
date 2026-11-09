<#
.SYNOPSIS
    Sets Driver Verifier for hostmic in the test VM: one of the M6 profiles, then a restart.

.DESCRIPTION
    Runs in the test VM only, from an elevated Windows PowerShell. Driver Verifier's settings
    are kept in the registry and take effect at the next boot (the PortCls and code integrity
    rule classes cannot be enabled without one): after this script, restart the VM, then run
    campaign.ps1, whose header shows the rule classes in effect.

        powershell -ExecutionPolicy Bypass -File C:\hostmic\<name>\verifier.ps1 standard

    Profiles (rule classes from "Driver Verifier Command Syntax" on Microsoft Learn):
      standard       hostmic_driver.sys under the standard rule classes (special pool, IRQL
                     checking, pool tracking, I/O verification, deadlock detection, security
                     checks, miscellaneous checks, DDI compliance), the additional DDI
                     compliance rules, port/miniport interface checking (PortCls's audio
                     rules) and code integrity checks
      stack          the same rule classes on hostmic_driver.sys, portcls.sys and ks.sys, so
                     that I/O verification follows requests across the audio stack
      low-resources  randomized low resources simulation on hostmic_driver.sys: allocations
                     fail at random (after the first minutes of the boot); requests may fail,
                     the system must not
      off            Driver Verifier off
      status         the settings for the next boot and the drivers verified now

    Each profile applies to the next boot only (verifier /bootmode oneboot): set it, restart,
    run the campaign; a violation found while Windows starts cannot repeat at every boot. It
    also sets a
    kernel memory dump (C:\Windows\MEMORY.DMP), kept across bug checks, for
    WinDbg's !analyze -v and !verifier.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory, Position = 0)]
    [ValidateSet('standard', 'stack', 'low-resources', 'off', 'status')]
    [string]$Mode
)

# Windows PowerShell 5.1, ASCII only (see campaign.ps1).
Set-StrictMode -Version 3
$ErrorActionPreference = 'Stop'

$principal = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    [Console]::Error.WriteLine('run verifier.ps1 from an elevated PowerShell')
    exit 255
}

# 1 special pool, 2 IRQL checking, 4 pool tracking, 5 I/O verification, 6 deadlock detection,
# 9 security checks, 12 miscellaneous checks, 18 DDI compliance (the standard classes without
# DMA and WDF verification, which do not apply to a WDM driver without DMA), 20 additional DDI
# compliance, 17 port/miniport interface checking, 26 code integrity checks.
$Classes = '1', '2', '4', '5', '6', '9', '12', '17', '18', '20', '26'

function Invoke-Verifier([string[]]$Arguments) {
    $ErrorActionPreference = 'Continue'
    $output = (& verifier.exe @Arguments 2>&1 | Out-String)
    $code = $LASTEXITCODE
    $output.TrimEnd()
    # verifier.exe exits 2 when the change needs a restart, 0 when nothing changed.
    if ($code -ne 0 -and $code -ne 2) { throw "verifier $($Arguments -join ' ') exited $code" }
}

# The settings apply to the next boot only: a violation found while the system starts cannot
# repeat at every boot, and every run starts from a profile set on purpose. ("resetonbootfail"
# was tried first: in the VM it brought back the previous profile's settings at the next boot.)
function Set-BootMode {
    Invoke-Verifier (@('/bootmode', 'oneboot'))
}

function Set-KernelDump {
    $key = 'HKLM:\SYSTEM\CurrentControlSet\Control\CrashControl'
    Set-ItemProperty -Path $key -Name CrashDumpEnabled -Value 2 -Type DWord   # kernel memory dump
    Set-ItemProperty -Path $key -Name Overwrite -Value 1 -Type DWord
    Set-ItemProperty -Path $key -Name AutoReboot -Value 1 -Type DWord
    Set-ItemProperty -Path $key -Name DumpFile -Value '%SystemRoot%\MEMORY.DMP' -Type ExpandString
}

switch ($Mode) {
    'standard' {
        Invoke-Verifier (@('/reset'))
        Invoke-Verifier (@('/rc') + $Classes + @('/driver', 'hostmic_driver.sys'))
        Set-BootMode
        Set-KernelDump
    }
    'stack' {
        Invoke-Verifier (@('/reset'))
        Invoke-Verifier (@('/rc') + $Classes + @('/driver', 'hostmic_driver.sys', 'portcls.sys', 'ks.sys'))
        Set-BootMode
        Set-KernelDump
    }
    'low-resources' {
        Invoke-Verifier (@('/reset'))
        Invoke-Verifier @('/rc', '3', '/driver', 'hostmic_driver.sys')
        Set-BootMode
        Set-KernelDump
    }
    'off' {
        Invoke-Verifier (@('/reset'))
    }
    'status' {
        '--- settings for the next boot (verifier /querysettings)'
        Invoke-Verifier (@('/querysettings'))
        '--- verified now (verifier /query)'
        Invoke-Verifier (@('/query'))
        return
    }
}
''
"profile    ${Mode}: restart the VM for it to take effect (shutdown /r /t 0), then run campaign.ps1"
