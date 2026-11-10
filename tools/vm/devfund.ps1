<#
.SYNOPSIS
    Runs the WDK's Device Fundamentals tests on hostmic's two devices in the test VM: the
    adapter (the Kernel Streaming filters) and the host interface (the IOCTLs). Prints one PASS,
    FAIL or SKIP line per test.

.DESCRIPTION
    Runs in the test VM only, from an elevated Windows PowerShell, next to a package that
    tools/vm/stage.ps1 copied, after `stage.ps1 -DevFund` copied the tests, TAEF and the WDTF
    installers into C:\hostmic\devfund ("Run DevFund tests via the command line" on Microsoft
    Learn). The first run installs WDTF. It installs the driver, runs the tests with TAEF
    (TE.exe), each on a device selected by its instance ID (WDTF's "DQ" parameter), removes the
    test filter the PnP tests add (DisableEnhancedDeviceTestingSupport), then removes the driver.

        powershell -ExecutionPolicy Bypass -File C:\hostmic\<name>\devfund.ps1

    With -Only and several groups, call it from PowerShell rather than with -File, which passes
    "fuzz,reinstall" as one string: Set-ExecutionPolicy -Scope Process Bypass, then
    & C:\hostmic\<name>\devfund.ps1 -Only fuzz,reinstall

    Groups (-Only runs a subset):
      pnp        PNPDTest on the adapter and on the host interface: disable and enable, remove
                 and restart, cancelled remove, cancelled stop, stop and restart, failed
                 restart, surprise removal (not "DIF remove and rescan parent": nothing
                 enumerates a root device again)
      fuzz       the IOCTL fuzzer (function codes 0 to 15 on the adapter, 0x8F0 to 0x91F on the
                 host interface) and the open, close and miscellaneous API fuzzer, on both;
                 WDTF finds no device to fuzz on the host interface (a raw device): SKIP
      reinstall  the driver reinstalled with I/O before and after, on the adapter

    Each test's TAEF output goes to a log folder next to the script. A test passes when TAEF
    counts it passed and found a device to test; one TAEF skips or blocks (a precondition the
    device does not meet) or that found none is SKIP, with the reason in its log. A run where
    nothing passed ends NONE (exit code 254); otherwise the exit code is the number of failed
    tests. The PnP tests take about two minutes each on the adapter, the whole run about an
    hour.

.PARAMETER Only
    The groups to run (default: all).
#>
[CmdletBinding()]
param(
    [ValidateSet('pnp', 'fuzz', 'reinstall')]
    [string[]]$Only
)

# Windows PowerShell 5.1, ASCII only (see campaign.ps1).
Set-StrictMode -Version 3
$ErrorActionPreference = 'Stop'

$Dir = $PSScriptRoot
$Kit = 'C:\hostmic\devfund'
$Te = Join-Path $Kit 'taef\TE.exe'
$Groups = 'pnp', 'fuzz', 'reinstall'
if ($Only) { $Groups = $Groups | Where-Object { $Only -contains $_ } }
$LogDir = Join-Path $Dir ('devfund-' + (Get-Date -Format 'yyyyMMdd-HHmmss'))
$null = New-Item -ItemType Directory -Path $LogDir
$script:Results = @()

$principal = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    [Console]::Error.WriteLine('run devfund.ps1 from an elevated PowerShell')
    exit 255
}
foreach ($path in $Te, (Join-Path $Kit 'tests'), (Join-Path $Kit 'runtimes'),
    (Join-Path $Dir 'hostmic_driver.inf'), (Join-Path $Dir 'devcon.exe')) {
    if (-not (Test-Path -LiteralPath $path)) {
        [Console]::Error.WriteLine("missing $path (tools/vm/stage.ps1 -DevFund copies it)")
        exit 255
    }
}

# ---------------------------------------------------------------------------------------------
# Setup

# WDTF, once: the TAEF and WDTF installers of the WDK's Testing\Runtimes. The order matters:
# WDTF's product installer runs custom actions that install its filter drivers (msdmfilt, the
# PnP tests' filter, and IoSpy) from files the content installer puts in place; installed
# first, it fails ("Failed to install IOSPY filter driver: 2", msiexec 1603).
$marker = Join-Path $Kit 'wdtf-installed.txt'
if (-not (Test-Path $marker)) {
    foreach ($msi in 'Test Authoring and Execution Framework x64-x64_en-us.msi',
        'OneCoreUap_WDTF_Desktop_Kit_Content-x64_en-us.msi', 'WDTF_TestRunner_Kit_Content-x64_en-us.msi',
        'WDTF_Desktop_Kit_Product-x64_en-us.msi') {
        $log = Join-Path $LogDir ("msi-" + ($msi -replace '[^A-Za-z0-9]+', '-') + '.txt')
        $arguments = @('/i', ('"' + (Join-Path $Kit "runtimes\$msi") + '"'), '/qn', '/norestart', '/l*v', ('"' + $log + '"'))
        $process = Start-Process msiexec.exe -ArgumentList $arguments -Wait -PassThru
        # 3010: installed, a restart would complete it; 1638: this version is installed already.
        if ($process.ExitCode -notin 0, 3010, 1638) {
            [Console]::Error.WriteLine("installing $msi failed: msiexec exit $($process.ExitCode) (log $log)")
            exit 255
        }
    }
    Set-Content -Path $marker -Value (Get-Date -Format s)
}

