<#
.SYNOPSIS
    hostmic's test campaign: installs the driver package next to this script, runs every
    scenario of docs/test-vm.md, prints one PASS or FAIL line per scenario, then removes the
    driver.

.DESCRIPTION
    Runs in the test VM only, from an elevated Windows PowerShell, in a folder that
    tools/vm/stage.ps1 filled from the host: the driver package (hostmic_driver.sys, .inf,
    .cat, WDRLocalTestCert.cer), devcon.exe and the release tools (test-host, capture-check,
    measure, handle-check, hostmic-ctl).

        powershell -ExecutionPolicy Bypass -File C:\hostmic\<name>\campaign.ps1

    Each tool's output goes to a log folder next to the script (one file per run, numbered);
    a FAIL line names the check that failed and the log to read. The exit code is the number
    of scenarios that failed.

    Scenarios, in order (-Only runs a subset, in the same order; without "install" the driver
    must already be installed, without "uninstall" it stays installed):
      install    trust the test certificate, install, the host interface answers, the
                 microphone is unplugged without a host
      power      the adapter idles to D3 without a stream (with and without a host), stays in
                 D0 through a recording, and idles again after it
      tone       test-host plays, a second host is refused, capture-check passes, the
                 microphone is unplugged again after the host ends
      latency    measure latency
      load       measure load, 16 spinning threads
      kills      hosts killed at random moments: the microphone is unplugged after each, and
                 the next host plays cleanly
      handles    handle-check's rules: a closed handle detaches, a duplicate in another
                 process neither detaches the host nor keeps a dead host's mapping
      fuzz       ioctl-fuzz for -Seconds: opens, control codes, sizes, other requests, then
                 threads and processes that attach, advance, detach and die; nothing
                 unexpected, and the interface free at the end
      removal    devcon remove while a host plays unloads the driver; after the reinstall the
                 host attaches again and plays cleanly
      disable    disabling and enabling the host interface (with and without a host) and the
                 adapter (with a host): the host lets go and attaches again
      audiosrv   restarting the audio service while a host plays
      uninstall  devcon remove unloads the driver

.PARAMETER Only
    The scenarios to run (default: all).

.PARAMETER Seconds
    Length of the capture-check and measure recordings (default 30).

.PARAMETER Kills
    Hosts killed by the "kills" scenario (default 10).
#>
[CmdletBinding()]
param(
    [ValidateSet('install', 'power', 'tone', 'latency', 'load', 'kills', 'handles', 'fuzz', 'removal',
        'disable', 'audiosrv', 'uninstall')]
    [string[]]$Only,
    [ValidateRange(10, 86400)]
    [int]$Seconds = 30,
    [ValidateRange(1, 1000)]
    [int]$Kills = 10
)

# Windows PowerShell 5.1 (the VM's), ASCII only: 5.1 reads a script without a BOM as ANSI.
Set-StrictMode -Version 3
$ErrorActionPreference = 'Stop'

$Dir = $PSScriptRoot
$Scenarios = 'install', 'power', 'tone', 'latency', 'load', 'kills', 'handles', 'fuzz', 'removal',
'disable', 'audiosrv', 'uninstall'
if ($Only) { $Scenarios = $Scenarios | Where-Object { $Only -contains $_ } }
$LogDir = Join-Path $Dir ('log-' + (Get-Date -Format 'yyyyMMdd-HHmmss'))
$null = New-Item -ItemType Directory -Path $LogDir
$script:Sequence = 0
$script:Results = @()

# ---------------------------------------------------------------------------------------------
# Helpers

function Fail([string]$Message) { throw [System.Exception]::new($Message) }

function Check([bool]$Condition, [string]$Message) { if (-not $Condition) { Fail $Message } }

# Polls $Condition every 250 ms for up to $Seconds; returns whether it became true.
function Wait-Until([scriptblock]$Condition, [double]$Seconds) {
    $end = (Get-Date).AddSeconds($Seconds)
    do {
        if (& $Condition) { return $true }
        Start-Sleep -Milliseconds 250
    } while ((Get-Date) -lt $end)
    return [bool](& $Condition)
}

function New-LogPath([string]$Name) {
    $script:Sequence++
    Join-Path $LogDir ('{0:D3}-{1}.txt' -f $script:Sequence, $Name)
}

