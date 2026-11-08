# hostmic — plan

This file is the source of truth for the project: read it before proposing anything, update it
when a decision changes. One notion per pull request.

## Goal

A Windows kernel driver, written in Rust, that exposes **one capture endpoint** (a microphone)
whose audio is supplied by a **user-mode host application** through a shared ring buffer, the host
providing the clock. Signed through Microsoft attestation (via an open-source signing programme),
so that it loads with Secure Boot enabled, on any Windows 10/11 x64 machine.

First user: Résonnance (not public yet), a real-time audio processing application, which today
writes into a virtual cable (VAC, VB-Cable) to reach other applications.

### Target, from measurements

Measured on 2026-11-08 with the Thin Audio Gateway demo, which uses the same architecture
(host-fed WaveRT microphone, host thread registered with MMCSS, shared-mode WASAPI reader as
most communication and recording applications use):

| | write → read | under load (16 saturated threads, 30 s) |
|---|---|---|
| host poll 5 ms | median 10.8 ms | 0 discontinuities |
| host poll 2 ms | median 9.6 ms | 0 discontinuities |

For comparison, VAC needs ~42 ms round trip (15 ms exclusive output period) and drops out under the
same load below 15 ms. **hostmic is done when it matches the first table.** The ~10 ms floor is the
Windows audio engine period.

### Non-goals

- No audio processing in the driver.
- No render (speaker) endpoint at first: that would rebuild a virtual cable.
- Not a clone of Thin Audio Gateway: no code, header or binary from it is used or read beyond its
  public documentation.

## Architecture

### Driver (kernel, `no_std` Rust, MS-PL)

- **Model:** PortCls with a WaveRT miniport, as in Microsoft's `simpleaudiosample` (capture path
  only) plus a topology miniport. ACX (the newer KMDF-based model) is set aside: Windows 11 only,
  fewer virtual-device examples; to reconsider once the WaveRT version works.
- **Bindings:** `wdk-sys` 0.5 (windows-drivers-rs, checked 2026-11-08) covers WDM/KMDF, HID, USB,
  storage… but not `portcls.h`, `ks.h` or `ksmedia.h`. A `hostmic-sys` crate (MIT OR Apache-2.0,
  like `wdk-sys`: generated from the WDK headers, no sample code) generates them with bindgen,
  through `wdk-build`, restricted to what the driver uses.
- **COM-style interfaces:** PortCls calls the miniport through C++ interfaces (`IUnknown`,
  `IMiniportWaveRT`, `IMiniportWaveRTStream`, `IMiniportTopology`…). They are implemented in Rust
  as `#[repr(C)]` structs whose first field is a vtable pointer, with explicit reference counting.
  Every `unsafe` block carries a `// SAFETY:` comment stating the invariant.
- **Static descriptors:** filter, pin, node and property tables (`PCFILTER_DESCRIPTOR` and
  friends) are `static` Rust data, built once, never mutated.
- **Host interface:** a device interface (its own GUID) opened with `CreateFile`; `DeviceIoControl`
  requests to attach as host, set the format range, receive a notification event, map the client
  buffer, advance the position. The client buffer is described by an MDL and mapped into the
  host process only.
- **No host, no microphone:** without a host, the pin reports its jack as disconnected
  (`KSPROPERTY_JACK_DESCRIPTION`), so Windows shows it unplugged instead of a silent device.
- **One host at a time.** The handle's cleanup unmaps the buffer and marks the pin unplugged if the
  host dies.

### Host library (user mode, Rust, MIT OR Apache-2.0)

- Crate `hostmic`: find and open the driver, attach as host, choose the format (48 kHz, 16-bit,
  stereo first), get a safe view of the ring buffer, advance the position.
- A reference host loop, clocked by QPC like Thin Audio Gateway's, registered with MMCSS
  ("Pro Audio"). Later: let the caller drive the clock (Résonnance wants to follow its physical
  microphone, to remove clock drift).
- Real-time rule in the host's audio path: no allocation, no lock, no I/O.

### Security

A driver that maps memory into a user process is a classic source of vulnerabilities. Rules:

- Validate every IOCTL input size and value; reject everything unexpected.
- The host interface (its device object or interface) is restricted to administrators and the
  interactive user. The Kernel Streaming filters keep the usual audio-device access (the INF's
  SDDL, as in Microsoft's sample): the audio service and applications must open them.
- Map only the client buffer, only into the attached host, with user-mode probing inside
  structured exception handling; unmap on cleanup.
- Compatible with Memory Integrity (HVCI): non-executable pool, no RWX mappings.
- Driver Verifier enabled in the test VM; static analysis (CodeQL) before any signing request.

## Development environment

- **Never on an everyday machine.** Test signing needs Secure Boot off, which also breaks games
  protected by anti-cheats such as BattlEye.
- A Hyper-V virtual machine (Windows 11 Pro), test signing on, kernel debugging over the network
  (KDNET), snapshots before each driver install.
- Host build machine: Visual Studio with the WDK (or the eWDK) and LLVM/Clang for bindgen, as
  required by windows-drivers-rs (re-read its README at M1 for exact versions).
- Rust toolchain pinned in `rust-toolchain.toml`, bumped on purpose only.

## Milestones (one notion per pull request)

- **M0, bootstrap** (this commit): README, plan, licences.
- **M1, toolchain proof:** a minimal Rust KMDF driver from windows-drivers-rs' samples builds,
  installs and logs a message in the VM. Signing programme: criteria read, see M7.
  *Build part done (2026-11-08):* workspace with the WDK configuration at the root (KMDF 1.33,
  as Microsoft's `Windows-rust-driver-samples`), crate `driver/` (`hostmic-driver`,
  `hostmic_driver.sys`), Rust 1.98.1, wdk 0.5.0 / wdk-sys 0.5.2 / wdk-build 0.6.0, WDK
  10.0.26100, LLVM 23.1.3 through `LIBCLANG_PATH` (the README suggests 17 because of an ARM64
  bug fixed in 19; x64 builds fine). Built with **`cargo wdk build`** (cargo-wdk 0.2.0, preview):
  build, stampinf, inf2cat, infverif, test signing with a self-signed `WDRLocalTestCert`. The
  older `cargo make` flow needs to create a symbolic link in `target/`, which Windows only
  allows to administrators or in Developer Mode. infverif rejects the samples' `Sample` setup
  class (reserved for Microsoft): the M1 device has a class of its own, `HostmicTest`.
  *Loaded in the VM (2026-11-08):* Hyper-V generation 2, Windows 11 Pro, Secure Boot off, test
  signing on; procedure in [`docs/test-vm.md`](docs/test-vm.md). `devcon install` (the device is
  root-enumerated, so `pnputil` alone creates no device node) logs `DriverEntry`,
  `WdfDriverCreate` and `WdfDeviceCreate` returning `STATUS_SUCCESS`; `devcon remove` unloads it.
  Lesson: for a PnP driver, `WdfDriverCreate` overwrites `DRIVER_OBJECT::DriverUnload` with the
  framework's routine, which only calls `WDF_DRIVER_CONFIG::EvtDriverUnload` (KMDF's
  `FxDriver::Initialize`); the sample's direct `DriverUnload` is never called, so the driver
  uses `EvtDriverUnload`. No host unit tests for the driver crate: a test binary links the KMDF
  stub, which needs the kernel's `DriverEntry`.
  *CI:* workflow `driver` on `windows-2025`: WDK from NuGet (`Microsoft.Windows.WDK.x64`, as
  windows-drivers-rs' own CI does, instead of the image's), the image's LLVM, `cargo fmt`,
  `cargo clippy`, `cargo wdk build`. Dependabot watches cargo (windows-drivers-rs crates grouped).