# TAEF blocks a test that may restart the machine unless it can resume after the restart: its
# service does that, with a state file per run (TE.exe /RebootStateFile). Started for the run
# and stopped after it, so that it does not hold the kit's files (a new stage.ps1 -DevFund).
if (-not (Get-Service -Name 'TE.Service' -ErrorAction SilentlyContinue)) {
    $null = & (Join-Path $Kit 'taef\Wex.Services.exe') /install:TE.Service
}
Set-Service -Name 'TE.Service' -StartupType Manual
if ((Get-Service -Name 'TE.Service').Status -ne 'Running') { Start-Service -Name 'TE.Service' }

function Invoke-Devcon([string[]]$Arguments) {
    $ErrorActionPreference = 'Continue'
    (& (Join-Path $Dir 'devcon.exe') @Arguments 2>&1 | Out-String)
}

function Get-Adapter {
    Get-PnpDevice -PresentOnly -ErrorAction SilentlyContinue |
        Where-Object { $_.InstanceId -like 'ROOT\MEDIA\*' -and $_.FriendlyName -eq 'hostmic' } |
        Select-Object -First 1
}

function Get-HostInterface {
    Get-PnpDevice -PresentOnly -ErrorAction SilentlyContinue |
        Where-Object { $_.InstanceId -like 'HOSTMIC\*' } | Select-Object -First 1
}

# The host interface's instance ID, once its device is back (the PnP tests remove and restart
# it); throws after 30 s.
function Wait-HostInterface {
    $end = (Get-Date).AddSeconds(30)
    do {
        $device = Get-HostInterface
        if ($device -and $device.Status -eq 'OK') { return $device.InstanceId }
        Start-Sleep -Milliseconds 500
    } while ((Get-Date) -lt $end)
    throw 'the host interface did not come back within 30 s'
}

# A driver left installed by an earlier run would be tested instead of this package's.
$cer = Join-Path $Dir 'WDRLocalTestCert.cer'
$null = certutil -addstore Root $cer
$null = certutil -f -addstore TrustedPublisher $cer
if (Get-Adapter) {
    $remove = Invoke-Devcon @('remove', 'root\hostmic')
    if ($remove -match 'reboot') {
        [Console]::Error.WriteLine('a driver already installed cannot be removed without a restart: restart the VM')
        exit 255
    }
    Start-Sleep -Seconds 3
}
$install = Invoke-Devcon @('install', (Join-Path $Dir 'hostmic_driver.inf'), 'root\hostmic')
if ($install -notmatch 'successfully' -or $install -match 'reboot') {
    [Console]::Error.WriteLine("devcon install failed: $(($install -split "`r?`n" | Where-Object { $_.Trim() } | Select-Object -Last 1))")
    exit 255
}
Start-Sleep -Seconds 5
$adapter = Get-Adapter
if (-not $adapter -or -not (Get-HostInterface)) {
    [Console]::Error.WriteLine('the driver did not install: no adapter or no host interface')
    exit 255
}

# ---------------------------------------------------------------------------------------------
# Runs