# Starts a tool in the background, its output (stdout and stderr) to a numbered log.
function Start-Tool([string]$Name, [string[]]$Arguments) {
    $log = New-LogPath $Name
    $process = Start-Process -FilePath (Join-Path $Dir "$Name.exe") -ArgumentList $Arguments `
        -RedirectStandardOutput $log -RedirectStandardError "$log.err" -PassThru -WindowStyle Hidden
    # Windows PowerShell reports no exit code unless the handle was opened before the exit.
    $null = $process.Handle
    [pscustomobject]@{ Name = $Name; Process = $process; Log = $log }
}

# A tool's output so far. The files stay open for writing while it runs (and in the processes
# that inherit its handles, such as handle-check's holder), hence the shared read.
function Read-Tool($Tool) {
    $text = ''
    foreach ($file in $Tool.Log, "$($Tool.Log).err") {
        if (-not (Test-Path $file)) { continue }
        $stream = [System.IO.File]::Open($file, 'Open', 'Read', 'ReadWrite')
        try {
            $text += [System.IO.StreamReader]::new($stream).ReadToEnd()
        } finally {
            $stream.Dispose()
        }
    }
    $text
}

# Waits for a background tool to exit (killing it after $Seconds); returns its exit code
# (-1 when it had to be killed) and its output.
function Wait-Tool($Tool, [int]$Seconds) {
    $code = -1
    if ($Tool.Process.WaitForExit($Seconds * 1000)) {
        $code = $Tool.Process.ExitCode
    } else {
        Stop-Process -Id $Tool.Process.Id -Force -ErrorAction SilentlyContinue
        $null = $Tool.Process.WaitForExit(5000)
    }
    [pscustomobject]@{ Code = $code; Text = (Read-Tool $Tool); Log = $Tool.Log }
}

function Stop-Tool($Tool) {
    Stop-Process -Id $Tool.Process.Id -Force -ErrorAction SilentlyContinue
    $null = $Tool.Process.WaitForExit(5000)
    [pscustomobject]@{ Code = -1; Text = (Read-Tool $Tool); Log = $Tool.Log }
}

function Invoke-Tool([string]$Name, [string[]]$Arguments, [int]$Seconds = 120) {
    Wait-Tool (Start-Tool $Name $Arguments) $Seconds
}

# The last line of $Text that matches $Pattern, trimmed ('' when none).
function Find-Line([string]$Text, [string]$Pattern) {
    $line = ($Text -split "`r?`n") | Where-Object { $_ -match $Pattern } | Select-Object -Last 1
    if ($line) { $line.Trim() } else { '' }
}

function Invoke-Devcon([string[]]$Arguments) {
    $log = New-LogPath 'devcon'
    # Windows PowerShell turns a native program's redirected stderr into errors, which 'Stop'
    # would throw.
    $ErrorActionPreference = 'Continue'
    $text = (& (Join-Path $Dir 'devcon.exe') @Arguments 2>&1 | Out-String)
    Set-Content -Path $log -Value $text
    [pscustomobject]@{ Code = $LASTEXITCODE; Text = $text; Log = $log }
}

# The driver service's state: RUNNING, STOPPED, ... or NONE when it is not registered.
function Get-DriverState {
    $line = sc.exe query hostmic | Select-String 'STATE'
    if ($line) { ($line.Line.Trim() -split '\s+')[-1] } else { 'NONE' }
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

# The microphone endpoint's DeviceState: 1 active (a host is attached), 8 unplugged, 4 not
# present; $null when there is none. Several endpoint keys can name hostmic after
# reinstallations: an active one wins, then an unplugged one.
function Get-EndpointState {
    $root = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Capture'
    $states = @()
    foreach ($key in Get-ChildItem $root -ErrorAction SilentlyContinue) {
        $properties = Get-ItemProperty -Path (Join-Path $key.PSPath 'Properties') -ErrorAction SilentlyContinue
        if (-not $properties) { continue }
        $named = $properties.PSObject.Properties |
            Where-Object { $_.Value -is [string] -and $_.Value -match 'hostmic' }
        if ($named) { $states += [int](Get-ItemProperty -Path $key.PSPath).DeviceState }
    }
    if ($states -contains 1) { return 1 }
    if ($states -contains 8) { return 8 }
    if ($states.Count -gt 0) { return $states[0] }
    return $null
}

# The adapter's most recent device power state, 'D0' to 'D3' ('unknown' without an adapter or
# the property): DEVPKEY_Device_PowerData is a CM_POWER_DATA, whose second ULONG is a
# DEVICE_POWER_STATE (1 for D0 to 4 for D3). Reading it sends the driver no request, so it does
# not wake the device. $InstanceId saves enumerating every device (about 0.5 s) at each read.
function Get-PowerState([string]$InstanceId) {
    if (-not $InstanceId) {
        $adapter = Get-Adapter
        if (-not $adapter) { return 'unknown' }
        $InstanceId = $adapter.InstanceId
    }
    $property = Get-PnpDeviceProperty -InstanceId $InstanceId -KeyName 'DEVPKEY_Device_PowerData' -ErrorAction SilentlyContinue
    if (-not $property -or $property.Data -isnot [byte[]] -or $property.Data.Length -lt 8) { return 'unknown' }
    $state = [System.BitConverter]::ToInt32($property.Data, 4)
    if ($state -ge 1 -and $state -le 4) { "D$($state - 1)" } else { 'unknown' }
}

# Waits for the adapter to idle to D3; returns the seconds it took from the call, or fails.
# With -Woken, it must first be seen out of D3: an adapter that something should have woken,
# and that stayed in D3, must not pass for one that idled again.
function Wait-Idle([string]$InstanceId, [string]$What, [switch]$Woken, [double]$Seconds = 20) {
    $started = Get-Date
    if ($Woken) {
        Check (Wait-Until { (Get-PowerState $InstanceId) -ne 'D3' } 10) "the adapter stayed in D3 $What"
    }
    Check (Wait-Until { (Get-PowerState $InstanceId) -eq 'D3' } $Seconds) "the adapter did not idle to D3 within $Seconds s $What (state $(Get-PowerState $InstanceId))"
    [int]((Get-Date) - $started).TotalSeconds
}

function Wait-Plugged([double]$Seconds = 15) { Wait-Until { (Get-EndpointState) -eq 1 } $Seconds }
function Wait-Unplugged([double]$Seconds = 10) { Wait-Until { (Get-EndpointState) -eq 8 } $Seconds }

function Wait-InterfaceAnswers([double]$Seconds = 30) {
    Wait-Until { (Invoke-Tool 'hostmic-ctl' @('info') 10).Code -eq 0 } $Seconds
}

# Starts a test host and waits until the microphone is active. The endpoint's state changes
# asynchronously: it must first have left "active" from any previous host, or a stale state
# would pass for this host's.
function Start-PlayingHost([int]$Seconds) {
    Check (Wait-Until { (Get-EndpointState) -ne 1 } 10) 'the microphone is still active from a previous host'
    $hostTool = Start-Tool 'test-host' @('--seconds', "$Seconds")
    if (-not (Wait-Plugged 15)) {
        $null = Stop-Tool $hostTool
        Fail "the microphone did not become active with a host (log $(Split-Path $hostTool.Log -Leaf))"
    }
    $hostTool
}

# Records the microphone; fails unless capture-check passes. Returns its summary.
function Assert-Capture([int]$Seconds, [string]$What) {
    $capture = Invoke-Tool 'capture-check' @('--seconds', "$Seconds") ($Seconds + 30)
    $reason = Find-Line $capture.Text '^(engine|glitches|frequency):'
    Check ($capture.Code -eq 0) "capture-check $What`: exit $($capture.Code), $reason (log $(Split-Path $capture.Log -Leaf))"
    $frequency = (Find-Line $capture.Text '^frequency:') -replace '^frequency:\s*(\S+ Hz).*$', '$1'
    "$frequency, $((Find-Line $capture.Text '^glitches:') -replace '^glitches:\s*', '') glitch"
}

