# Test VM

The driver is only ever loaded in a disposable virtual machine, never on an everyday machine:
test signing needs Secure Boot off, which also breaks games protected by anti-cheats.

This is the procedure used since milestone M1 (2026-11-08), on a Windows 11 Pro host with
Hyper-V enabled.

## 1. Create the VM (host, elevated PowerShell)

Windows 11 setup requires Secure Boot and a TPM, so the VM is installed with both, and Secure
Boot is turned off afterwards.

```powershell
$vm  = 'hostmic-test'
$dir = 'C:\HyperV'
$iso = 'C:\path\to\Windows11.iso'   # x64 ISO from microsoft.com/software-download/windows11
New-VM -Name $vm -Generation 2 -MemoryStartupBytes 8GB -SwitchName 'Default Switch' -Path $dir -NewVHDPath "$dir\$vm\$vm.vhdx" -NewVHDSizeBytes 80GB
Set-VMMemory $vm -DynamicMemoryEnabled $false
Set-VMProcessor $vm -Count 4
Set-VMKeyProtector $vm -NewLocalKeyProtector
Enable-VMTPM $vm
Add-VMDvdDrive $vm -Path $iso
Set-VMFirmware $vm -EnableSecureBoot On -SecureBootTemplate MicrosoftWindows -FirstBootDevice (Get-VMDvdDrive $vm)
Set-VM $vm -CheckpointType Standard -AutomaticCheckpointsEnabled $false
Get-VMIntegrationService $vm | Where-Object Name -match 'Guest Service' | Enable-VMIntegrationService
Start-VM $vm; vmconnect localhost $vm
```

Press a key at "Press any key to boot from CD or DVD", install Windows 11 Pro, and do not press
anything at that prompt after the first restart.

Notes:

- `vmconnect` started from an elevated shell runs elevated, and Windows then blocks input from
  non-elevated tools (UI automation). Members of the local *Hyper-V Administrators* group
  (SID `S-1-5-32-578`) can run it without elevation; that membership lets any program of the
  user drive Hyper-V without a UAC prompt, so remove it when no longer needed.
- Enhanced session goes through Remote Desktop, which does not accept a Windows Hello PIN: with
  a PIN-only account, untick *View → Enhanced session*. In basic mode, *Clipboard → Type
  clipboard text* types text from the host into the VM.

## 2. Turn Secure Boot off and test signing on

In the VM, check BitLocker first: changing Secure Boot can trigger a recovery-key prompt.

```powershell
manage-bde -status C:   # if protection is on: manage-bde -off C:, and wait for "Fully Decrypted"
```

On the host:

```powershell
Stop-VM hostmic-test
Set-VMFirmware hostmic-test -EnableSecureBoot Off
Start-VM hostmic-test
```

In the VM, elevated:

```powershell
bcdedit /set testsigning on
# DbgPrint output is filtered by default; without this, DebugView shows nothing.
reg add "HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Debug Print Filter" /v DEFAULT /t REG_DWORD /d 0xf /f
shutdown /r /t 0
```

After the restart, the desktop shows "Test Mode". Copy the package and `devcon` in, then take
a checkpoint (host):

```powershell
$src = 'target\debug\hostmic_driver_package'
foreach ($f in 'hostmic_driver.sys','hostmic_driver.inf','hostmic_driver.cat','WDRLocalTestCert.cer') {
  Copy-VMFile hostmic-test -SourcePath "$src\$f" -DestinationPath "C:\hostmic\$f" -FileSource Host -CreateFullPath -Force
}
Copy-VMFile hostmic-test -SourcePath 'C:\Program Files (x86)\Windows Kits\10\Tools\10.0.26100.0\x64\devcon.exe' -DestinationPath 'C:\hostmic\devcon.exe' -FileSource Host -CreateFullPath -Force
Checkpoint-VM hostmic-test -SnapshotName 'clean-testsigning'
```

## 3. Install the driver and check it (VM, elevated)

