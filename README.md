# hostmic

An open-source Windows **virtual microphone driver fed by a user-mode host**, written in Rust.

Some applications generate or process audio that other programs should receive as if it came
from a regular microphone: audio processing and effects, sound synthesis, streaming and
broadcasting tools, accessibility software, audio test and measurement. Today this usually goes
through a *virtual audio cable*:
the application plays into a virtual speaker, the cable copies the audio to a virtual microphone
at its own pace, with its own buffering. That middleman costs latency and, under heavy CPU load,
dropouts.

hostmic removes the middleman. The driver exposes a real capture endpoint (a microphone) and
maps the client's ring buffer into the **host** application, which writes samples directly where
Windows reads them and **drives the clock** by advancing the device position. When no host is
connected, the microphone is reported as unplugged.

```
host application ──writes into the shared ring buffer──▶ [hostmic microphone] ──▶ Windows audio engine ──▶ any recording application
```

## Status

**Experimental — nothing works yet.** See [PLAN.md](PLAN.md) for the roadmap. Until the driver is
signed through Microsoft attestation, it can only be loaded with test signing enabled, which
requires Secure Boot to be disabled: use a virtual machine, never your everyday machine.

## Why Rust

[windows-drivers-rs](https://github.com/microsoft/windows-drivers-rs) makes kernel drivers in
Rust possible, but it does not cover audio (PortCls / Kernel Streaming) yet. hostmic generates
its own bindings for the audio headers and implements the WaveRT miniport interfaces in Rust,
starting from Microsoft's
[`simpleaudiosample`](https://github.com/microsoft/Windows-driver-samples/tree/main/audio/simpleaudiosample).

## Quick start

hostmic is experimental: try it in a virtual machine only. In about an hour, from scratch:

1. **A test VM.** Create a Hyper-V virtual machine with Windows 11, turn Secure Boot off and
   test signing on: [docs/test-vm.md](docs/test-vm.md), sections 1 and 2.
2. **Build the driver** on your Windows machine (requirements in
   [CONTRIBUTING.md](CONTRIBUTING.md#build-and-test)):

   ```powershell
   git clone https://github.com/Nopitch/hostmic
   cd hostmic
   cargo wdk build
   ```

   The test-signed package lands in `target\debug\hostmic_driver_package\`.
3. **Install it in the VM**: copy the package and the WDK's `devcon.exe` into the VM, trust the
   test certificate, then run `devcon install hostmic_driver.inf root\hostmic` as administrator
   ([docs/test-vm.md](docs/test-vm.md), section 3).
4. **Use it**: "Microphone (hostmic)" appears in Settings → System → Sound → Input. Record it
   with any application, for example the Sound Recorder: you hear a steady 1 kHz tone (the
   built-in test signal until the host interface exists). `tools/capture-check` measures it
   ([docs/test-vm.md](docs/test-vm.md), section 4).
5. **Remove it**: `devcon remove root\hostmic`.

## Licensing

- The **driver** is a port of Microsoft's audio driver samples and is distributed under the
  [Microsoft Public License](LICENSES/MS-PL.txt) (MS-PL), like the samples it derives from.
- The **user-mode host library** is original code, licensed under either the
  [MIT license](LICENSES/MIT.txt) or the [Apache License 2.0](LICENSES/Apache-2.0.txt), at
  your option.
- **`hostmic-sys`**, the bindings to the WDK's audio headers, is generated from those headers
  like `wdk-sys` and shares its license: MIT or Apache-2.0, at your option.

Each crate states its license in its `Cargo.toml` (SPDX expression); the license texts are in
[`LICENSES/`](LICENSES), named by their SPDX identifiers, as in the
[REUSE](https://reuse.software/) layout.

## Prior art

- Thin Audio Gateway (Eugene Muzychenko), a commercial driver built on the same idea; hostmic
  shares no code with it.
- Virtual Audio Cable, VB-Cable: virtual cables, the model hostmic replaces.
- [Leyline](https://github.com/slategraydev/LeylineVirtualAudioDeviceRust), an early Rust WaveRT
  driver.