# "(N attachments, M releases, ...)" from a test host's last "host" line.
function Get-Attachments([string]$Text) {
    $line = Find-Line $Text '^host\s'
    if ($line -match '\((\d+) attachments, (\d+) releases') {
        return @([int]$Matches[1], [int]$Matches[2])
    }
    return @(0, 0)
}

# Undoes what a failed or aborted scenario may have left behind, so that the next one starts
# clean: running tools (a holder keeps a handle that would hold a removal back) and disabled
# hostmic devices (a disabled host interface keeps its flag in its instance key).
function Clear-Leftovers {
    foreach ($name in 'test-host', 'measure', 'handle-check', 'capture-check', 'ping') {
        Get-Process -Name $name -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    }
    Get-PnpDevice -ErrorAction SilentlyContinue |
        Where-Object { ($_.InstanceId -like 'HOSTMIC\*' -or $_.FriendlyName -eq 'hostmic') -and $_.Problem -eq 'CM_PROB_DISABLED' } |
        ForEach-Object { Enable-PnpDevice -InstanceId $_.InstanceId -Confirm:$false -ErrorAction SilentlyContinue }
    Start-Sleep -Milliseconds 500
}

# devcon exits 0 when done, 1 when a reboot is needed, 2 on a failure.
function Remove-Driver {
    $remove = Invoke-Devcon @('remove', 'root\hostmic')
    $log = Split-Path $remove.Log -Leaf
    Check ($remove.Text -notmatch 'reboot') "devcon remove needs a reboot: a handle stayed open (log $log)"
    # An aborted earlier run can have left more than one root\hostmic device.
    Check ($remove.Code -eq 0 -and $remove.Text -match '[1-9]\d* device\(s\) were removed') "devcon remove exit $($remove.Code) (log $log)"
    Check ($null -eq (Get-Adapter)) 'the adapter is still present after devcon remove'
    Check (Wait-Until { (Get-DriverState) -ne 'RUNNING' } 15) "the driver did not unload after devcon remove (state $(Get-DriverState))"
}

