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

Copy the freshly built package into the VM (host), in a folder of its own per test so that
older packages are not mixed in:

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

`tools/test-host` attaches as the host and plays a 1 kHz tone at -6.02 dBFS: while the stream
runs, a thread registered with MMCSS writes the frames due by the performance counter into the
shared buffer and advances the position (`ADVANCE`) about once a millisecond. It prints the
stream's state at each change, its progress once a second, and the driver's counters at the
end. Build it on the host (`cargo build -p hostmic-test-host --release`), copy
`target\release\test-host.exe` to `C:\hostmic\`, then in the VM run it in the background while
recording:

```powershell
$p = Start-Process C:\hostmic\test-host.exe -ArgumentList '--seconds','45' -RedirectStandardOutput C:\hostmic\h.txt -PassThru -WindowStyle Hidden
Start-Sleep 2
C:\hostmic\test-host.exe --seconds 1
$c = Start-Job { C:\hostmic\capture-check.exe --seconds 30 }
Start-Sleep 12; C:\hostmic\hostmic-ctl.exe status
Receive-Job $c -Wait; $p | Wait-Process; Get-Content C:\hostmic\h.txt
```

Expected: the second `test-host` fails with `ATTACH: The requested resource is in use.
(0x800700AA)` (one host at a time). `capture-check` passes as in section 4.
`hostmic-ctl status` shows the stream `running` with its buffer (960 frames, packets of 480 in
shared mode), its position, and the counters: frames advanced, packets completed, none
skipped, no request refused. The first host prints `attached` with the address of the shared
buffer in its process (page-aligned, 192000 bytes), `thread MMCSS Pro Audio`, one line per
change with a growing generation (`stopped`, `paused`, `running`, `paused`, `stopped`, `no
stream`), `playing` lines with `0 late` and a longest gap of a few milliseconds, the driver's
counters, then `detached`. A `late` line means the host was not scheduled for more than a
buffer (20 ms in shared mode): the engine loses those frames, and `capture-check` reports a
discontinuity. A host killed with `Stop-Process`, or one that exits without detaching
(`--no-detach`), is detached when its handle closes: the next `test-host` attaches.

The host's mapping must never outlive it, whoever holds its handle. `--holder` starts a process
(`ping`) that inherits the host's handle; `--close-own-handle` also closes the host's own:

```powershell
# The host dies, the holder keeps the handle: the host is detached at its exit.
$p = Start-Process C:\hostmic\test-host.exe -ArgumentList '--seconds','60','--holder' -RedirectStandardOutput C:\hostmic\a.txt -PassThru -WindowStyle Hidden
Start-Sleep 3; Stop-Process $p -Force; Start-Sleep 2
C:\hostmic\test-host.exe --seconds 1
Get-Process ping | Stop-Process -Force
# The holder closes the last handle while the host lives: the host's mapping goes.
$p = Start-Process C:\hostmic\test-host.exe -ArgumentList '--seconds','8','--holder','--close-own-handle' -RedirectStandardOutput C:\hostmic\b.txt -PassThru -WindowStyle Hidden
Start-Sleep 3; Get-Process ping | Stop-Process -Force; $p | Wait-Process; Get-Content C:\hostmic\b.txt
```

Expected: after the first kill, the driver logs `the host's process exited with its handle
still open elsewhere` and `host detached`, the microphone is unplugged, and the next
`test-host` attaches; in the second case `b.txt` shows the mapping `mapped`, then `unmapped`
once the holder is killed, while the host still runs. A holder left running keeps a handle to
the interface open, which makes `devcon remove` answer "Removed on reboot" (below).

The microphone's jack follows the host. The endpoint's state is in the registry, under
`HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Capture\{…}` (the key whose
`Properties` name `hostmic`), value `DeviceState`: 8 (unplugged) without a host, 1 (active)
while `test-host` is attached, 8 again once it detaches or is killed; `capture-check` then
finds no active "hostmic" endpoint.

Removing the driver while a host is attached is refused (the host interface vetoes the
removal: `devcon` answers "Removed on reboot" and the driver stays loaded until the VM
restarts). Close the host first.

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