- **M2, audio bindings:** `hostmic-sys`, bindgen over the PortCls/KS headers, with the subset
  used by `simpleaudiosample`'s capture path; compiles, tests check struct sizes and offsets
  against the C headers.
  *Done (2026-11-08):* the capture path (inventoried from the sample) uses KMDF in miniport mode
  (`WdfDriverInitNoDispatchOverride`) next to PortCls, 8 PortCls functions, ~17 COM interfaces,
  ~45 structs, ~45 GUIDs. `hostmic-sys/build.rs`:
  - bindgen in C mode over `portcls.h` and `ksmedia.h`, name allowlists, non-recursive: every
    WDM type is `wdk-sys`'s own. A parse callback tells bindgen those types are `Copy` (else C
    unions such as `KSDATAFORMAT` become an over-aligned emulation that packed structs such as
    `KSDATAFORMAT_WAVEFORMATEX` cannot contain).
  - In C, `portcls.h` never defines `INTERFACE`, so each method's `This` would be a pointer to
    WDM's unrelated `INTERFACE` struct: the headers are included in `portcls.h`'s order and
    `INTERFACE` is `void` for `portcls.h` alone.
  - GUIDs as Rust constants, values read through MSVC's preprocessor (KS: `STATIC_*` macros;
    PortCls IIDs/CLSIDs: `DEFINE_GUID` under `INITGUID`, since no WDK library defines them).
    `punknown.h`'s `IID_IUnknown` is `…-0000-C00000000046`, not COM's `…-C000-000000000046`
    (the one in `ksguid.lib`).
  - Tests (`cargo test`, in CI): sizes, alignments and field offsets of 38 structs and vtables
    compared with MSVC (a C file compiled by `cc`), and GUIDs against published values.
  For M3: `QueryInterface` accepts both `IID_IUnknown` values; the adapter keeps its state in
  its own device extension (the sample writes into PortCls's reserved one); the sample's
  timer, `GetPosition` and `GetReadPacket` take a spin lock, against the real-time rule.
- **M3, a microphone that plays a tone:** port the adapter, the WaveRT capture miniport and the
  topology miniport; the driver itself fills the buffer with a sine, like the sample's tone
  generator. Done when "Microphone (hostmic)" appears in the VM and records a clean 1 kHz tone.
  *Design (2026-11-08), deviations from the sample (read at commit `d49bb7d`):*
  - **WDM, no KMDF.** The sample's only framework object is a `WdfDeviceMiniportCreate` device
    used for Bluetooth bypass; without it the driver is plain PortCls. To reconsider in M4 if
    a KMDF control device helps the host interface.
  - **No adapter COM object, no adapter power management** (M6): the adapter state is a Rust
    struct in the driver's own part of the device extension (`PcAddAdapterDevice` with
    `PORT_CLASS_DEVICE_EXTENSION_SIZE` plus its size), not in PortCls's reserved part.
  - **A plain microphone:** topology bridge pin of category `KSNODETYPE_MICROPHONE` without a
    custom name, hence "Microphone (hostmic)"; no microphone-array geometry, SNR or sensitivity;
    no volume or mute node (Windows applies software volume); no signal processing modes.
  - **One format:** 48 kHz, 16-bit, stereo (the sample: 32-bit).
  - **No lock in the real-time path:** the stream's only writer is its high-resolution timer
    callback, which publishes the position through atomics; `GetPosition` and `GetReadPacket`
    only read them. The position is a pure function of the performance counter since RUN.
  - **The tone is a table:** one period of 1 kHz at 48 kHz is exactly 48 frames, computed at
    compile time; frame `n` is `TABLE[n % 48]`, so no floating point runs in the kernel.
  - **COM objects:** `#[repr(C)]` structs with one `&'static` vtable field per interface chain,
    an atomic reference count, allocated in non-paged pool and freed by the last `Release`;
    methods find their object from the interface pointer with `offset_of!`.
  - Arithmetic that needs no kernel call lives in `hostmic-driver-core` (`no_std`, no `unsafe`,
    MS-PL), unit-tested by `cargo test`; M4's IOCTL validation will go there too.
  Pull requests: `hostmic-driver-core`; the adapter, the COM plumbing, the topology miniport
  and the `MEDIA` INF; a user-mode capture checker (frequency, level, discontinuities) for the
  VM test; the WaveRT miniport, its stream and the tone.
  *`hostmic-driver-core` done (#9).* *Adapter and topology (2026-11-08):* the workspace builds
  a WDM driver (imports `ntoskrnl.exe` and `portcls.sys` only); in the VM the device installs
  as `ROOT\MEDIA\0000` (class `MEDIA`, no problem code), registers the topology filter's
  `KSCATEGORY_AUDIO` and `KSCATEGORY_TOPOLOGY` interfaces, shows no microphone yet, and
  `devcon remove` unloads it. Lesson: `wdk-build` reads the WDK configuration from the
  `Cargo.toml` it finds above `OUT_DIR`, so a `CARGO_TARGET_DIR` outside the workspace (or
  shared with another checkout) silently builds with another configuration: the first WDM
  build still linked KMDF's `WdfDriverEntry`. Keep each checkout's `target/` inside it.
  *Done (2026-11-08):* the WaveRT miniport, its stream and the tone. In the VM, "Microphone
  (hostmic)" is active and `capture-check` (shared-mode, event-driven WASAPI) records 60 s at
  1000.00 Hz, -6.02 dBFS, identical channels, 6000 packets of 10 ms, no discontinuity and no
  glitch; three stop/start cycles pass; `devcon remove` unloads the driver. Lesson: the audio
  endpoint builder probes the capture pin with bare `KSDATAFORMAT`s (64 bytes, no
  `WAVEFORMATEX`), one per subtype (PCM, then compressed pass-through); rejecting the PCM probe
  leaves the endpoint "not present" (`DeviceState` 4) without any error message, so
  `PROPOSEDATAFORMAT` accepts a bare PCM probe (`format::is_proposal_supported`). Not done in
  M3: Driver Verifier (M6).
- **M4, the host interface:** IOCTLs, buffer mapping, host-advanced position, unplugged without a
  host; the tone generator goes away. IOCTL input validation is pure Rust, without kernel
  calls, so that it can be unit-tested and **fuzzed** on the host with `cargo-fuzz`; a short
  fuzzing run in CI on each pull request (also OpenSSF Scorecard's Fuzzing check).
- **M5, the host crate:** `hostmic` API, reference host loop, examples; latency and load tests
  ported from Résonnance (`tag_micro`): write → read, discontinuities under load, endurance.
- **M6, hardening:** security review against the rules above, Driver Verifier, HVCI, sleep and
  resume, host crash, several hours of endurance.
- **M7, signing:** submit to an open-source signing programme, then install on a real machine
  with Secure Boot on. OSSign signs kernel drivers through Microsoft attestation. Its published
  criteria (read 2026-11-08; applications were then suspended because of a backlog):
  - an OSI-approved license, all released code public;
  - an actively maintained project with a demonstrable user base or community, and **at least
    6 months of activity** of the account and the project: the repository was created on
    2026-11-08, so not before May 2027;
  - **public automation**: a build pipeline where the whole build is verifiable and each
    release binary is linked to its source, with code quality checks. Before applying, add a
    release workflow: a `v*` tag builds the package on GitHub-hosted runners, attaches it to
    the GitHub release and publishes a build provenance attestation;
  - no commercial focus (possible on request) and no political or controversial purpose.
  Fallback if refused: Microsoft attestation signing through a Partner Center hardware
  developer account, which requires an EV code-signing certificate (yearly cost).
- **M8, Résonnance:** in Résonnance's repository, an output "Résonnance microphone (hostmic)"
  replacing the virtual cable.

## OpenSSF Best Practices badge

Project entry: <https://www.bestpractices.dev/projects/16465>; the README shows the Baseline
badge. Status on 2026-11-08, after the governance and security documents (GOVERNANCE.md,
SECURITY.md, docs/security-assurance.md), the DCO and the `dependencies` and `dco` required
checks:

- **Baseline:** level 1 and level 2 met; level 3 at 95%, missing only a non-author approval
  (OSPS-QA-07.01), which needs a second maintainer.
- **Metal:** passing at 99%, missing only release notes (no release yet); silver at 91%,
  missing passing, access continuity and bus factor (single maintainer), 80% statement
  coverage, fuzzing (M4), reproducible builds, signed tags and hardening (M6).

**The first release must not be a hand-made one.** Several criteria answered "not applicable"
because no release exists become requirements with it; in particular, OSPS-BR-06.01 (baseline-2)
requires a signed release or a signed manifest of its assets' hashes, so an unsigned release
would lose baseline-2. Before tagging `v0.1.0`, the release workflow (M7) must provide: a
protected `v*` tag (signed if possible), release notes with functional and security changes,
assets named after the version, build provenance attestations, a software bill of materials,
documentation on verifying the assets and their author, a user guide and defect-reporting
guide, the license files with the assets, and a statement of support scope and duration in
SECURITY.md.

## Open questions

- Endpoint naming: the device name is fixed in the INF; the endpoint name per line.
- Windows 10 support: keep it if it costs nothing (WaveRT is fine there; ACX would not be).
- ARM64: later.
- Signing programme: whether OSSign reopens applications, accepts the project and how long it
  takes; a user base to show by then (a usable public release would help).

## Prior art and references

- Microsoft driver samples, MS-PL:
  [`audio/simpleaudiosample`](https://github.com/microsoft/Windows-driver-samples/tree/main/audio/simpleaudiosample),
  [`audio/sysvad`](https://github.com/microsoft/Windows-driver-samples/tree/main/audio/sysvad).
- [windows-drivers-rs](https://github.com/microsoft/windows-drivers-rs) (`wdk`, `wdk-sys`,
  `wdk-build`).
- Thin Audio Gateway (public documentation only): the host-fed WaveRT design and its measured
  latency.
- [Leyline](https://github.com/slategraydev/LeylineVirtualAudioDeviceRust): early Rust WaveRT
  driver. **No license** (all rights reserved): read to learn, never copy.