# Fails unless a host that Start-PlayingHost started is still running: Stop-Tool would hide a
# host that died on its own.
function Assert-Alive($Tool, [string]$What) {
    Check (-not $Tool.Process.HasExited) "the host exited by itself $What (exit $($Tool.Process.ExitCode), log $(Split-Path $Tool.Log -Leaf))"
}

function Install-Driver {
    $install = Invoke-Devcon @('install', (Join-Path $Dir 'hostmic_driver.inf'), 'root\hostmic')
    Check ($install.Text -match 'successfully') "devcon install failed (log $(Split-Path $install.Log -Leaf))"
    # A host interface disabled before an earlier removal comes back disabled.
    Start-Sleep -Seconds 2
    Get-PnpDevice -PresentOnly -ErrorAction SilentlyContinue |
        Where-Object { $_.InstanceId -like 'HOSTMIC\*' -and $_.Problem -eq 'CM_PROB_DISABLED' } |
        ForEach-Object { Enable-PnpDevice -InstanceId $_.InstanceId -Confirm:$false }
    Check (Wait-InterfaceAnswers 30) 'the host interface did not answer within 30 s of the install'
}

# ---------------------------------------------------------------------------------------------
# Scenarios: each returns its summary or throws its reason.

function Test-Install {
    $cer = Join-Path $Dir 'WDRLocalTestCert.cer'
    $null = certutil -addstore Root $cer
    $null = certutil -f -addstore TrustedPublisher $cer
    if (Get-Adapter) { Remove-Driver }
    Install-Driver
    $adapter = Get-Adapter
    Check ($null -ne $adapter -and $adapter.Status -eq 'OK') 'the adapter is not OK after the install'
    $interface = Get-HostInterface
    Check ($null -ne $interface -and $interface.Status -eq 'OK') 'the host interface device is not OK'
    Check (Wait-Unplugged 20) "the microphone is not unplugged without a host (DeviceState $(Get-EndpointState))"
    # What Driver Verifier verifies now, with the driver loaded: its settings may be pending a
    # restart.
    $query = (verifier.exe /query | Out-String)
    Set-Content -Path (New-LogPath 'verifier-query') -Value $query
    # The flags in effect and the drivers verified, as they are, not as they were set.
    $flags = if ($query -match 'Verifier Flags:\s*(0x[0-9a-fA-F]+)') { $Matches[1] } else { 'none' }
    $verifiedDrivers = @([regex]::Matches($query, '[A-Za-z0-9_]+\.sys') | ForEach-Object Value | Sort-Object -Unique)
    $now = if ($verifiedDrivers -contains 'hostmic_driver.sys') {
        "verified now: flags $flags, $($verifiedDrivers -join ' ')"
    } else {
        'not verified now'
    }
    "driver $(Get-DriverState), $($adapter.InstanceId), host interface OK, microphone unplugged, $now"
}