# Runs one TAEF test (a name, wildcards allowed) of $Dll on the device $InstanceId, with extra
# TAEF parameters ("Name=Value"); records one result line from TAEF's own summary. A test that
# outlives its time is stopped with TAEF's test host, which would otherwise go on alone.
# $NothingIsSkip: WDTF finding no device to test is a SKIP, not a FAIL (see below).
function Invoke-DevFund([string]$Group, [string]$Dll, [string]$Test, [string]$Target, [string]$InstanceId,
    [string[]]$Parameters = @(), [int]$Minutes = 30, [switch]$NothingIsSkip) {
    # A test's own name, or the library's for a whole class ("CFuzz::*").
    $name = $Test -replace '^[^:]+::', '' -replace '[#*].*$', ''
    if (-not $name) { $name = $Dll -replace '^Devfund_', '' -replace '_WLK_.*$', '' -replace '\.dll$', '' }
    $label = "$name ($Target)"
    $number = $script:Results.Count + 1
    $log = Join-Path $LogDir ('{0:D2}-{1}-{2}.txt' -f $number, $Target, ($name -replace '[^A-Za-z0-9]+', '-').Trim('-'))
    $arguments = @(('"' + (Join-Path $Kit "tests\$Dll") + '"'), "/name:$Test", "/p:DQ=DeviceID='$InstanceId'",
        ('/RebootStateFile:"' + (Join-Path $LogDir ('{0:D2}-reboot-state.xml' -f $number)) + '"'))
    foreach ($parameter in $Parameters) { $arguments += "/p:$parameter" }
    $started = Get-Date
    $process = Start-Process -FilePath $Te -ArgumentList $arguments -RedirectStandardOutput $log `
        -RedirectStandardError "$log.err" -PassThru -WindowStyle Hidden
    $null = $process.Handle
    $timedOut = -not $process.WaitForExit($Minutes * 60 * 1000)
    if ($timedOut) {
        # TE.exe first, so that it cannot report the stopped test as anything but stopped.
        Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue
        Get-Process -Name 'TE.ProcessHost' -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
        Start-Sleep -Seconds 2
    }
    # Shared: a process TAEF started may still hold the file.
    $stream = [System.IO.File]::Open($log, 'Open', 'Read', 'ReadWrite')
    try { $text = [System.IO.StreamReader]::new($stream).ReadToEnd() } finally { $stream.Dispose() }
    $summary = ($text -split "`r?`n") | Where-Object { $_ -match '^Summary:' } | Select-Object -Last 1
    $counts = @{}
    if ($summary) {
        foreach ($match in [regex]::Matches($summary, '([A-Za-z ]+)=(\d+)')) {
            $counts[$match.Groups[1].Value.Trim()] = [int]$match.Groups[2].Value
        }
    }
    $count = { param($key) if ($counts.ContainsKey($key)) { $counts[$key] } else { 0 } }
    $total = & $count 'Total'
    $passed = & $count 'Passed'
    $failed = & $count 'Failed'
    $blocked = & $count 'Blocked'
    $notRun = & $count 'Not Run'
    $skipped = & $count 'Skipped'
    # The tests pass a run that found no device to test ("No devices were found for testing").
    # WDTF's query finds the host interface (Assert-Target), but the tests' own selection leaves
    # it out, presumably as a raw device without a function driver: there, that is a SKIP;
    # anywhere else, a FAIL.
    $nothing = $text -match 'No devices were found for testing'
    $verdict = if ($timedOut -or -not $summary -or $total -eq 0) {
        'FAIL'
    } elseif ($failed -gt 0 -or $blocked -gt 0 -or $notRun -gt 0) {
        'FAIL'
    } elseif ($nothing) {
        if ($NothingIsSkip) { 'SKIP' } else { 'FAIL' }
    } elseif ($passed -gt 0 -and $passed + $skipped -eq $total -and $process.ExitCode -eq 0) {
        'PASS'
    } elseif ($passed -eq 0 -and $skipped -eq $total) {
        'SKIP'
    } else {
        'FAIL'
    }
    $detail = if ($timedOut) {
        "stopped after $Minutes minutes"
    } elseif ($nothing) {
        'the test found no device to test (and passed it all the same)'
    } elseif ($summary) {
        ($summary.Trim() -replace '^Summary:\s*', '') + ", TE exit $($process.ExitCode)"
    } else {
        "no TAEF summary (TE exit $($process.ExitCode))"
    }
    $elapsed = [int]((Get-Date) - $started).TotalSeconds
    $line = '{0} {1,-9} {2,5} s  {3}: {4} (log {5})' -f $verdict, $Group, $elapsed, $label, $detail, (Split-Path $log -Leaf)
    Add-Content -Path (Join-Path $LogDir 'summary.txt') -Value $line
    $line
    $script:Results += [pscustomobject]@{ Verdict = $verdict }
}

# WDTF's own query must find the device a test is given, or every test on it would report that
# it found nothing to test.
$wdtf = New-Object -ComObject WDTF2.WDTF
function Assert-Target([string]$InstanceId) {
    $found = $wdtf.DeviceDepot.Query("DeviceID='$InstanceId'").Count
    if ($found -ne 1) { throw "WDTF finds $found device(s) for DeviceID='$InstanceId'" }
}

"devfund    $(Get-Date -Format 'yyyy-MM-dd HH:mm:ss'), adapter $($adapter.InstanceId), host interface $((Get-HostInterface).InstanceId)"
"checks     driver verifier now: $(((verifier.exe /query | Out-String) -split "`r?`n" | Where-Object { $_ -match 'Verifier Flags' } | Select-Object -First 1))"
"logs       $LogDir"
''

$pnp = 'PNPDisableAndEnableDevice', 'PNPRemoveAndRestartDevice', 'PNPCancelRemoveDevice', 'PNPCancelStopDevice',
'PNPTryStopAndRestartDevice', 'PNPTryStopDeviceAndFailRestart', 'PNPSurpriseRemoveAndRestartDevice'

try {
    foreach ($group in $Groups) {
        foreach ($target in 'adapter', 'interface') {
            # The PnP tests remove and restart the devices: the host interface is looked up again.
            $id = if ($target -eq 'interface') { Wait-HostInterface } else { $adapter.InstanceId }
            Assert-Target $id
            $skipNothing = $target -eq 'interface'
            switch ($group) {
                'pnp' {
                    foreach ($test in $pnp) {
                        Invoke-DevFund 'pnp' 'Devfund_PnPDTest_WLK_Functional.dll' "PNPDTest::$test*" $target $id -NothingIsSkip:$skipNothing
                        if ($target -eq 'interface') { $id = Wait-HostInterface }
                    }
                    # The PnP tests add Microsoft's test filter (msdmfilt.sys) to the device: removed.
                    Invoke-DevFund 'pnp' 'Devfund_PnPDTest_WLK_Functional.dll' 'PNPDTest::DisableEnhancedDeviceTestingSupport' $target $id -NothingIsSkip:$skipNothing
                }
                'fuzz' {
                    # The IOCTL fuzzer tries every function code (0 to 4095) on every surface in
                    # several modes: hours. Its range is the device's own codes and their
                    # neighbours: Kernel Streaming's on the adapter (IOCTL_KS_PROPERTY is 0 ...
                    # RESET_STATE 6), hostmic's on the host interface (0x900 to 0x905, hostmic-abi).
                    $range = if ($target -eq 'adapter') { @('MinFunctionCode=0', 'MaxFunctionCode=15') } else { @('MinFunctionCode=2288', 'MaxFunctionCode=2335') }
                    Invoke-DevFund 'fuzz' 'Devfund_FuzzIoctlTest_WLK_Certification.dll' 'CFuzz::*' $target $id $range 45 -NothingIsSkip:$skipNothing
                    Invoke-DevFund 'fuzz' 'Devfund_FuzzTest_WLK_Certification.dll' 'CFuzz::*' $target $id -NothingIsSkip:$skipNothing
                }
                'reinstall' {
                    if ($target -eq 'adapter') {
                        Invoke-DevFund 'reinstall' 'Devfund_Reinstall_With_IO_BeforeAndAfter_wlk_functional.dll' 'Reinstall_With_IO_BeforeAndAfter::*' $target $id
                    }
                }
            }
        }
    }
} catch {
    "FAIL       the run stopped: $($_.Exception.Message)"
    $script:Results += [pscustomobject]@{ Verdict = 'FAIL' }
} finally {
    # Whatever happened: no test process left, the driver removed (with its device goes the test
    # filter a PnP test may have left on it), TAEF's service stopped.
    Get-Process -Name 'TE', 'TE.ProcessHost' -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    $remove = Invoke-Devcon @('remove', 'root\hostmic')
    $state = sc.exe query hostmic | Select-String 'STATE'
    $state = if ($state) { ($state.Line.Trim() -split '\s+')[-1] } else { 'NONE' }
    ''
    "remove     $(($remove -split "`r?`n" | Where-Object { $_.Trim() } | Select-Object -Last 1)) driver $state"
    if ($remove -match 'reboot') { 'WARNING    the removal waits for a restart: a handle stayed open' }
    Stop-Service -Name 'TE.Service' -ErrorAction SilentlyContinue
}

$failed = @($script:Results | Where-Object { $_.Verdict -eq 'FAIL' }).Count
$skipped = @($script:Results | Where-Object { $_.Verdict -eq 'SKIP' }).Count
$passed = @($script:Results | Where-Object { $_.Verdict -eq 'PASS' }).Count
if ($failed -eq 0 -and $passed -eq 0) {
    # Nothing ran: that is not a pass.
    "NONE       $($script:Results.Count) test(s), all skipped: see their logs"
    exit 254
} elseif ($failed -eq 0) {
    "PASS       $passed of $($script:Results.Count) test(s), $skipped skipped"
} else {
    "FAIL       $failed of $($script:Results.Count) test(s), $skipped skipped"
}
exit $failed
