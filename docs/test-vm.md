# Test VM

The driver is only ever loaded in a disposable virtual machine, never on an everyday machine:
test signing needs Secure Boot off, which also breaks games protected by anti-cheats.

This is the procedure used for milestone M1 (2026-11-08), on a Windows 11 Pro host with
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

## 3. Install the driver and read its messages (VM, elevated)

Trust the test certificate, in the VM only (`-f` creates the `TrustedPublisher` store, absent
on a fresh install):

```powershell
certutil -addstore Root C:\hostmic\WDRLocalTestCert.cer
certutil -f -addstore TrustedPublisher C:\hostmic\WDRLocalTestCert.cer
```

Start [DebugView](https://learn.microsoft.com/sysinternals/downloads/debugview) elevated with
kernel capture (`Dbgview64.exe /accepteula /k /v`), then:

```powershell
C:\hostmic\devcon.exe install C:\hostmic\hostmic_driver.inf root\HOSTMIC_M1
C:\hostmic\devcon.exe remove root\HOSTMIC_M1
```

Expected output:

```text
hostmic: DriverEntry
hostmic: WdfDriverCreate returned 0x00000000
hostmic: device added, WdfDeviceCreate returned 0x00000000
hostmic: driver unloaded
```

The device is root-enumerated: `pnputil /add-driver` alone stages the package but creates no
device node, hence `devcon`.

To go back to a clean state (host):

```powershell
Restore-VMCheckpoint -VMName hostmic-test -Name clean-testsigning -Confirm:$false
```