# PortCls's inactivity timer (the INF's PowerSettings, 5 s): the adapter idles to D3 without a
# stream, with or without a host; a recording powers it up and keeps it in D0 to the end; it
# idles again afterwards. The time-out runs from the last access, so the times reported (from
# each wait's start) can be shorter than its 5 s.
function Test-Power {
    Check (Wait-Until { (Get-EndpointState) -ne 1 } 10) 'the microphone is still active from a previous host'
    $adapter = Get-Adapter
    Check ($null -ne $adapter) 'no adapter'
    $id = $adapter.InstanceId
    $alone = Wait-Idle $id 'without a host'
    $player = Start-PlayingHost 240
    # Attaching plugs the microphone in: the audio service's requests wake the adapter.
    $attached = Wait-Idle $id 'with a host attached and no stream' -Woken

    $seconds = 15
    $capture = Start-Tool 'capture-check' @('--seconds', "$seconds")
    # The stream is open once the adapter has left D3; from then on it must stay in D0 until
    # the recording ends. The samples only see the most recent state: capture-check's glitch
    # count covers a power-down between two of them.
    Check (Wait-Until { (Get-PowerState $id) -eq 'D0' } 10) "the adapter did not power up for a recording (state $(Get-PowerState $id))"
    $samples = 0
    $states = @()
    while (-not $capture.Process.HasExited -and $samples -lt 4 * ($seconds + 10)) {
        $state = Get-PowerState $id
        $samples++
        if ($state -ne 'D0' -and -not $capture.Process.HasExited) { $states += $state }
        Start-Sleep -Milliseconds 250
    }
    $recorded = Wait-Tool $capture 60
    $reason = Find-Line $recorded.Text '^(engine|glitches|frequency):'
    Check ($recorded.Code -eq 0) "capture-check after D3: exit $($recorded.Code), $reason (log $(Split-Path $recorded.Log -Leaf))"
    Check ($states.Count -eq 0) "the adapter left D0 during the recording: $($states -join ' ') in $samples samples"
    $frequency = (Find-Line $recorded.Text '^frequency:') -replace '^frequency:\s*(\S+ Hz).*$', '$1'
    $glitches = (Find-Line $recorded.Text '^glitches:') -replace '^glitches:\s*', ''

    $after = Wait-Idle $id 'after the recording, with a host attached'
    Assert-Alive $player 'across the power transitions'
    $played = Stop-Tool $player
    Check ($played.Text -notmatch '(?m)^error') "the host stopped on an error (log $(Split-Path $played.Log -Leaf))"
    "D3 within $alone s without a host, woken and D3 again within $attached s with one; D0 through a $seconds s recording ($samples samples): $frequency, $glitches glitch; D3 again within $after s"
}

function Test-Tone {
    $first = Start-PlayingHost ($Seconds + 40)
    $second = Invoke-Tool 'test-host' @('--seconds', '3') 30
    Check ($second.Code -eq 0 -and $second.Text -match 'not attached at the end') "a second host was not kept waiting (log $(Split-Path $second.Log -Leaf))"
    $capture = Assert-Capture $Seconds 'with test-host'
    Assert-Alive $first 'during the recording'
    $played = Wait-Tool $first ($Seconds + 60)
    $log = Split-Path $played.Log -Leaf
    Check ($played.Code -eq 0) "test-host exit $($played.Code) (log $log)"
    Check ($played.Text -match '(?m)^detached') "test-host did not detach at the end (log $log)"
    $playing = Find-Line $played.Text '^playing'
    Check ($playing -match ' 0 late') "test-host was late: $playing"
    $driver = Find-Line $played.Text '^driver'
    Check ($driver -match ' 0 skipped') "packets skipped: $driver"
    Check (Wait-Unplugged 10) 'the microphone stayed active after the host ended'
    "$capture; second host kept waiting; 0 late, $($playing -replace '^.*(longest gap.*)$', '$1')"
}

function Test-Measure([string]$Mode) {
    Check (Wait-Until { (Get-EndpointState) -ne 1 } 10) 'the microphone is still active from a previous host'
    $arguments = @($Mode, '--seconds', "$Seconds")
    if ($Mode -eq 'load') { $arguments += @('--threads', '16') }
    $run = Invoke-Tool 'measure' $arguments ($Seconds + 60)
    $result = Find-Line $run.Text "^$Mode\s"
    Check ($run.Code -eq 0) "measure $Mode exit $($run.Code): $result $(Find-Line $run.Text '^(engine|player     ended)') (log $(Split-Path $run.Log -Leaf))"
    # The gaps' distribution stays in the log.
    $result -replace "^$Mode\s+", ''
}

function Test-Kills {
    $random = [System.Random]::new()
    for ($i = 1; $i -le $Kills; $i++) {
        $victim = Start-PlayingHost 120
        Start-Sleep -Milliseconds ($random.Next(100, 3000))
        Assert-Alive $victim "before kill $i"
        $null = Stop-Tool $victim
        Check (Wait-Unplugged 10) "the microphone stayed active after killing host $i of $Kills"
    }
    $next = Start-PlayingHost 30
    $capture = Assert-Capture 10 "after $Kills killed hosts"
    $played = Wait-Tool $next 60
    Check ($played.Code -eq 0 -and $played.Text -match '(?m)^detached') "the host after the kills failed (log $(Split-Path $played.Log -Leaf))"
    "$Kills hosts killed, microphone unplugged after each; next host: $capture"
}