Sections 3 to 7 are the manual procedures; the test campaign (section 8) runs all of them and
prints one verdict per scenario. Copy the freshly built package into the VM (host), in a folder of its own per test so that
older packages are not mixed in. The `driver` CI job also publishes the test-signed debug and
release packages of every run as an artifact (`hostmic-driver-<commit>`, kept 14 days):
`gh run download <run id> --repo Nopitch/hostmic` fetches them without a local build.

```powershell
$src = 'target\debug\hostmic_driver_package'
foreach ($f in 'hostmic_driver.sys','hostmic_driver.inf','hostmic_driver.cat','WDRLocalTestCert.cer') {
  Copy-VMFile hostmic-test -SourcePath "$src\$f" -DestinationPath "C:\hostmic\pkg\$f" -FileSource Host -CreateFullPath -Force
}
```

Trust the test certificate, in the VM only (`-f` creates the `TrustedPublisher` store, absent
on a fresh install):

```powershell
certutil -addstore Root C:\hostmic\pkg\WDRLocalTestCert.cer
certutil -f -addstore TrustedPublisher C:\hostmic\pkg\WDRLocalTestCert.cer
```

Then install and check:

```powershell
C:\hostmic\devcon.exe install C:\hostmic\pkg\hostmic_driver.inf root\hostmic
Get-PnpDevice -FriendlyName hostmic | Format-List FriendlyName,Status,Class,InstanceId,Problem
sc.exe query hostmic
Get-PnpDevice -Class AudioEndpoint | Format-Table FriendlyName,Status
```

Expected: `hostmic`, status `OK`, class `MEDIA`, instance `ROOT\MEDIA\0000`, problem
`CM_PROB_NONE`, service `RUNNING`, and an audio endpoint `Microphone (hostmic)`, status `OK`.
`pnputil /enum-interfaces /enabled` lists the filters' enabled KS interfaces (`TopologyMic`:
audio and topology; `WaveMic`: audio, capture and real time).

## 4. Record the microphone (VM)

