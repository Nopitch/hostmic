# hostmic

[![OpenSSF Baseline](https://www.bestpractices.dev/projects/16465/baseline)](https://www.bestpractices.dev/projects/16465)

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

**Experimental, not usable yet.** The driver already exposes a working capture endpoint,
"Microphone (hostmic)", fed by a host application through a buffer it shares with the Windows
audio engine; the host sets the pace (milestone M4, done: verified in a test VM with the test
host playing a 1 kHz tone). The host library is the next milestone. See [PLAN.md](PLAN.md) for the
roadmap. There is no release. Until the driver is signed through Microsoft attestation, it can
only be loaded with test signing enabled, which requires Secure Boot to be disabled: use a
virtual machine, never your everyday machine ([docs/test-vm.md](docs/test-vm.md)).

## How hostmic is built

**Directed by a human, written with AI.** The maintainer sets the goals, makes the design
decisions (recorded in [PLAN.md](PLAN.md)), tests the driver and decides what is merged; most of
the code and documentation is written with Claude, Anthropic's AI assistant, and every such
commit says so (`Co-Authored-By`). Nothing reaches `main` without:

- the maintainer's review of the pull request;
- the required checks: formatting, clippy with strict lints, unit tests, the driver build,
  CodeQL, secret scanning, dependency review, workflow audit and a signed-off commit (DCO);
- for driver changes, a test in a virtual machine with measured results
  ([docs/test-vm.md](docs/test-vm.md)).

Why the design can be trusted, and where it cannot yet, is argued in
[docs/security-assurance.md](docs/security-assurance.md).

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
4. **Use it**: without a host, "Microphone (hostmic)" shows as unplugged. Start the test host
   (`tools/test-host`): the microphone appears in Settings → System → Sound → Input. Record it
   with any application, for example the Sound Recorder: you hear the steady 1 kHz tone the
   test host plays. `tools/capture-check` measures it, and `tools/hostmic-ctl status` shows the
   stream and the driver's counters ([docs/test-vm.md](docs/test-vm.md), sections 4 and 6).
5. **Remove it**: `devcon remove root\hostmic`.

## Licensing

- The **driver** is a port of Microsoft's audio driver samples and is distributed under the
  [Microsoft Public License](LICENSES/MS-PL.txt) (MS-PL), like the samples it derives from.
- The **user-mode host library** is original code, licensed under either the
  [MIT license](LICENSES/MIT.txt) or the [Apache License 2.0](LICENSES/Apache-2.0.txt), at
  your option.
- **`hostmic-abi`**, the contract between the driver and the host (control codes and request
  layouts), is shared by both and licensed like the host library: MIT or Apache-2.0, at your
  option.
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