function Test-Handles {
    $close = Invoke-Tool 'handle-check' @('--seconds', '3', '--close') 30
    Check ($close.Code -eq 0) "--close: $(Find-Line $close.Text '^(error|mapping)') (log $(Split-Path $close.Log -Leaf))"

    $child = Invoke-Tool 'handle-check' @('--seconds', '3', '--detach-from-child') 30
    Check ($child.Code -eq 0) "--detach-from-child: $(Find-Line $child.Text '^(error|child|mapping)') (log $(Split-Path $child.Log -Leaf))"

    # The host dies while a holder keeps its handle: the driver detaches it at its exit, and the
    # next host attaches (Player::follow retries every second).
    $holder = Start-Tool 'handle-check' @('--seconds', '60', '--holder')
    Check (Wait-Until { (Read-Tool $holder) -match '(?m)^holder' } 10) "--holder did not start its holder (log $(Split-Path $holder.Log -Leaf))"
    $null = Stop-Tool $holder
    Check ($null -ne (Get-Process -Name ping -ErrorAction SilentlyContinue)) 'the holder (ping) is gone: nothing was checked'
    $next = Invoke-Tool 'test-host' @('--seconds', '5') 30
    Get-Process -Name ping -ErrorAction SilentlyContinue | Stop-Process -Force
    Check ($next.Code -eq 0 -and $next.Text -match '(?m)^detached') "a dead host whose handle a holder keeps was not detached (log $(Split-Path $next.Log -Leaf))"

    # The holder's handle is the last one: the mapping lasts while it lives (the host's own close
    # did not undo it), and killing the holder unmaps the living host's buffer.
    $last = Start-Tool 'handle-check' @('--seconds', '8', '--holder', '--close-own-handle')
    Check (Wait-Until { (Read-Tool $last) -match 'closed its own handle' } 10) "--close-own-handle did not close its handle (log $(Split-Path $last.Log -Leaf))"
    Start-Sleep -Seconds 1
    $before = Find-Line (Read-Tool $last) '^mapping'
    Check ($before -match ': mapped$') "the mapping did not last while the holder's handle was open: $before (log $(Split-Path $last.Log -Leaf))"
    Get-Process -Name ping -ErrorAction SilentlyContinue | Stop-Process -Force
    $watched = Wait-Tool $last 30
    $after = Find-Line $watched.Text '^mapping'
    Check ($watched.Code -eq 0 -and $after -match ': unmapped$') "the mapping outlived the last handle: $after (log $(Split-Path $watched.Log -Leaf))"
    'a closed handle detaches; a child cannot detach the host; a dead host is detached despite a holder; the last handle unmaps'
}

# The host interface fuzzed from user mode (tools/ioctl-fuzz); its seed is in the log.
function Test-Fuzz {
    $fuzz = Invoke-Tool 'ioctl-fuzz' @('--seconds', "$Seconds") ($Seconds + 300)
    # The first unexpected result names the problem; the FAIL line only counts them.
    $first = ($fuzz.Text -split "`r?`n") | Where-Object { $_ -match '^(UNEXPECTED|error)' } | Select-Object -First 1
    Check ($fuzz.Code -eq 0) "$(if ($first) { $first.Trim() } else { Find-Line $fuzz.Text '^FAIL' }) (log $(Split-Path $fuzz.Log -Leaf))"
    $codes = if ((Find-Line $fuzz.Text '^codes') -match '(\d+) requests') { $Matches[1] } else { '?' }
    $stress = if ((Find-Line $fuzz.Text '^stress +\d+ operations') -match '(\d+) operations in (\d+) threads \((\d+) attachments\), (\d+) worker processes \((\d+) killed\)') {
        "$($Matches[1]) operations in $($Matches[2]) threads ($($Matches[3]) attachments) and $($Matches[4]) worker processes ($($Matches[5]) killed)"
    } else { '?' }
    "$codes control codes, sizes, other requests; $stress; the interface free at the end ($(Find-Line $fuzz.Text '^seed'))"
}

function Test-Removal {
    $player = Start-PlayingHost 300
    Remove-Driver
    # Removed, the endpoint is "not present" (4) rather than unplugged (8).
    Check (Wait-Until { (Get-EndpointState) -ne 1 } 10) "the microphone stayed active after the removal (DeviceState $(Get-EndpointState))"
    Install-Driver
    Check (Wait-Plugged 20) 'the host did not attach again after the reinstall'
    $capture = Assert-Capture 10 'after the reinstall'
    Assert-Alive $player 'across the removal'
    $played = Stop-Tool $player
    $counts = Get-Attachments $played.Text
    Check ($counts[0] -ge 2 -and $counts[1] -ge 1) "attachments $($counts[0]), releases $($counts[1]) (log $(Split-Path $played.Log -Leaf))"
    "unloaded while a host played, attached again after the reinstall: $capture"
}

