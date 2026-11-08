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

## Licensing

- The **driver** is a port of Microsoft's audio driver samples and is distributed under the
  [Microsoft Public License](LICENSE-MS-PL) (MS-PL), like the samples it derives from.
- The **user-mode host library** is original code, licensed under either the
  [MIT license](LICENSE-MIT) or the [Apache License 2.0](LICENSE-APACHE), at your option.

Each crate states its license in its `Cargo.toml`.

## Prior art

- Thin Audio Gateway (Eugene Muzychenko), a commercial driver built on the same idea; hostmic
  shares no code with it.
- Virtual Audio Cable, VB-Cable: virtual cables, the model hostmic replaces.
- [Leyline](https://github.com/slategraydev/LeylineVirtualAudioDeviceRust), an early Rust WaveRT
  driver.