`tools/capture-check` records the endpoint through shared-mode WASAPI and checks the tone.
Build it on the host (`cargo build -p hostmic-capture-check --release`), copy
`target\release\capture-check.exe` to `C:\hostmic\`, then in the VM:

```powershell
$h = Start-Process C:\hostmic\test-host.exe -ArgumentList '--seconds','70' -PassThru -WindowStyle Hidden
C:\hostmic\capture-check.exe --seconds 60
```

From milestone M4 the microphone is unplugged without a host and the driver produces no
audio: `tools/test-host` (section 6) plays the tone and runs in the background while
recording. Expected: 1000.00 Hz, peak -6.02 dBFS, RMS -9.03 dBFS, identical channels, no
discontinuity flag, no silent packet, no glitch, `PASS`.

## 5. Read the driver's messages (VM, elevated)

The driver logs its non-real-time events (`hostmic: …`: start, streams, state changes,
interfaces it was asked for and does not have) with `DbgPrint`.
[DebugView](https://learn.microsoft.com/sysinternals/downloads/debugview)'s command-line
version captures them; run it in the foreground (redirected, it writes only its banner) while
a background job reinstalls the device:

```powershell
$j = Start-Job { Start-Sleep 8; C:\hostmic\devcon.exe remove root\hostmic; Start-Sleep 3; C:\hostmic\devcon.exe install C:\hostmic\pkg\hostmic_driver.inf root\hostmic }
C:\hostmic\dbgviewcli64.exe --accepteula -k -v --no-banner -i *hostmic* --duration 35
```

## 6. Check the host interface (VM, elevated)

From milestone M4 the adapter has a child device, "hostmic host interface"
(`HOSTMIC\Host\…`), which carries the host interface `{B2BFAFAE-4F0A-4295-B39B-E6B9F62EE08A}`.
Build `tools/hostmic-ctl` on the host (`cargo build -p hostmic-ctl --release`), copy
`target\release\hostmic-ctl.exe` to `C:\hostmic\`, then in the VM:

```powershell
Get-PnpDevice -PresentOnly | Where-Object InstanceId -like HOSTMIC*
pnputil /enum-interfaces /class '{B2BFAFAE-4F0A-4295-B39B-E6B9F62EE08A}'
C:\hostmic\hostmic-ctl.exe info
```

Expected: the device `OK` (no setup class: it is a raw device, started without a driver); the
interface `Enabled`; `hostmic-ctl info` prints its path, ABI version 1, a maximum buffer of
192000 bytes, 48000 Hz, 2 ch, int16, and exits with 0.

The interface is open to the system, administrators and interactive users only. To check that
anyone else is refused, run the tool as `NETWORK SERVICE` through a scheduled task:

```powershell
$a = New-ScheduledTaskAction -Execute cmd.exe -Argument '/c C:\hostmic\hostmic-ctl.exe info > C:\hostmic\ns.txt 2>&1'
Register-ScheduledTask -TaskName hmicns -Action $a -User 'NT AUTHORITY\NETWORK SERVICE' -Force
Start-ScheduledTask hmicns; Start-Sleep 4; Get-Content C:\hostmic\ns.txt
Unregister-ScheduledTask hmicns -Confirm:$false
```

Expected: the interface is listed, and opening it fails with `Access is denied.
(0x80070005)`. Disabling the child device (`Disable-PnpDevice`) makes the interface disappear
(`hostmic-ctl info` exits with 1); enabling it again makes it answer again.

`tools/test-host` plays a 1 kHz tone at -6.02 dBFS with the `hostmic` crate's reference loop
following the microphone (`hostmic::Player::follow`): it attaches as soon as the microphone is
there, lets it go when Windows asks to remove it, and attaches again when it comes back. While
the stream runs, a thread registered with MMCSS writes the frames due by the performance counter
into the shared buffer, about a millisecond per `ADVANCE`. It prints a `host` line whenever the
attachment or the stream's state changed, the player's counters once a second, and at the end
the driver's counters. Build it on the host (`cargo build -p hostmic-test-host --release`), copy
`target\release\test-host.exe` to `C:\hostmic\`, then in the VM run it in the background while
recording:

```powershell
$p = Start-Process C:\hostmic\test-host.exe -ArgumentList '--seconds','45' -RedirectStandardOutput C:\hostmic\h.txt -PassThru -WindowStyle Hidden
Start-Sleep 2
C:\hostmic\test-host.exe --seconds 2
$c = Start-Job { C:\hostmic\capture-check.exe --seconds 30 }
Start-Sleep 12; C:\hostmic\hostmic-ctl.exe status
Receive-Job $c -Wait; $p | Wait-Process; Get-Content C:\hostmic\h.txt
```

Expected: the second `test-host` waits (`waiting for the microphone`: one host at a time) and
ends `not attached at the end`. `capture-check` passes as in section 4. `hostmic-ctl status`
shows the stream `running` with its buffer (960 frames, packets of 480 in shared mode), its
position, and the counters: frames advanced, packets completed, none skipped, no request
refused. The first host prints `host attached` lines (`stopped`, `running`, `stopped`…),
`playing` lines with `0 late`, a longest gap of a few milliseconds and no `(no MMCSS)`, the
driver's counters, then `detached`. A nonzero `late` means the host was not scheduled for more
than a buffer (20 ms in shared mode): the engine loses those frames, and `capture-check`
reports a discontinuity. A host killed with `Stop-Process` is detached when its handle closes:
the next `test-host` attaches.

The host lets the microphone go when Windows asks to remove it. While `test-host` plays:

```powershell
C:\hostmic\devcon.exe remove root\hostmic; sc.exe query hostmic
C:\hostmic\devcon.exe install C:\hostmic\pkg\hostmic_driver.inf root\hostmic
$i = Get-PnpDevice -FriendlyName 'hostmic host interface' -PresentOnly
Disable-PnpDevice -InstanceId $i.InstanceId -Confirm:$false; Enable-PnpDevice -InstanceId $i.InstanceId -Confirm:$false
```

Expected: `devcon remove` removes the device and the driver unloads (`STOPPED`), the host
printing `waiting for the microphone`; after the reinstall it attaches again and
`capture-check` passes; disabling and enabling the host interface makes it let go and attach
again (`attachments` and `releases` grow by one each time).

`tools/measure` measures the microphone as a communication application sees it (build
`cargo build -p hostmic-measure --release`, copy `target\release\measure.exe`): it plays with
the crate's reference loop and records in the same process. No other host may be attached.

```powershell
C:\hostmic\measure.exe latency --seconds 30
C:\hostmic\measure.exe load --seconds 30 --threads 16
C:\hostmic\measure.exe load --seconds 1800 --threads 16
```

Expected: `latency` reads every click due (a click every half second) with a write → read
median near 10 ms (the engine's period); `load` counts 0 crackle with 16 spinning threads,
over 30 s and over the long endurance run; both report the player's gaps by bucket, mostly
below 2 ms, and end with `PASS`.

The driver's handle rules are checked with `tools/handle-check` (raw requests: the crate's
`Host` always detaches on drop). Build it (`cargo build -p hostmic-handle-check --release`) and
copy `target\release\handle-check.exe` to `C:\hostmic\`. Closing the host's handle detaches it;
the host's mapping must never outlive it, whoever holds its handle; and it must last while the
host lives:

```powershell
# The host closes its only handle without DETACH: its mapping goes.
C:\hostmic\handle-check.exe --seconds 3 --close
# The host dies, a holder (ping) keeps an inherited handle: the host is detached at its exit.
$p = Start-Process C:\hostmic\handle-check.exe -ArgumentList '--seconds','60','--holder' -RedirectStandardOutput C:\hostmic\a.txt -PassThru -WindowStyle Hidden
Start-Sleep 3; Stop-Process $p -Force; Start-Sleep 2
C:\hostmic\test-host.exe --seconds 1
Get-Process ping | Stop-Process -Force
# The holder closes the last handle while the host lives: the host's mapping goes.
$p = Start-Process C:\hostmic\handle-check.exe -ArgumentList '--seconds','8','--holder','--close-own-handle' -RedirectStandardOutput C:\hostmic\b.txt -PassThru -WindowStyle Hidden
Start-Sleep 3; Get-Process ping | Stop-Process -Force; $p | Wait-Process; Get-Content C:\hostmic\b.txt
# A child that inherits the host's handle sends DETACH: refused, the mapping stays.
C:\hostmic\handle-check.exe --seconds 3 --detach-from-child
```

Expected: `--close` shows the mapping `unmapped` right after the close. After the kill, the
driver logs `the host's process exited with its handle still open elsewhere` and `host
detached`, the microphone is unplugged, and the next `test-host` attaches; `b.txt` shows the
mapping `mapped`, then `unmapped` once the holder is killed, while the host still runs.
`--detach-from-child` prints the child's `DETACH` refused, the mapping `mapped`, then
`detached` (its own `DETACH`). A holder left running keeps a handle to the interface open,
which makes `devcon remove` answer "Removed on reboot" (below).

The microphone's jack follows the host. The endpoint's state is in the registry, under
`HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Capture\{…}` (the key whose
`Properties` name `hostmic`), value `DeviceState`: 8 (unplugged) without a host, 1 (active)
while `test-host` is attached, 8 again once it detaches or is killed; `capture-check` then
finds no active "hostmic" endpoint.

The host interface is fuzzed from user mode with `tools/ioctl-fuzz` (cargo-fuzz covers the
driver's pure validation; this covers the kernel code around it, which Microsoft's Device
Fundamentals fuzzers leave out). Copy `target\release\ioctl-fuzz.exe` to `C:\hostmic\`, with no
other host attached, under Driver Verifier (section 9):

```powershell
C:\hostmic\ioctl-fuzz.exe --seconds 60
```

It prints its seed (`--seed` replays the same choices), then one line per phase: `opens`
(read, write and read-write opens and what each allows; opens with a trailing name or relative
to a handle, refused; a low-integrity process may read, not write), `codes` (every vendor
control code, method and access, and random ones: only the six host interface codes succeed),
`sizes` (the six codes with every size around theirs, invalid buffer addresses, and `ATTACH`
with handle values that are not a usable event), `requests` (read, write, flush, lock,
information requests, and overlapped requests each followed by `CancelIoEx`), `stress` (threads
and processes that open, attach, advance with valid, stale and random positions, detach, hand
handles to short-lived processes and die while attached, while some are killed; every process
must end). No capture stream runs meanwhile, so every `ADVANCE` is refused: the advancing path
itself is the `tone`, `load` and `kills` scenarios',
and `end`: the interface is free. Expected: `PASS`; each `UNEXPECTED` line names what happened.
A bug check or a Driver Verifier stop is the other way it fails.

A host that keeps its handle open (`handle-check`, an old test host, a `Device` kept open by a
tool) still makes a removal wait: the host interface vetoes it while a handle is open, `devcon`
answers "Removed on reboot" and the driver stays loaded until the VM restarts. Close it first.

## 7. Remove the driver (VM, elevated)

```powershell
C:\hostmic\devcon.exe remove root\hostmic
sc.exe query hostmic
```

Expected: `1 device(s) were removed` and the service `STOPPED`: the driver is unloaded, which
also shows that the host interface's child device was deleted (a device object left behind
would keep the driver loaded).

The device is root-enumerated: `pnputil /add-driver` alone stages the package but creates no
device node, hence `devcon`.

Notes for driving the VM window from the host:

- The VM keyboard is US-International: quotes and backticks are dead keys when typed through
  *Clipboard → Type clipboard text*, so commands typed that way avoid them.
- Keep a single Virtual Machine Connection window per VM: a second one offers to switch to an
  enhanced session (a "Connect to …" dialog) and takes the console.

To go back to a clean state (host):

```powershell
Restore-VMCheckpoint -VMName hostmic-test -Name clean-testsigning -Confirm:$false
```

## 8. Run the test campaign

`tools/vm/campaign.ps1` runs sections 3 to 7 in one go (under Driver Verifier: section 9): it installs the package next to it,
runs every scenario, prints one `PASS` or `FAIL` line per scenario (with the check that failed
and the log to read), removes the driver, and exits with the number of failed scenarios. Each
tool's output goes to a numbered file in a `log-<date>` folder next to the script.

On the host, build the release tools and stage a package with `tools/vm/stage.ps1`, which
copies the package, the tools, the WDK's `devcon.exe` and the campaign into
`C:\hostmic\<name>\` in the VM (as a member of Hyper-V Administrators). The package is a
local build or the `driver` CI job's artifact (section 3), debug or release:

```powershell
cargo build --release -p hostmic-test-host -p hostmic-capture-check -p hostmic-measure -p hostmic-handle-check -p hostmic-ioctl-fuzz -p hostmic-ctl
.\tools\vm\stage.ps1 -Package target\debug\hostmic_driver_package -Name debug
```

In the VM, elevated (Windows PowerShell; the command has no quotes, so it can be typed with
*Clipboard → Type clipboard text*):

```powershell
powershell -ExecutionPolicy Bypass -File C:\hostmic\debug\campaign.ps1
```

| Scenario | What passes |
|---|---|
| `install` | the test certificate trusted, `devcon install` succeeds, the host interface answers, the adapter and the host interface device are `OK`, the microphone is unplugged (`DeviceState` 8) |
| `power` | the adapter idles to D3 within 20 s without a host (PortCls's inactivity timer, 5 s, set by the INF; read from `DEVPKEY_Device_PowerData`); a host attaching wakes it, and it idles again; it powers up for a `capture-check`, stays in D0 to the end of the recording, which passes, and idles again after it |
| `tone` | `test-host` plays; a second host is kept waiting; `capture-check` passes; the host ends with 0 late, no packet skipped, `detached`; the microphone is unplugged again |
| `latency` | `measure latency` passes (every click due read, median near 10 ms) |
| `load` | `measure load` with 16 spinning threads passes (0 crackle) |
| `kills` | hosts killed at random moments (`-Kills`, default 10): the microphone is unplugged within 10 s after each; the next host plays and `capture-check` passes |
| `handles` | `handle-check --close` and `--detach-from-child` pass; a dead host whose handle a holder keeps is detached (the next host attaches); the holder's last handle unmaps the living host's buffer |
| `fuzz` | `ioctl-fuzz --seconds` (the recordings' length) prints `PASS`: nothing unexpected, the interface free at the end |
| `removal` | `devcon remove` while a host plays unloads the driver without a reboot; after the reinstall the host attaches again and `capture-check` passes |
| `disable` | the host interface answers again after a disable and enable; with a host, disabling the host interface or the adapter makes it let go, and it attaches again (`capture-check` passes each time) |
| `audiosrv` | the host keeps playing across a restart of the audio service (`capture-check` passes) |
| `uninstall` | `devcon remove` unloads the driver (`STOPPED`), the host interface is gone |

`-Only` runs a subset (in that order; without `install` the driver must already be installed,
without `uninstall` it stays installed), `-Seconds` sets the recordings' length (default 30),
`-Kills` the number of killed hosts. The header line `checks` gives Driver Verifier's settings
for the next boot and whether Memory Integrity runs; the `install` line gives what Driver
Verifier verifies now (its flags and drivers, from `verifier /query`), so that a run's output
says under which conditions it passed.

The endurance run (M6) is the `load` scenario for 4 hours, under Driver Verifier (section 9):

```powershell
powershell -ExecutionPolicy Bypass -File C:\hostmic\debug\campaign.ps1 -Only install,load,uninstall -Seconds 14400
```

Its log folder keeps `measure`'s full output (the engine's and the driver's counters, the
player's gaps by bucket) and `verifier /query` at the end (pool allocations, and none failed).

## 9. Run the campaign under Driver Verifier

`tools/vm/verifier.ps1` (staged with the campaign) sets one of M6's Driver Verifier profiles
for the next boot only (`verifier /bootmode oneboot`), and a kernel memory dump:

| Profile | What Driver Verifier checks |
|---|---|
| `standard` | `hostmic_driver.sys` under rule classes 1, 2, 4, 5, 6, 9, 12, 18 (the standard classes without DMA and WDF verification), 20 (additional DDI compliance), 17 (port/miniport interface checking: PortCls's audio rules) and 26 (code integrity) |
| `stack` | the same classes on `hostmic_driver.sys`, `portcls.sys` and `ks.sys` |
| `low-resources` | randomized low resources simulation (class 3) on `hostmic_driver.sys`: from 8 minutes after the boot, about 6 % of its allocations fail |
| `off`, `status` | Driver Verifier off; the settings for the next boot and what is verified now |

In the VM, elevated, then restart **from Windows**:

```powershell
powershell -ExecutionPolicy Bypass -File C:\hostmic\debug\verifier.ps1 standard
shutdown /r /t 0
```

Do not restart with `Restart-VM` on the host: it resets the VM (a hard reset, its default
`-Type`), and the new settings were then lost, Driver Verifier coming back with an earlier
profile's. After the restart (and the sign-in), run the campaign (section 8); its `install`
line must show the profile's flags and drivers. Under `standard` and `stack` every scenario
passes; under `low-resources` some may fail, but the system must stay up. A bug check restarts
the VM with Driver Verifier off and leaves `C:\Windows\MEMORY.DMP`: copy the WDK's
`Debuggers\x64` folder into the VM and run `kd -z C:\Windows\MEMORY.DMP`, then `!analyze -v`
and `!verifier`.

## 10. Run the Device Fundamentals tests

`tools/vm/devfund.ps1` runs Microsoft's Device Fundamentals tests from the WDK
(`Testing\Tests\Additional Tests\x64\DevFund`) with TAEF, without an HLK controller ("Run
DevFund tests via the command line" on Microsoft Learn), on the adapter (`ROOT\MEDIA\0000`, the
Kernel Streaming filters) and on the host interface (`HOSTMIC\…`). Stage the package with the
test kit once (host):

```powershell
.\tools\vm\stage.ps1 -Package target\debug\hostmic_driver_package -Name df -DevFund
```

`-DevFund` copies TAEF, the WDTF installers and their cabinets, and the tests into
`C:\hostmic\devfund`. In the VM, elevated, preferably under Driver Verifier (section 9):

```powershell
powershell -ExecutionPolicy Bypass -File C:\hostmic\df\devfund.ps1
```

The first run installs WDTF (the content installers before the product's, whose custom actions
install the PnP tests' filter driver from the content's files; the other order fails with
msiexec 1603, "Failed to install IOSPY filter driver") and TAEF's service (TAEF blocks a test
that may restart the machine unless its service can resume it). Then:

| Group | Tests | Device |
|---|---|---|
| `pnp` | PNPDTest: disable and enable, remove and restart, cancelled remove, cancelled stop, stop and restart, failed restart, surprise removal; then the test filter removed (`DisableEnhancedDeviceTestingSupport`) | both (the host interface: `SKIP`, see below) |
| `fuzz` | the IOCTL fuzzer (function codes 0 to 15 on the adapter, Kernel Streaming's; 0x8F0 to 0x91F on the host interface, around hostmic's 0x900 to 0x905) and the API fuzzer (open, close, sub-opens, miscellaneous calls) | both (the host interface: `SKIP`) |
| `reinstall` | the driver reinstalled with I/O before and after | the adapter |

One line per test, from TAEF's own count and exit code: `PASS` when every test ran and passed
(or was skipped) and TE.exe exited with 0; `FAIL` on a failed, blocked or unrun test, no test
at all, a timeout, or no device found; `SKIP` when everything was skipped. The tests leave the
host interface out (WDTF's query finds it, which the script checks first, but the tests' own
selection does not take it, presumably as a raw device without a function driver) while
counting that as a pass: there, "no device found for testing" is a `SKIP`. The
script installs this package's driver first (removing one left installed), and removes it at
the end whatever happened, with any test process left and the test filter. The whole run
takes about an hour (two minutes per PnP test on the adapter). `-Only`
with several groups needs PowerShell rather than `-File`: `Set-ExecutionPolicy -Scope Process
Bypass`, then `& C:\hostmic\df\devfund.ps1 -Only fuzz,reinstall`.

## 11. Memory Integrity (HVCI)

`tools/vm/memory-integrity.ps1` sets Memory Integrity with the registry keys of "Enable memory
integrity" on Microsoft Learn (recommended settings, without UEFI lock) and reports what runs.
In the VM, elevated:

```powershell
powershell -ExecutionPolicy Bypass -File C:\hostmic\debug\memory-integrity.ps1 on
shutdown /r /t 0
```

then, after the restart, `memory-integrity.ps1 status`. Only `memory integrity RUNNING` (from
`Win32_DeviceGuard`) means it runs; the campaign's header says the same. In this VM it does
not: virtualization-based security requires Secure Boot (the only documented settings are
Secure Boot, and Secure Boot with DMA protection), which test signing needs off, and `status`
reports `missing: secure boot`. `memory-integrity.ps1 off` puts the settings back. The run
with Memory Integrity on is M7's, with a signed driver and Secure Boot on; until then, Driver
Verifier's code integrity checks (section 9) and the `driver` CI job's check of the binaries
(`.github/scripts/check-hvci.ps1`: no writable and executable section, 4 KiB alignment, NX
compatible) stand in for it.