function Test-Disable {
    $disabled = @()
    try {
        # Without a host: the interface answers again after a disable and enable (#52).
        $interface = Get-HostInterface
        Check ($null -ne $interface) 'no host interface device'
        $disabled += $interface.InstanceId
        Disable-PnpDevice -InstanceId $interface.InstanceId -Confirm:$false
        Check ((Invoke-Tool 'hostmic-ctl' @('info') 10).Code -ne 0) 'the host interface answered while disabled'
        Enable-PnpDevice -InstanceId $interface.InstanceId -Confirm:$false
        Check (Wait-InterfaceAnswers 15) 'the host interface did not answer after a disable and enable'

        $player = Start-PlayingHost 300
        $interface = Get-HostInterface
        Check ($null -ne $interface) 'no host interface device'
        $disabled += $interface.InstanceId
        Disable-PnpDevice -InstanceId $interface.InstanceId -Confirm:$false
        Check (Wait-Unplugged 10) 'the microphone stayed active with the host interface disabled'
        Enable-PnpDevice -InstanceId $interface.InstanceId -Confirm:$false
        Check (Wait-Plugged 20) 'the host did not attach again after the host interface came back'
        $first = Assert-Capture 8 'after the host interface came back'

        $adapter = Get-Adapter
        Check ($null -ne $adapter) 'no adapter'
        $disabled += $adapter.InstanceId
        Disable-PnpDevice -InstanceId $adapter.InstanceId -Confirm:$false
        Check (Wait-Until { (Get-DriverState) -ne 'RUNNING' } 15) "the driver did not unload with the adapter disabled (state $(Get-DriverState))"
        Check (Wait-Until { (Get-EndpointState) -ne 1 } 10) 'the microphone stayed active with the adapter disabled'
        Enable-PnpDevice -InstanceId $adapter.InstanceId -Confirm:$false
        Check (Wait-Plugged 30) 'the host did not attach again after the adapter came back'
        $second = Assert-Capture 8 'after the adapter came back'
        Assert-Alive $player 'across the disables'
        $played = Stop-Tool $player
        $counts = Get-Attachments $played.Text
        Check ($counts[0] -ge 3 -and $counts[1] -ge 2) "attachments $($counts[0]), releases $($counts[1]) (log $(Split-Path $played.Log -Leaf))"
        "interface: $first; adapter: $second"
    } finally {
        # A failed check must not leave a device disabled for the next scenarios.
        foreach ($id in $disabled) {
            $device = Get-PnpDevice -InstanceId $id -ErrorAction SilentlyContinue
            if ($device -and $device.Problem -eq 'CM_PROB_DISABLED') {
                Enable-PnpDevice -InstanceId $id -Confirm:$false -ErrorAction SilentlyContinue
            }
        }
    }
}

function Test-AudioService {
    $player = Start-PlayingHost 300
    Restart-Service -Name Audiosrv -Force
    Check (Wait-Until { (Get-Service Audiosrv).Status -eq 'Running' } 30) 'the audio service did not restart'
    Start-Sleep -Seconds 3
    # The endpoint's state stays active across the restart: the recording is the check.
    $capture = Assert-Capture 10 'after the audio service restarted'
    Assert-Alive $player 'across the restart'
    $played = Stop-Tool $player
    Check ($played.Text -notmatch '(?m)^error') "the host stopped on an error (log $(Split-Path $played.Log -Leaf))"
    "the host kept playing across the restart: $capture"
}

function Test-Uninstall {
    Remove-Driver
    Check ((Invoke-Tool 'hostmic-ctl' @('info') 10).Code -ne 0) 'the host interface still answers after the removal'
    "driver $(Get-DriverState)"
}

# ---------------------------------------------------------------------------------------------
# Run

$principal = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
# Exit code 255 for a run that could not start (Write-Error would throw under 'Stop' and exit 1,
# which reads as one failed scenario).
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    [Console]::Error.WriteLine('run campaign.ps1 from an elevated PowerShell')
    exit 255
}
foreach ($file in 'hostmic_driver.sys', 'hostmic_driver.inf', 'hostmic_driver.cat', 'WDRLocalTestCert.cer',
    'devcon.exe', 'test-host.exe', 'capture-check.exe', 'measure.exe', 'handle-check.exe', 'hostmic-ctl.exe') {
    if (-not (Test-Path -LiteralPath (Join-Path $Dir $file))) {
        [Console]::Error.WriteLine("missing $file next to campaign.ps1 (tools/vm/stage.ps1 copies it)")
        exit 255
    }
}

$os = Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion'
$sys = Get-Item (Join-Path $Dir 'hostmic_driver.sys')
$hash = (Get-FileHash $sys.FullName -Algorithm SHA256).Hash.Substring(0, 16).ToLowerInvariant()
# Driver Verifier's settings for the next boot (those of this one unless changed since without
# a restart): its rule classes ("  9  - Security checks.", "  17 - Port/miniport interface
# checking.") and the drivers it verifies, each listed in its own section. Whether it verifies
# the driver now, `verifier /query` says once the driver is loaded (the install scenario).
function Get-VerifierSection([string]$Text, [string]$Title) {
    $inside = $false
    foreach ($line in $Text -split "`r?`n") {
        if ($line -match "^\s*$([regex]::Escape($Title)):\s*$") { $inside = $true; continue }
        if (-not $inside) { continue }
        if ($line -match '^\s*\S[^-]*:\s*$') { break }
        if ($line.Trim()) { $line.Trim() }
    }
}
$verifier = (verifier.exe /querysettings | Out-String)
Set-Content -Path (Join-Path $LogDir '000-verifier.txt') -Value $verifier
$classes = @(Get-VerifierSection $verifier 'Active Rule Classes' | ForEach-Object { if ($_ -match '^(\d+)\s+-\s') { $Matches[1] } })
$drivers = @(Get-VerifierSection $verifier 'Verified Drivers' | Where-Object { $_ -match '\.sys$' })
$verified = if ($drivers -contains 'hostmic_driver.sys') {
    "on, rule classes $($classes -join ' '), for $($drivers -join ' ')"
} else {
    'off for hostmic_driver.sys'
}
$guard = Get-CimInstance -ClassName Win32_DeviceGuard -Namespace root\Microsoft\Windows\DeviceGuard -ErrorAction SilentlyContinue
$integrity = if ($guard -and @($guard.SecurityServicesRunning) -contains 2) { 'running' } else { 'off' }

"campaign   $(Get-Date -Format 'yyyy-MM-dd HH:mm:ss'), $($env:COMPUTERNAME), Windows $($os.DisplayVersion) build $($os.CurrentBuild).$($os.UBR)"
"package    $Dir, hostmic_driver.sys $($sys.Length) bytes, sha256 $hash..."
"checks     driver verifier for the next boot $verified (now: see install); memory integrity $integrity"
"settings   $Seconds s recordings, $Kills kills; logs in $LogDir"
''

# An aborted earlier run can have left hosts, a holder or a disabled device behind.
Clear-Leftovers
$installed = $true
foreach ($name in $Scenarios) {
    $started = Get-Date
    try {
        if (-not $installed -and $name -ne 'uninstall') { Fail 'skipped: the driver is not installed' }
        $detail = switch ($name) {
            'install' { Test-Install }
            'power' { Test-Power }
            'tone' { Test-Tone }
            'latency' { Test-Measure 'latency' }
            'load' { Test-Measure 'load' }
            'kills' { Test-Kills }
            'handles' { Test-Handles }
            'fuzz' { Test-Fuzz }
            'removal' { Test-Removal }
            'disable' { Test-Disable }
            'audiosrv' { Test-AudioService }
            'uninstall' { Test-Uninstall }
        }
        $passed = $true
    } catch {
        $detail = $_.Exception.Message
        $passed = $false
        if ($name -eq 'install') { $installed = $false }
    }
    Clear-Leftovers
    # A scenario that failed between a removal and a reinstall must not fail the next ones.
    # Only when this run installs the driver: with -Only and no "install", the caller installed it.
    if (-not $passed -and $installed -and $Scenarios -contains 'install' -and $name -ne 'uninstall' -and -not (Get-Adapter)) {
        try {
            Install-Driver
            $detail += ' (driver reinstalled for the next scenarios)'
        } catch {
            $installed = $false
            $detail += " (reinstall failed: $($_.Exception.Message))"
        }
    }
    $verdict = if ($passed) { 'PASS' } else { 'FAIL' }
    $elapsed = [int]((Get-Date) - $started).TotalSeconds
    $line = '{0} {1,-9} {2,4} s  {3}' -f $verdict, $name, $elapsed, $detail
    Add-Content -Path (Join-Path $LogDir 'summary.txt') -Value $line
    $line
    $script:Results += [pscustomobject]@{ Name = $name; Passed = $passed }
}

# What Driver Verifier counted for the verified drivers (pool allocations, IRQL raises,
# violations logged without a bug check).
Set-Content -Path (Join-Path $LogDir '999-verifier-query.txt') -Value (verifier.exe /query | Out-String)

$failed = @($script:Results | Where-Object { -not $_.Passed }).Count
''
if ($failed -eq 0) {
    "PASS       $($script:Results.Count) scenario(s)"
} else {
    "FAIL       $failed of $($script:Results.Count) scenario(s): " + (($script:Results | Where-Object { -not $_.Passed } | ForEach-Object Name) -join ', ')
}
exit $failed
