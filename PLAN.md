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
  *Reconsidered (2026-11-09, the WaveRT version works since M5):* WaveRT stays at least through
  M8. PortCls is not deprecated (Microsoft recommends ACX for new hardware drivers; WaveRT
  remains supported and underlies the existing ones); ACX would drop Windows 10; windows-
  drivers-rs has no ACX bindings either, so it would not spare the generated bindings; and
  rewriting the driver's core now, while M6 hardens it for signing (M7), would be the riskiest
  moment for it.
  What ACX would bring (framework objects instead of the hand-built COM plumbing, less
  `unsafe`) changes nothing a user sees. To weigh again after M8.
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
- **Host interface:** a device interface with its own GUID, found with
  `CM_Get_Device_Interface_List` and opened with `CreateFile`, registered on a raw child PDO
  that the adapter enumerates (M4 design below); `DeviceIoControl` requests to attach as host
  with a format range and a notification event, read the stream state, advance the position,
  read counters, detach. The ring buffer the audio engine reads is mapped into the host process
  only, for as long as the host stays attached.
- **No host, no microphone:** without a host, the pin reports its jack as disconnected
  (`KSPROPERTY_JACK_DESCRIPTION`), so Windows shows it unplugged instead of a silent device.
- **One host at a time.** Other processes of the same users may open the interface to read its
  state and counters. The handle's cleanup unmaps the buffer and marks the pin unplugged if the
  host dies.

### Host library (user mode, Rust, MIT OR Apache-2.0)

- Crate `hostmic`: find and open the driver, attach as host, choose the format (48 kHz, 16-bit,
  stereo first), get a safe view of the ring buffer, advance the position.
- A reference host loop, clocked by QPC like Thin Audio Gateway's, registered with MMCSS
  ("Pro Audio"). Later: let the caller drive the clock (Résonnance wants to follow its physical
  microphone, to remove clock drift), with a clock correction ratio the caller can read and
  adjust at run time, as Virtual Audio Cable offers.
- A C header next to the Rust API: professional hosts are often C++ applications (after M8,
  once a real host has settled the API).
- Tools in the style of Virtual Audio Cable's control panel: plain text, every setting and
  counter visible with its unit (formats, stream state, overflows, underflows, rejected
  requests), factual messages; no decoration.
- Real-time rule in the host's audio path: no allocation, no lock, no I/O.

### Security

A driver that maps memory into a user process is a classic source of vulnerabilities. Rules:

- Validate every IOCTL input size and value; reject everything unexpected.
- The host interface is restricted to the system, administrators and interactive users. The
  Kernel Streaming filters keep the usual audio-device access (the INF's SDDL, as in
  Microsoft's sample): the audio service and applications must open them. Every device object
  of a PnP stack shares one security descriptor, so the host interface lives on a device object
  of its own: a raw child PDO created with `IoCreateDeviceSecure`, whose descriptor
  administrators can override in the registry through its class GUID (for a host running as a
  service, for example).
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
- **CI** (GitHub-hosted runners only; actions pinned by SHA; read-only tokens by default):
  - caches only in pull-request and `main` CI, written by pushes to `main` only, keyed on
    exact versions (the WDK, with its NuGet SHA-512 checked on every run; cargo-wdk; the
    toolchain; LLVM; `Cargo.lock`), with the workspace's own crates removed before saving;
    never in a release workflow (M7);
  - required checks never filtered by path (a check that never reports blocks merging): a
    job that has nothing to do succeeds quickly instead, and a fan-out (the fuzz targets, one
    job each) reports through an aggregate job, named after the required check, that fails
    unless every job succeeded;
  - no third-party action where a few lines of shell do (change detection, aggregation): the
    2025 compromise of `tj-actions/changed-files` is the reason;
  - Driver Verifier, HLK and SDV need a machine that loads the driver and reboots: they run in
    the test VM (M6), as open-source driver projects run them outside GitHub-hosted runners.

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
  *Design (2026-11-08):*
  - **The host interface is a device interface on a raw child PDO.** Microsoft recommends
    device interfaces over named device objects for PnP drivers ("Named Device Objects"), and
    OSR recommends against naming the FDO. But a device interface opens through its stack's
    PDO, and every device object of the stack shares the INF's descriptor, which must let the
    audio service in. The adapter therefore acts as a bus driver for one child: a raw PDO (no
    function driver, as in the `kbfiltr` sample's sideband device) created with
    `IoCreateDeviceSecure`, SDDL `D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)` (system,
    administrators, interactive users; `IU` is part of the device-object SDDL subset) and a
    class GUID of its own for registry overrides. It registers the interface
    `GUID_DEVINTERFACE_HOSTMIC_HOST`, so hosts find it by GUID, get arrival and removal
    notifications (`CM_Register_Notification`), and several instances stay possible later. A
    fixed-name control device was set aside: no notifications, one instance only, the legacy
    pattern. Still WDM: `wdmsec.lib` provides `IoCreateDeviceSecure`, no KMDF needed.
  - **Dispatch:** PortCls owns the driver object's dispatch table; the driver puts a switch in
    front of it that sends the child PDO's requests to its own handlers (create, cleanup, close,
    device control, PnP and power for a PDO) and everything else to `PcDispatchIrp`. The switch
    compares device object pointers, never file names.
  - **Requests** (`hostmic-abi`, MIT OR Apache-2.0 so that the host crate can use it; all
    `METHOD_BUFFERED`, so the input is copied once by the I/O manager and never probed or read
    twice; exact sizes, zero reserved fields, an ABI version): `GET_INFO` (version,
    capabilities, supported formats), `ATTACH` (the host's format range and notification event;
    returns the buffer's address in the host), `GET_STREAM` (generation, state, format, buffer
    and packet size, position; the host reads it when its event is signaled), `ADVANCE`
    (generation, expected position, frames written), `DETACH`, and with the position work
    `GET_STATS` (counters). Any process the descriptor lets in may open the interface and read;
    only the attached host's process may advance. Hosts are 64-bit processes: for a 32-bit
    one the buffer could be mapped out of reach. The validation of every request is pure Rust
    in `hostmic-driver-core`, unit-tested and fuzzed.
  - **One buffer for the adapter's life.** WaveRT allocates a buffer per stream and frees it
    when the stream closes; mapping that buffer into the host would mean taking it away while
    the host writes. Instead the adapter owns one buffer (1 s at most of the largest format,
    rounded up to whole pages), allocated at its first start and reference-counted by the
    adapter, each stream buffer and the host's mapping; a host that detaches and attaches again
    while a stream runs finds the same pages (a buffer per attachment, the first idea, would
    have left a running stream on the old one). `AllocateAudioBuffer` returns a partial MDL
    over its start. The audio engine requires the actual size to be at least the requested
    one, so a larger request fails. The driver no longer writes audio: it zeroes the buffer and
    nothing else.
  - **Mapping into the host:** `MmMapLockedPagesSpecifyCache` in user mode raises an exception
    on failure and Rust cannot catch structured exceptions, so a C function of a few lines,
    compiled with `cc`, wraps it in `__try`/`__except`; user mappings are never executable. The
    mapping is undone in the host's process: on `DETACH` from the host's own process (since
    M5: a duplicate of its handle in another process may not), on the handle's cleanup or close (in
    whatever process closes it last, attaching to the host's), and from a process notification
    routine (`PsSetCreateProcessNotifyRoutineEx`, hence `/INTEGRITYCHECK`) when the host's
    process exits while another process holds a duplicate of its handle; it runs in the dying
    process before its address space goes, and attach and detach share a lock with it.
  - **The host advances the position, without a lock:** the stream's generation and position
    share one 64-bit word (an even generation accepts frames, an odd one does not; writers
    halt the generation before changing anything else), and `ADVANCE` moves it with one
    compare-and-exchange, so a request computed for an earlier stream or racing another one
    cannot win;
    `GetPosition` and `GetReadPacket` take no lock. `ADVANCE` signals the engine's event when a
    packet completes; rundown protection (`EX_RUNDOWN_REF`, a non-blocking atomic count) keeps
    the event alive while it runs, instead of the timer's `KeFlushQueuedDpcs`. The timer and
    the tone leave the driver; the tone moves to a test host. Each `ADVANCE` is still an IRP,
    which the I/O manager allocates with its system buffer, about a thousand times a second: an
    accepted cost of an IOCTL-driven position, outside the driver's own real-time code.
  - **Unplugged without a host:** the topology keeps its port's `IPortEvents`, reports
    `IsConnected` from the host's presence with `JACKDESC2_PRESENCE_DETECT_CAPABILITY`, and
    generates `KSEVENT_PINCAPS_JACKINFOCHANGE` on attach, detach and host death.
  - **Formats:** the ABI carries format ranges (rates, channels, sample types) from the start;
    M4 accepts 48 kHz, 16-bit, stereo only.
  - **Counters and tools:** `GET_STATS` reports frames advanced, packets completed, packets
    skipped (overwritten by the host before the engine read them: the overflow a capture
    device can have; a host that stops advancing is not an underflow the driver can see, the
    engine just gets no packet) and refused requests with the last reason, all atomic
    counters; `tools/hostmic-ctl status` prints them, and the driver's rejection messages
    state the operation, received and required sizes and the process, as Virtual Audio Cable's
    log does.
  - **Fuzzing:** `fuzz/` (cargo-fuzz) with a target per request decoder and a target that
    replays sequences of attach, stream, advance and detach against the pure model; a CI job on
    a GitHub-hosted Linux runner with a pinned nightly (cargo-fuzz needs one; the repository's
    toolchain stays stable), 60 s per target on each pull request, required as `fuzz`.
    *Done (2026-11-09):* six targets: Kernel Streaming data formats, the ABI layouts' round
    trips, `ATTACH`, `ADVANCE`, stream buffer geometry, and `sequence`, which replays any
    interleaving of state changes and `ADVANCE`s (including a state change between an
    `ADVANCE`'s check and its compare-and-exchange) on a model of the driver's atomics and
    checks the `Cursor` protocol's invariants. The `fuzz` workflow also formats and lints the
    fuzz crate, which is a workspace of its own; 10 minutes per target every week.
  Pull requests, one at a time: the ABI and its validation; fuzzing; the child PDO and its
  interface; attach, detach and the stream state with `tools/test-host`; the jack following
  the host; the shared buffer and its mapping; the host-advanced position with the counters
  and `tools/hostmic-ctl`; the closing notes. Done when `tools/test-host` plays a tone that
  `tools/capture-check` records cleanly from "Microphone (hostmic)", the microphone shows
  unplugged without a host, killing the host mid-capture leaves the system up, and fuzzing runs
  in CI.
  *Child PDO and host interface done (2026-11-09):* `driver/src/host_device.rs` creates the
  child (`HOSTMIC\Host`, "hostmic host interface") in the adapter's `StartDevice`, reports it
  in `BusRelations` and answers its PnP and power requests as its bus driver; the interface
  answers `GET_INFO`; `tools/hostmic-ctl info` prints it. The driver's dispatch routine wraps
  all 28 major functions and PortCls's fast I/O table, so that nothing of the child ever
  reaches PortCls; the child is recognized by address. Opens are refused before the child
  starts, after it stops, and relative to another handle. In the VM: the child is `OK` (raw,
  no setup class), the interface enabled, `NETWORK SERVICE` refused with `ACCESS_DENIED`,
  disabling and enabling the child in Device Manager works, the tone still passes
  `capture-check`, and `devcon remove` unloads the driver. Lessons: PortCls forwards the
  adapter's `BusRelations` query down untouched, so adding the child before calling
  `PcDispatchIrp` is enough; an independent review found that the first version forgot fast
  I/O, relative opens and re-enabling, and could leak a child never enumerated.
  *Attach, detach and the stream state done (2026-11-09):* the adapter's device extension
  holds a `HostLink` (`driver/src/host_link.rs`) that the capture stream and the host
  interface share: the stream publishes its format, buffer geometry and state under the
  `Cursor` protocol and signals the host's event on every change; `ATTACH` references that
  event (`ObReferenceObjectByHandle`, user mode, in the requesting process only) and records
  the host's handle, `DETACH` or the handle's cleanup and close release it, `GET_STREAM`
  reads a consistent snapshot. No buffer is mapped yet (`ATTACH` replies with a null buffer).
  `tools/test-host` attaches and prints every change. In the VM: a second host is refused
  with `STATUS_DEVICE_BUSY`; the host sees stopped, paused (960-frame buffer in packets of
  480), running, paused, stopped while `capture-check` records (still `PASS`); a killed host
  and a host exiting without `DETACH` are detached. Lessons from the independent review: a
  request can still arrive on a device object after the adapter's removal deleted it, so the
  host interface now refuses removal while a handle to it is open (removing the driver with a
  host attached is "removed on reboot"; forcing a detach instead is for M6), and an `ATTACH`
  racing a `DETACH` on the same handle needed an "attaching" state not to leak the event.
  The generation is 16 bits wide in the driver while the ABI carries 32: settled at M4's close,
  the ABI keeps 32 bits and documents that the driver uses 16, which wrap; a host only compares
  generations for equality.
  *The jack follows the host (2026-11-09):* the topology's jack reports `IsConnected` from
  `HostLink` (connected while a host is attached) with
  `JACKDESC2_PRESENCE_DETECT_CAPABILITY`, and a filter-level `KSEVENT_PINCAPS_JACKINFOCHANGE`
  event, as in Microsoft's sysvad: the miniport keeps a reference to its port's `IPortEvents`
  for the event's enables, `HostLink` keeps another (rundown protection for its users, a
  guarded mutex for its changes, which the topology's `Init` and `Drop` may make concurrently
  across a restart) and generates the event on attach and detach. In the VM the endpoint's
  `DeviceState` is 8 (unplugged) without a host, 1 (active) with `test-host` attached
  (`capture-check` still `PASS`), 8 again after detach or a killed host; `devcon remove`
  unloads the driver, so the counted `IPortEvents` makes no lasting cycle (PortCls releases the
  miniport when the filters stop). Lessons: the endpoint builder honours `IsConnected` for an
  integrated device; PortCls installs no fast I/O table (logged at load); the review moved the
  event's enables to the miniport's own port, since an old miniport's enable must never land
  in a newer port's list.
  *The shared buffer and its mapping (2026-11-09):* `driver/src/shared_buffer.rs` allocates the
  buffer at the adapter's first start (192512 bytes, 47 zeroed pages); each stream's
  `AllocateAudioBuffer` returns a partial MDL over its start, sized up by
  `buffer::geometry` (an exclusive-mode request beyond one second fails); `ATTACH` zeroes it,
  maps it into the requesting process (`hostmic-sys/c/map_user.c`, the driver's only C) and
  returns its address. `HostLink` undoes the mapping on detach, cleanup and close, attaching
  to the host's process when another one closes the handle, and from
  `PsSetCreateProcessNotifyRoutineEx` when the host's process exits; the driver is linked with
  `/INTEGRITYCHECK`, which that routine requires. The driver still writes its tone, now into
  the shared pages: `tools/test-host` reads it through its own mapping. In the VM: the audio
  engine accepts the miniport's own partial MDL (`capture-check` `PASS`, 0 glitch); the host
  reads peaks of -6.02 dBFS while the stream runs; a host killed while another process holds
  a duplicate of its handle is detached by the process notification, and the next host
  attaches; when that other process closes the last handle while the host lives, the host's
  mapping disappears (`VirtualQuery`); `devcon remove` unloads the driver. Lessons: the
  independent review found that removing an adapter that never started would wait forever on
  an uninitialized mutex, and that a removal with a host attached left its mapping behind
  (now detached at removal); the driver writes the shared pages only as aligned 32-bit atomic
  stores, never through a non-atomic Rust access, since the host, the audio engine, the timer
  and `ATTACH` share them. A first version of that change, written inline in the driver,
  subtracted without saturation and bug-checked the VM (0x1E, a debug build's overflow
  panic): the frame arithmetic went back to `hostmic-driver-core` (`ring::tone_words`), where
  it is unit-tested. A second review found the link's "started" state inferred from a cursor
  value of 0, which a wrapped generation can reproduce: it is now a flag of its own. (The
  shared-pages tone and `ring::tone_words` left with the next pull request.) Still
  open: an adapter removed while a handle to the host interface stays open (forced detach,
  M6).
  *The host advances the position (2026-11-09):* the driver produces no audio and keeps no
  clock: its timer and tone are gone (`ring` and `tone` left `hostmic-driver-core`).
  `ADVANCE` (`HostLink::advance`) accepts only the attached handle in the host's own process,
  validates against the packed cursor and commits with a compare-and-exchange, then signals
  the engine's event for each completed packet. `GetReadPacket` hands out the oldest complete
  packet still intact (`packet::next_read`), counting the frames the host may already be
  writing (the size of its last advance) as overwriting, with `MoreData` and the skipped
  packets counted; its performance counter value is counted back from the last advance of the
  current run (`clock::sampling_instant`). Refusals that are races (stale generation or
  position, stream no longer running) complete with `STATUS_DEVICE_NOT_READY`
  (`ERROR_NOT_READY`) and are not logged; others are logged at a bounded rate. `GET_STATS`
  and `tools/hostmic-ctl status` show the counters; `tools/test-host` plays the tone from an
  MMCSS thread paced by the performance counter. New fuzz target: `read_packet`. In the VM,
  with the final driver: `capture-check` records the host's tone for 30 s, 1000.00 Hz, -6.02
  dBFS, 0 glitch, no discontinuity, `PASS`, with the host's longest gap between two writes at
  2.6 ms, no packet skipped and no request refused; a host killed mid-capture leaves the
  system up (the capture ends with `AUDCLNT_E_DEVICE_INVALIDATED`, the microphone shows
  unplugged), and a new host right after passes again; `devcon remove` unloads the driver. One
  of the six "killed, then a new host" captures reported `FAIL` with a clean level and no
  glitch; its reason line was not kept and the next five passed: to watch for. Lessons:
  the first two runs failed on one discontinuity each, which the test host's own counters
  traced to itself: its real-time loop wrote its progress to a redirected file, and one write
  blocked for 45 ms, more than the 20 ms shared-mode buffer; printing moved to another thread.
  The real-time rule applies to hosts as much as to the driver, and the host library (M5)
  must enforce it. The independent reviews found the packet numbering able to survive a stop
  that raced `GetReadPacket` (now a run epoch packed with the last packet read, moved by one
  compare-and-exchange), packets dated from the wrong run, and the frames written ahead of an
  advance ignored by the "intact" check.
  ***M4 done (2026-11-09).*** A user-mode host now feeds the microphone: it finds the host
  interface by GUID (a raw child device with its own ACL), attaches, writes into a buffer the
  driver maps into it and shares with the audio engine, and sets the pace with `ADVANCE`; the
  driver produces no audio and keeps no clock, and the microphone shows unplugged without a
  host. Pull requests: the ABI and its validation (#27), fuzzing in CI (#28), the child device
  (#31), attach and the stream state (#32), the jack (#33), the shared buffer and its mapping
  (#34), the host-advanced position with the counters and `hostmic-ctl status` (#35). The
  "done when" holds: `tools/test-host` plays a tone that `tools/capture-check` records cleanly
  (30 s, 1000.00 Hz, -6.02 dBFS, 0 glitch, no discontinuity); the microphone is unplugged
  without a host; killing the host mid-capture leaves the system up and the next host plays
  again; fuzzing runs on every pull request (seven targets, required check `fuzz`). Each
  kernel pull request had an independent review, and each review found real defects; two of
  them were in the reviews' own fixes, and one of those bug-checked the VM (an overflow in a
  debug build): every fix is reviewed and tested again before merging. Carried over:
  - to M5: the host library enforces the real-time rule the test host had to learn (no I/O,
    lock or allocation in its audio loop; an MMCSS thread; pacing by the performance counter;
    `ERROR_NOT_READY` means "read the stream again"), and reports how late it ever was;
  - to M6: a removal while a handle to the host interface stays open waits for a reboot. A
    forced detach is ruled out since M5: the host writes into the mapping without asking the
    driver, so the mapping lasts until `DETACH` from the host or the handle's close
    (`AttachReply::buffer`); hosts on the crate release their handle on a query-remove (M5's
    device notifications), others keep the removal waiting. Driver Verifier over attach, advance,
    detach and host death; one isolated `capture-check` `FAIL` after a killed host (clean
    level, no glitch, reason not kept, not reproduced in five runs);
  - open question: the `PROPOSEDATAFORMAT` probe (below).
- **M5, the host crate:** `hostmic` API, reference host loop, examples; latency and load tests
  ported from Résonnance (`tag_micro`): write → read, discontinuities under load, endurance.
  The reference loop follows M4's lessons (above): `tools/test-host`'s player is its first
  draft.
  *Design (2026-11-09):*
  - **What the first draft gets wrong for a library:** its real-time thread reports through an
    `mpsc` channel (which allocates, as does its `Note::Thread(String)`); it paces itself with
    `WaitForSingleObject(event, 1)` under `timeBeginPeriod(1)` (2.6 ms longest gap in M4's
    run); its errors are strings; its format (48 kHz, 16-bit, stereo) and its tone writer are
    hard-coded; it never reverts its MMCSS registration.
  - **Crate `hostmic`** (`host/`, MIT OR Apache-2.0, on `hostmic-abi`), Windows only for now,
    its names free of Windows terms so that a PipeWire backend can come later (open question
    below). Two layers.
  - **Low layer, the protocol made safe**, for hosts with a loop of their own (Résonnance
    later, clocked by its physical microphone): `interfaces()` lists the host interfaces;
    `Device::open` opens one, `info()` and `stats()` read it; `attach(FormatRange)` returns a
    `Host` that owns the handle, the event and the mapping, and detaches on drop;
    `Host::stream()` reads `GET_STREAM`; `Host::write(&stream, from, samples)` copies the
    samples into the ring at the position, wrapping, then sends `ADVANCE`, and returns
    `Advanced(position)` or `Stale` (`ERROR_NOT_READY`: read the stream again). Writes into the
    shared pages are volatile, never through a `&mut [T]`: the audio engine reads them at the
    same time. Errors are typed: `NotInstalled`, `Busy` (another host is attached),
    `Version { driver, host }`, `NoCommonFormat`, `Refused(Reason)`, `Os`.
  - **High layer, the reference loop:** `Player::start(host, callback)` runs a thread
    registered with MMCSS ("Pro Audio"), reverted on exit, paced by the performance counter
    and a high-resolution waitable timer (`CREATE_WAITABLE_TIMER_HIGH_RESOLUTION`), waiting on
    the stream's event, the timer and a stop event at once. Each period it calls the caller's
    `FnMut(&mut Block)` with the format, the position and a preallocated `f32` block of the
    frames due, converts it to the stream's sample type (clamped) and writes it with the low
    layer, about a millisecond per `ADVANCE` as the ABI asks. More than a buffer late, it
    counts the event and anchors its clock again. No allocation, lock or I/O in that thread:
    it publishes atomic counters (frames written, advances, stale refusals, late events,
    longest gap between two writes, stream state) that `Player::stats()` reads from any
    thread. `Player::stop()` signals the stop event.
  - **Device notifications** (`CM_Register_Notification`): wait for the interface to arrive;
    on a query-remove of the host interface, detach and close the handle, so that removing or
    updating the driver no longer waits for a reboot while a host runs (a host that keeps its
    handle keeps the removal waiting: the driver never unmaps under a live host); attach again
    when it comes back.
  - **Pure parts** (frames due, re-anchoring, the ring's wrap, sample conversion) in a module
    without system calls, unit-tested by the `driver` workflow's `cargo test` on Windows, as
    `hostmic-driver-core` is for the driver.
  - **Measurements**, in the test VM: write → read latency, discontinuities under load (16
    saturated threads, 30 s), endurance. The target table (above) was measured on a real
    machine; until a signed driver can run there (M7), VM figures are indicative.
  - **Not in M5:** the C header (after M8, once a real host has settled the API); a
    caller-driven clock with a correction ratio; the PipeWire backend.
  Pull requests, one at a time: this design; the low layer, with `tools/hostmic-ctl` moved
  onto it; the reference loop and an example, with `tools/test-host` rewritten on it (done
  when `capture-check` passes and the longest gap is below M4's 2.6 ms); device
  notifications (done when `devcon remove` unloads the driver while a host is attached, and the
  host attaches again after reinstallation); the measurements; the closing notes.
  *The low layer done (2026-11-09):* crate `hostmic` (`host/`): `interfaces`, `Device` (read),
  `Host` (attach, stream, stats, wait, event, write, detach; `Drop` sends `DETACH`). `attach`
  reads `GET_INFO` first and refuses another ABI version or a range without a common format
  itself: the driver tells refusals apart by status only (`STATUS_DEVICE_BUSY` for `Busy`,
  `STATUS_DEVICE_NOT_READY` for a stale `ADVANCE`, `STATUS_INVALID_DEVICE_STATE` for
  `NotHost`), so the design's `Refused(Reason)` became those errors. `write` copies frames
  into the ring with volatile byte stores and sends `ADVANCE`, allocating nothing even on its
  error paths (`windows` builds its errors without `ErrorInfo`); a stream that does not accept
  frames and a stale request are outcomes (`Advance::NotRunning`, `Advance::Stale`), not
  errors. `FormatRange::is_valid` and `intersect` moved from `hostmic-driver-core` to
  `hostmic-abi`, so that a host checks a range as the driver does; `StreamState::generation`
  documents its parity (even: frames accepted) as part of the contract. `tools/hostmic-ctl`
  uses the crate and no `unsafe`. In the VM: `hostmic-ctl info` and `status` answer through it,
  `test-host` (not yet on the crate) plays a tone that `capture-check` records cleanly (20 s,
  1000.00 Hz, 0 glitch, no discontinuity, `PASS`, no request refused), and `hostmic-ctl info`
  reports the missing driver after `devcon remove`, which unloads it. Lessons from the
  independent review: dropping a `Host` must send `DETACH` (closing the handle detaches only
  when no other process holds a duplicate); the review also found `DETACH` accepted from any
  process holding the host's handle, which can unmap the buffer from under a live host (its
  own pull request), and the host interface refusing every open after a disable and enable
  in Device Manager (its own pull request).
  *Only the host's process detaches (2026-11-09):* the host writes into the mapping without
  asking the driver, so the ABI now states how long it lasts (`AttachReply::buffer`): until
  `DETACH` from the host's own process, the close of every handle to its file object, or the
  host's exit; nothing else undoes it. `DETACH` was accepted from any process holding the
  host's handle (an inherited one, say) and now requires the host's process, as `ADVANCE`
  does; the handle's cleanup and close still detach from whatever process closes the last
  handle. The adapter's removal cannot find a host whose handle is open: the PnP manager sends
  `IRP_MN_REMOVE_DEVICE` to a device's children first ("Handling an IRP_MN_REMOVE_DEVICE
  Request"), and the host interface's device gets it only once its handles are closed (it
  refuses a query-remove while one is open; after a surprise removal the PnP manager waits for
  the handles); `HostLink::shutdown` still undoes such a mapping and logs it, should it ever
  happen. M4's forced detach for M6 is dropped (above). New `test-host --detach-from-child`: a
  copy of `test-host` inherits the host's handle and sends `DETACH` with it. In the VM, three
  runs: the child's `DETACH` refused (`NotHost`, `0xC0000184`, one request refused), the host
  playing to the end and detaching itself; `capture-check` `PASS` twice (0 discontinuity, 0
  packet skipped) and `FAIL` once, on the first run, 15 s after restoring the VM's checkpoint
  (15 discontinuities, 17 packets skipped: the host got more than a buffer ahead of the engine),
  with the same binaries; like M4's isolated `FAIL`, to watch with M5's measurements.
  *The host interface opens again after a disable and enable (2026-11-09):* disabling it in
  Device Manager sends an accepted query-remove, then a removal (the PDO is kept, the adapter
  still reports it); enabling it sends a start, with no cancellation in between. The
  `remove_pending` flag the query sets was cleared only by a cancellation (since #32), so after
  the restart every open failed with `STATUS_DELETE_PENDING` (`ERROR_ACCESS_DENIED` in user
  mode) until the adapter's removal; the start now clears it, next to `removed`. In the VM,
  with `Disable-PnpDevice` and `Enable-PnpDevice`: `main`'s driver refuses `hostmic-ctl info`
  after the enable (access denied), the fixed one answers, and `test-host` attaches and
  detaches; `devcon remove` unloads both.
  *The reference loop (2026-11-09):* `hostmic::Player` (`host/src/player.rs`) runs a thread
  registered with MMCSS ("Pro Audio", reverted on every exit, a panic included) that owns the
  `Host` and waits on the stop event, the stream's event and a high-resolution waitable timer
  (Windows 10 1803 and later, required), armed at 1 ms only while the stream accepts frames.
  Each tick it computes the frames due by the performance counter (`clock`), has the callback
  fill them in one `f32` block (`Block`: format, position, the performance counter value its
  first frame is due at, for a host comparing clocks), converts them (`convert`: clamped, NaN
  as 0) and writes them in near-equal parts of one to two milliseconds, each copied just before
  its own `ADVANCE`: the driver takes the size of the last `ADVANCE` as how far the host writes
  ahead, so a late tick sent as one large request made it skip a packet the engine could still
  read (the independent review's main finding; the first split, into fixed 1 ms chunks plus a
  remainder, doubled the requests with 1-frame tails). `PlayerStats` (atomic counters, read from
  any thread): frames written, advances, stale refusals, late events, longest gap between two
  writing wake-ups, stream state, MMCSS, finished. `examples/tone.rs` plays a tone;
  `tools/test-host` plays capture-check's tone through the `Player` and follows the stream
  through a read-only `Device`; its handle-rule modes moved to `tools/handle-check` (raw
  requests: the crate's `Host` always detaches on drop), with a new `--close` (the only handle
  closed without `DETACH`, while the host lives) replacing `test-host --no-detach`;
  `--close` and `--detach-from-child` now check their own outcome. In the VM, three runs of
  `test-host` with a 30 s `capture-check`: `PASS` each time (1000.00 Hz, 0 glitch, no
  discontinuity), 0 late, no packet skipped, no request refused, about 70 frames per `ADVANCE`
  with the final split; the longest gap was 2.5, 2.6 and 8.7 ms. The "below 2.6 ms" criterion
  is therefore not met reliably: a single maximum in a VM measures the hypervisor's scheduling
  as much as the loop (the 8.7 ms gap cost nothing: the shared-mode buffer is 20 ms). The
  measurements pull request reports the gap's distribution instead of one maximum.
  *Device notifications (2026-11-09):* `Player::follow(formats, callback)` attaches by itself
  and follows the first microphone, with `CM_Register_Notification` as Microsoft's "Registering
  for Notification of Device Interface Arrival and Device Removal" describes: interface
  arrivals, and the device behind the attached handle. On a removal request
  (`DEVICEQUERYREMOVE`) the handle must be closed before the callback returns, but the player's
  thread is the one writing into the mapping: the callback (a system thread) signals it and
  waits, up to 2 s, while the thread detaches and closes its handle. Until the removal is
  decided (cancelled, pending or complete) the thread does not attach again: its first version
  reattached at once, before the driver's query-remove, and its new handle made the driver
  refuse the removal (seen in the VM, "Removed on reboot", and found by the independent review
  at the same time). The registration on the device is kept across reattachments (it stays
  valid after its handle closes) and dropped, from the player's thread, once the device is
  gone: unregistering waits for the callbacks in flight, so it never happens while one waits
  for that thread. `Player::stop` now returns `Option<Host>`; `PlayerStats` gains `attached`,
  `attachments`, `releases`. `tools/test-host` and the example follow the microphone; test-host
  no longer keeps a read-only handle, which would hold a removal back. A kernel change first
  planned with this one (enabling the interface after marking the device started) was dropped:
  the PnP manager neither announces an interface nor lets it open before
  `IRP_MN_START_DEVICE` completes (`IoSetDeviceInterfaceState`'s remarks). In the VM, while
  `test-host` plays: `devcon remove` removes the device and unloads the driver; after the
  reinstall the host attaches again and `capture-check` passes; disabling and enabling the
  host interface makes it let go and attach again (`capture-check` `PASS`); 3 attachments, 2
  releases, no packet skipped, no request refused.
  *The measurements (2026-11-09):* `tools/measure`, after Résonnance's `tag_micro` (its
  measuring method only: nothing of Thin Audio Gateway's API). One process plays into the
  microphone with `Player::follow` and records it as a communication application does
  (shared-mode, event-driven WASAPI on an MMCSS thread, float). `measure latency`: a 1 ms click
  every half second; the callback notes the performance counter when it writes a click's first
  frame, the reader when it gets the packet with the click's rising edge (packetization
  included, as an application sees it); median as `tag_micro` computes it, 95th percentile,
  maximum. `measure load`: a 440 Hz sine while 16 threads spin; crackles by `tag_micro`'s
  criterion (a sample the sine does not predict, or 2 ms of silence). Both fail on an engine
  discontinuity, a timed-out wait, less than 90 % of the frames due, and, per mode, under 80 %
  of the clicks due or 90 % of audible signal (the independent review found that the first
  version passed a silent or stalled microphone). `PlayerStats::gaps` counts the gaps between
  writing wake-ups in buckets (`GAP_EDGES_US`, from 1.5 ms: the timer ticks every millisecond),
  the distribution the reference loop's note asked for. In the VM: latency 58 of 58 clicks,
  **median 10.1 ms**, 95th percentile 10.7 ms, maximum 10.8 ms (the target table: 9.6 to
  10.8 ms); load, 16 threads, 30 s: **0 crackle**, 0 late, longest gap 1.6 ms, 98.8 %
  audible; endurance, load for 10 minutes: **0 crackle**, 0 late, 0 discontinuity, 0 packet
  skipped, 99.9 % of the frames due read, 14 of 413576 gaps above 2 ms, the longest 4.5 ms.
  The gaps sit mostly in 1.5-2 ms (the VM's 1 ms timer ticks about every 1.46 ms), none above
  3 ms in the 30 s runs. In a VM; the comparison with the target table, which was measured on
  a real machine, is for M7, with a signed driver.
  ***M5 done (2026-11-09).*** Hosts no longer speak the protocol themselves: the crate
  `hostmic` (`host/`) finds the microphone, attaches, writes into the shared buffer and
  advances the position safely (the low layer, for hosts with a loop of their own), and
  `Player` runs the reference loop (MMCSS, a high-resolution timer, pacing by the performance
  counter, no allocation, lock or I/O, atomic counters), following the microphone as Windows
  adds and removes it. `tools/hostmic-ctl`, `tools/test-host`, `tools/handle-check` and
  `tools/measure` are built on it. Pull requests: the design (#49), the low layer (#50), only
  the host's process detaches (#51), the host interface opens again after a disable and enable
  (#52), the reference loop (#53), device notifications (#54), the measurements (#55). The
  "done when" items, in the VM:
  - `test-host` plays through the `Player` and `capture-check` passes (three 30 s runs, 0
    glitch, no discontinuity, 0 late);
  - the longest gap below 2.6 ms is **not met reliably** (2.5, 2.6 and 8.7 ms in three runs): a
    single maximum in a VM measures the hypervisor's scheduling as much as the loop. The
    distribution replaced it (`PlayerStats::gaps`): none above 3 ms in the 30 s runs, 14 of
    413576 above 2 ms in 10 minutes under load, the longest 4.5 ms, with no consequence (the
    shared-mode buffer is 20 ms);
  - `devcon remove` while a host plays removes the device and unloads the driver, and the host
    attaches again after the reinstall; disabling and enabling the host interface makes it let
    go and attach again;
  - the measurements meet the target table: write → read median 10.1 ms (9.6 to 10.8 ms), 0
    crackle under load for 30 s and for 10 minutes.
  Each pull request had an independent review, and the reviews found real defects again: a
  `DETACH` accepted from another process holding a duplicate of the host's handle (#51), the
  host interface refusing every open after a disable and enable (#52), a late tick sent as one
  large `ADVANCE` that made the driver skip a packet (#53), a host attaching again before the
  driver's query-remove and so holding the removal back until a reboot (#54, also seen in the
  VM), and a measurement verdict that passed a silent or stalled microphone (#55). Carried
  over:
  - to M6: the gaps' tail in a VM (hours of `measure load`, with the distribution); Driver
    Verifier while a crate host attaches, follows a removal and dies; the isolated
    `capture-check` `FAIL` after a checkpoint restore (#51), not seen in M5's later runs;
  - to M7: `measure` on a real machine, against the target table, once the driver is signed;
  - to M8: Résonnance on the low layer, clocked by its physical microphone; the caller-driven
    clock with a correction ratio, and the C header, once that host has settled the API;
  - open question: the PipeWire backend (below).
- **M6, hardening:** security review against the rules above, Driver Verifier, HVCI, sleep and
  resume, host crash, several hours of endurance; M4's and M5's carried-over items (above).
  *Design (2026-11-09):* the driver under the conditions it must survive before it is signed
  (M7), with evidence anyone can reproduce in the test VM rather than one-off runs.
  - **A test campaign in the repository** (`tools/vm/`, PowerShell, run in the VM): it
    installs a package, runs every scenario and prints one `PASS` or `FAIL` line each, with the
    reason. Scenarios: install and remove (the driver unloads); `capture-check` with
    `test-host`; `measure latency` and `measure load`; a host killed mid-stream, N times at
    random moments, each followed by a new host; `handle-check`'s modes; `devcon remove` while
    a host plays, then reinstall (the host attaches again); disabling and enabling the adapter
    and the host interface; restarting the audio service while a host plays. It replaces the
    ad hoc scripts of M4 and M5, and runs on both the debug package and the release package
    (the one M7 submits for signing, built by CI).
  - **Driver Verifier** on `hostmic_driver.sys` ("Driver Verifier", "Driver Verifier
    options", "Rules for audio drivers" on Microsoft Learn): the standard rule classes (special
    pool, IRQL checking, pool tracking, I/O verification, deadlock detection, security and
    miscellaneous checks, DDI compliance; not DMA nor WDF verification, which do not apply to a
    WDM driver without DMA) and the additional DDI compliance rules, port/miniport interface
    checking (`/rc 17`: PortCls's ten audio rules, `PcAllocateAndMapPages` and
    `PcUnmapAllocatedPages` among them) and code integrity checking (flag `0x02000000`:
    executable pool or mapping, writable and executable sections). The whole campaign passes
    with it on; pool tracking bug-checks an unload that leaks. A separate run with randomized
    low-resources simulation (`/rc 3`): allocations that fail must fail requests, never the
    system. `portcls.sys` and `ks.sys` are verified along with it in one run, for I/O
    verification across the stack. This makes the badge's `dynamic_analysis_unsafe` met.
  - **Device Fundamentals tests**, from the WDK (TAEF's `TE.exe`, WDTF, the `Devfund_*.dll`
    under `Testing\Tests\Additional Tests\x64\DevFund`), run standalone in the VM ("Run
    DevFund tests via the command line"): PnP (`PnPDTest`: disable and enable, remove,
    cancelled remove, rebalance, surprise removal), reinstall, and IOCTL fuzzing of the host
    interface in the kernel (`FuzzIoctlTest`), which `cargo-fuzz` cannot do: it fuzzes the
    pure validation code only. WDTF's audio I/O plugin needs a render endpoint, which the VM
    has none of, so the tests run with WDTF's generic I/O; `capture-check` covers the audio.
  - **Power:** a Hyper-V guest offers neither S1-S3 nor modern standby; hibernation needs an
    undocumented VM setting. The transition that matters on a real machine is the device's
    own D0 → D3 → D0: PortCls's idle timer ("Audio Device Class Inactivity Timer
    Implementation") is off by default (`ConservationIdleTime` and `PerformanceIdleTime` 0);
    the INF sets it (`PowerSettings`, a few seconds, `IdlePowerState` 3), and the campaign
    checks the device idles to D3 without a stream and streams again after. Adapter power
    management (`IAdapterPowerManagement`, absent since M3) is added only if those tests need
    it. Hibernation (`pwrtest /sleep /s:4`) is optional, if the maintainer enables it on the
    VM.
  - **HVCI:** Microsoft lists Secure Boot among VBS's requirements, and test signing needs it
    off. The VM tries Memory Integrity anyway (registry, then `Win32_DeviceGuard`'s
    `SecurityServicesRunning`); if it does not run, M6 relies on Driver Verifier's code
    integrity checking, and the run with Memory Integrity on moves to M7, with a signed driver
    and Secure Boot on. Microsoft's `hvciscan` (a separate download) checks the binary
    statically, with the maintainer's agreement to download it.
  - **Endurance:** 4 hours of `measure load` under Driver Verifier, with the gaps'
    distribution.
  - **Crash analysis:** kernel memory dumps in the VM (`C:\Windows\MEMORY.DMP`), read in the
    VM with the WDK's debugger (`kd -z`, `!analyze -v`, `!verifier`): `Copy-VMFile` copies
    only from the host into the VM; a kernel debugger over the network (KDNET) only if a dump
    is not enough, since it needs an external virtual switch on the maintainer's machine.
  - **Overflow checks in release** (`overflow-checks = true` in `[profile.release]`): the
    arithmetic already saturates or is checked; if one ever wraps, the release driver stops
    with a bug check that names the place instead of corrupting memory silently, as the debug
    build does.
  - **Security review** against the rules above and `docs/security-assurance.md`'s weakness
    table, one trust boundary at a time (Kernel Streaming, the host interface, the shared
    memory, PnP and teardown), each by an independent review; each defect gets its own pull
    request, and the assurance case is updated.
  - **Carried over from M4 and M5:** Driver Verifier over attach, advance, detach and a host's
    death, and with a crate host following a removal (the campaign); the gaps' tail over hours
    (endurance); the isolated `capture-check` `FAIL`s (the campaign also runs right after a
    checkpoint restore, where #51's appeared).
  - **Not in M6:** HLK certification (WHCP), Static Driver Verifier (no Rust support), ARM64,
    a real machine (M7).
  Pull requests, one at a time: this design; the campaign (`tools/vm/`, `docs/test-vm.md`);
  overflow checks in release; Driver Verifier and its fixes; Device Fundamentals; power; HVCI;
  endurance; the security review (a pull request per defect); the closing notes. Done when the
  campaign and the Device Fundamentals tests pass under Driver Verifier on the debug and
  release packages, the device idles to D3 and streams again, 4 hours of load show 0 crackle,
  and every security finding is fixed or recorded with its reason.
  *The test campaign (2026-11-09):* `tools/vm/campaign.ps1` (Windows PowerShell 5.1, the VM's,
  ASCII only) runs ten scenarios (`install`, `tone`, `latency`, `load`, `kills`, `handles`,
  `removal`, `disable`, `audiosrv`, `uninstall`; docs/test-vm.md, section 8), prints one
  `PASS` or `FAIL` line each with the failed check and its log, and exits with the number of
  failures; its header says whether Driver Verifier verifies the driver and whether Memory
  Integrity runs, so that a run's output states its conditions. `tools/vm/stage.ps1` copies a
  package, the release tools, the WDK's `devcon` and the campaign into the VM. New checks
  beside M3 to M5's procedures: ten hosts killed at random moments (the microphone unplugged
  after each), disabling the adapter while a host plays, restarting the audio service. In the
  VM, the final version: 10 of 10 on the local debug package (twice) and on the release
  package of `main`'s CI run (latency median 9.4 ms, 0 crackle under load, 1000.00 Hz and 0
  glitch in every recording), as earlier versions had on both. The independent review found
  two checks that could pass without their behaviour (the last handle's unmapping without
  first seeing the mapping last while the holder lived; a `devcon remove` that removed nothing
  while the driver was already stopped), waits satisfied by the endpoint's stale state from
  the previous host, and a failed `disable` able to leave a device disabled for the next runs.
  Lesson from the fixes' first run: a removed endpoint is "not present" (`DeviceState` 4),
  never "unplugged" (8); a scenario that fails between a removal and a reinstall now reinstalls
  the driver, so that one failure does not fail every scenario after it.
  *Overflow checks in release (2026-11-09):* `overflow-checks = true` in `[profile.release]`,
  so the build a release ships checks arithmetic as the tested debug build always did. A panic
  in the driver is `wdk-panic`'s bug check `RPNC` with the file and line in its parameters, so
  a wrapped size or position stops the system where it happened instead of corrupting memory
  further on; the tools and hosts built in release abort. The checks stay only where the
  compiler cannot prove the operation in range: the release driver grows from 72192 to 73728
  bytes and gains two panic locations, `hostmic-driver-core`'s `packet.rs` and the driver's
  `com.rs` (reference counts). In the VM, the campaign passes 10 of 10 on this release package
  (latency median 10.8 ms, 0 crackle under load).
  *Driver Verifier (2026-11-09):* `tools/vm/verifier.ps1` sets the profiles for the next boot
  only (`/bootmode oneboot`) with a kernel dump: `standard` (`hostmic_driver.sys`, rule classes
  1 2 4 5 6 9 12 17 18 20 26), `stack` (the same on `portcls.sys` and `ks.sys` too) and
  `low-resources` (class 3); the campaign's `install` line shows the flags and drivers
  `verifier /query` reports, as they are. The first run found a defect: under code integrity
  checking every `ATTACH` was refused, so no host could play. `hostmic_map_user` mapped the
  shared buffer into the host without `MdlMappingNoExecute`; Microsoft documents a user-mode
  mapping as never executable and the flag as unnecessary, but Driver Verifier (and HVCI
  alike) refuses the request ("specified an executable mapping") by raising a breakpoint, and
  the wrapper's `__except (EXCEPTION_EXECUTE_HANDLER)` caught it and returned null, which the
  driver reported as `STATUS_INSUFFICIENT_RESOURCES`. The wrapper now passes the flag, leaves
  breakpoints and assertion failures to the system (a Driver Verifier violation stops it, as
  intended) and returns the exception's status, which the refusal carries to the log and the
  host. In the VM: `standard` and `stack` pass 10 of 10 on the debug and the release packages
  (no bug check; latency median 10.4 to 10.9 ms, 0 crackle under load); `low-resources` (6 %
  of the driver's allocations failing from 8 minutes after the boot, 5 failed deliberately
  during the run) passes 9 of 10 with no bug check: in `kills`, a host's
  `CM_Register_Notification` on its handle failed (`CONFIGRET 0x1f`) because the driver's
  answer to `TargetDeviceRelation` could not be allocated, and `Player::follow` ended on that
  error; the next scenarios attached and played. The driver failing a request on an
  allocation failure is right; a player that ends on a transient failure is not (next: retry
  the registration rather than end). Lessons: `Restart-VM` resets the VM (its default
  `-Type`), and Driver Verifier then came back with an earlier profile's settings, so two
  runs believed to be `stack` and `low-resources` were `standard` (found by `verifier /query`;
  the VM restarts from Windows now); `/bootmode resetonbootfail` behaved the same way. The
  independent review found the boot loop a persistent profile could cause, the campaign's
  header showing the next boot's settings as the current ones, and assertion failures still
  swallowed by the wrapper. The badge's `dynamic_analysis_unsafe` is met.
  *A following player registers before it attaches (2026-11-10):* `Player::follow` ended when
  its registration for the microphone's removal requests failed (Driver Verifier's
  low-resources run above). It now registers first, on a handle of its own opened without
  `ATTACH` and closed at once (a registration on a device outlives its handle), and attaches
  only once registered: a failure leaves the microphone alone (no attach and detach, no jack
  change) and is retried a second later, counted in `PlayerStats::registration_failures`
  (printed by `test-host` and `measure`); 60 failures in a row (a minute) end the loop with the
  error, so that a lasting defect still shows. A removal requested while registering is not
  followed by an attach. In the VM, under `low-resources` with 40 killed hosts, no bug check in
  any run: 10 of 10 (before: 9 of 10), several hosts registering at their second try, 46
  allocations failed deliberately; a second run of the final version, 59 failed: 7 of 10, the
  failures not the host's (`capture-check`'s stream refused, "insufficient system resources";
  after disabling and enabling the adapter, no host interface came back, so `disable` and
  `audiosrv` found no microphone to attach to, with no registration failure). To examine with
  the security review: whether the adapter should fail its start when it cannot create the host
  interface's device, so that the PnP manager tries again, rather than run without it until the
  next restart. The independent review found that a first version, which
  retried after attaching, made the jack flap every second under lasting pressure and
  swallowed every registration error.
  *Device Fundamentals tests (2026-11-10):* `tools/vm/devfund.ps1` runs the WDK's DevFund tests
  with TAEF in the VM (`stage.ps1 -DevFund` copies the kit; docs/test-vm.md, section 10). Under
  Driver Verifier (`standard`), on the adapter: the 8 PnP tests pass (disable and enable, remove
  and restart, cancelled remove and stop, stop and restart, failed restart, surprise removal,
  each with Microsoft's test filter on the device, then the filter removed); the IOCTL fuzzer
  (6 tests, function codes 0 to 15, Kernel Streaming's, about 6 minutes) and the API fuzzer
  (5 tests) pass on its Kernel Streaming filters; the reinstall with I/O passes; no bug check.
  The host interface is tested by none of them: WDTF's query finds it (`DeviceID='…'`, one
  device), but each test's own selection leaves it out, presumably as a raw device without a
  function driver, and passes all the same ("No devices were found for testing"): the script
  calls that SKIP. A first run read those passes as 16 PnP passes on both devices; the
  stricter verdict below exposed them. So the host interface's PnP paths are tested only by the
  campaign (removal, disable and enable, killed hosts), and its IOCTLs are not fuzzed in the
  kernel: the security review is to cover them, with a fuzzer of our own sending random
  requests to the interface (the validation itself stays fuzzed by `cargo-fuzz`). Also found:
  the host interface reports instance ID "0" without `UniqueID`, so its devices are named
  after the adapter's devnode, which every `devcon remove` and reinstall replaces; 53 phantom
  host interface devices had piled up in the VM. Harmless, but a stable unique ID would avoid
  it: for the security review too. Lessons, on the way to a working run: WDTF's content
  installers must come before its product's, whose custom actions install the filter drivers
  from the content's files (the other order fails with "Failed to install IOSPY filter
  driver"); TAEF blocks a test that may restart the machine unless its service can resume it
  (`/RebootStateFile`); the IOCTL fuzzer tries all 4096 function codes by default (hours), and
  a timed-out `TE.exe` leaves its test host fuzzing alone. The independent review found false
  passes in the first version of the verdict (a class with one test blocked and the others
  passed, a timed-out run, a device query matching nothing, a driver left installed by an
  earlier run tested instead of the package's) and no cleanup when a run throws: a test now
  passes only when every test ran and passed (or was skipped) and TE.exe exited with 0, the
  script checks that WDTF finds each device first, and a `finally` removes the driver, the test
  filter with it, whatever happened.
  *Power (2026-11-10):* the INF turns PortCls's inactivity timer on (`PowerSettings`: 5 s on
  battery and on AC power, `IdlePowerState` 3). hostmic has no hardware to save power on, and
  the release package that M7 signs keeps the 5 s on purpose: every stream start after an idle
  period goes through D3 and D0, the path a real machine takes, instead of a path tried once
  in a while. No driver code: PortCls handles the adapter's power requests without
  `IAdapterPowerManagement` (not needed, as the design expected), no miniport code runs on a
  power change, and a running stream stays in D0 because the audio engine sends a request every
  period. The campaign's `power` scenario reads the adapter's `DEVPKEY_Device_PowerData`: D3
  without a host; with a host attached, the audio service's requests wake the adapter (it must
  be seen out of D3) and it idles again; D0 throughout a 15 s `capture-check` (1000.00 Hz, 0
  glitch); D3 again after it, the host still attached. In the VM under Driver Verifier
  (`standard`), the campaign passes 11 of 11 on the debug and the release packages, and on the
  debug package again after the review's fixes. The independent review found one check that
  could pass without its behaviour (the adapter idling again after the attach, without having
  been seen woken) and slow sampling (each read enumerated every device); both fixed. Left for
  the security review (PnP and teardown): the host interface's PDO completes `IRP_MN_SET_POWER`
  without `PoSetPowerState`, and, as its power policy owner, requests no device power IRP on a
  system power change; nothing reaches that path in the VM (the idle timer is the adapter's,
  and a Hyper-V guest has no sleep state), but sleep and hibernation on a real machine (M7)
  do. Hibernation in the VM (an undocumented Hyper-V setting) is left to M7's real machine.
  *HVCI (2026-11-10):* Memory Integrity does not run in the test VM.
  `tools/vm/memory-integrity.ps1 on` sets Microsoft's registry keys (recommended settings,
  without UEFI lock; the VM, a Hyper-V generation 2 guest, needs no nested virtualization for
  it); after the restart, `Win32_DeviceGuard` reports VBS "enabled but not running": it requires
  Secure Boot (required properties 1 and 2, available 1, 3, 5 and 7), Microsoft documents no
  setting without it (Secure Boot, or Secure Boot with DMA protection), and test signing needs
  Secure Boot off. As the design foresaw, the run with Memory Integrity on moves to M7 (a signed
  driver, Secure Boot on, on a real machine; a Hyper-V VM with Secure Boot on would do too).
  Microsoft's "Driver compatibility with memory integrity and VBS" lists three checks: Driver
  Verifier's code integrity checking, a run with Memory Integrity on, and HLK's readiness test
  (not in M6). The first runs in the `standard` and `stack` profiles since #61 (it found the
  host mapping without `MdlMappingNoExecute`). New, a check of the binaries in the `driver` CI
  job (`.github/scripts/check-hvci.ps1`, reading the PE headers itself): no writable and
  executable section, sections aligned on pages, NX compatible ("How to build compatible
  drivers"); the debug and release drivers pass, as does Windows' `portcls.sys`, and a copy
  with NX cleared and `.text` made writable fails. `hvciscan` (a separate download) is left
  out: Microsoft's page does not list it, and the CI check reads those properties from the file.
  The independent review's other findings were fixed (an optional header too short to read, a
  test-signing check that depended on the display language).
  *Endurance (2026-11-10):* 4 hours of `measure load` (16 spinning threads) on the debug
  package under Driver Verifier (`standard`, flags 0x021b093b), in the VM: `campaign.ps1 -Only
  install,load,uninstall -Seconds 14400` (docs/test-vm.md, section 8) passes. **0 crackle**,
  0 silence, 98.8 % audible after the warm-up (as in the 30 s runs); the engine read 1439989
  packets with 0 discontinuity, 0 silent and 0 timeout, 100.0 % of the frames due; the player
  made 9942076 advances, 0 late, 0 stale, one attachment; the driver advanced the same
  691205689 frames, skipped 0 packet and refused 0 request. The gaps' tail the reference loop's
  note asked for: 1415749 below 1.5 ms, 8526214 in 1.5-2 ms, then 37 in 2-3 ms, 6 in 3-4 ms,
  2 in 4-5 ms, 2 in 5-7 ms and none above, the longest 5.9 ms (the 10 minutes of M5: 14 above
  2 ms, the longest 4.5 ms): 47 gaps above 2 ms in 4 hours, all far below the engine's 10 ms
  period, none heard. Driver Verifier counted 16 pool allocations in 4 hours, all from the
  special pool, none failed: nothing allocates per period. No bug check (the VM's uptime ran
  through), and `uninstall` unloaded the driver. The release package was not run for 4 hours:
  it differs by its optimisations and passes the 30 s `load` under Driver Verifier (#60, #61).
  *Security review (2026-11-10):* four independent reviews, one per trust boundary (Kernel
  Streaming, the host interface, the shared memory, PnP and power); no critical or high
  finding. Each defect gets its own pull request, in this order: the host's process cleared
  before its mapping was undone (#82); a fuzzer of the host interface of our own; IOCTLs still
  served after the host interface's surprise removal; the IOCTL input read before the control
  code is checked; the stream buffer's allocation check-then-act and its partial zeroing; a late
  stream's close stopping the next stream; the host interface's power policy (no
  `PoSetPowerState`, no device power request on a system one); the adapter starting without its
  host interface; the host interface's instance ID (phantom devices). Accepted and documented:
  another interactive session's process may attach when no host is (the device is the
  machine's, like a physical microphone; an administrator restricts it through the device
  class's `Security` value; #81 will say who holds it).
  *IOCTL fuzzer (2026-11-10):* `tools/ioctl-fuzz` (docs/test-vm.md, section 6; the campaign's
  `fuzz` scenario) sends every vendor control code with every method and access, the six codes
  with every size around theirs and invalid buffer addresses, `ATTACH` with handles that are not
  a usable event, other requests (read, write, flush, lock, information, overlapped requests each
  followed by `CancelIoEx`), and runs threads and processes that attach, advance with valid,
  stale and random positions (all refused: no capture stream runs then; the `tone`, `load` and
  `kills` scenarios cover advancing), detach, hand their handles to short-lived processes and
  die while attached, some killed; every process must end and the interface be free afterwards.
  It checks that its requests reach the driver (the driver's refusal counter grows by the vendor
  codes sent; relative and trailing-name opens fail with the driver's statuses). In the VM under
  Driver Verifier, 60 s runs (about 175000 operations in four threads, about 200 worker
  processes) find no crash and nothing unexpected but one: a
  process at low integrity (checked: level 0x1000) opens the host interface for writing, and so
  could attach as the host. The device object has no integrity label (`IoCreateDeviceSecure`
  takes a DACL only), and despite Microsoft's "objects that lack an integrity label are treated
  as medium", the open succeeds; the fix is #83.
  *Low integrity (2026-11-10):* the security review's fuzzer found that a process at low
  integrity (its token checked at level 0x1000) could open the host interface for writing, and
  so attach as the host and feed the microphone. The device object has no integrity label
  (`IoCreateDeviceSecure` takes a DACL only), and despite Microsoft's "objects that lack an
  integrity label are treated as medium", the I/O manager let the open through. The host
  interface's `IRP_MJ_CREATE` now refuses (`STATUS_ACCESS_DENIED`) an open asking for any write
  right (`FILE_WRITE_DATA`, `FILE_APPEND_DATA`, `FILE_WRITE_EA`, `FILE_WRITE_ATTRIBUTES`,
  `DELETE`, `WRITE_DAC`, `WRITE_OWNER`, and the generic rights that include writing) unless the
  process token, and the impersonation token if there is one, are at least of medium integrity;
  reading stays open to low integrity (`GET_INFO`, `GET_STREAM`, `GET_STATS`), as the security
  descriptor allows. The rule is `hostmic_driver_core::host::open_allowed`, unit-tested. Its
  first version read `SeQueryInformationToken(TokenIntegrityLevel)`'s result as a pointer to a
  label, while the routine writes the level itself in place of the pointer ("a DWORD value
  (not a pointer to it)"): the first open bug-checked the VM (0x3B, reading address 0x3000, the
  terminal's high integrity level; read from the minidump with the WDK's `kd`). The independent
  review found it; fixed, the campaign passes 12 of 12 under Driver Verifier (`standard`) with
  `tools/ioctl-fuzz`'s `fuzz` scenario, whose low-integrity probe now reads but cannot write.
  *Surprise removal (2026-11-10):* after the host interface's `IRP_MN_SURPRISE_REMOVAL`, its
  handles still open got `ATTACH` and `ADVANCE` served, while Microsoft's "Handling an
  IRP_MN_SURPRISE_REMOVAL Request" says a driver must fail every later request it would have
  handled, but close, cleanup and PnP. The child now refuses `ATTACH` and `ADVANCE` with
  `STATUS_DEVICE_REMOVED` from its surprise removal to its next start (a plain stop for
  rebalancing refuses nothing). A deliberate deviation: `DETACH` and the reads (`GET_INFO`,
  `GET_STREAM`, `GET_STATS`) stay served, so that a host can let go and diagnose; they touch
  nothing a removal frees (the link lives until the last handle closes). The host crate turns
  `ERROR_DEVICE_REMOVED` on `ADVANCE` into `Error::NotHost` (attachment kept, for `DETACH`), so a
  following `Player` releases and waits for the next arrival instead of ending: the PnP manager
  surprise-removes the child before it tells applications, so the refusal comes first. The
  independent review found that last point. Not triggered in the VM (it needs a surprise
  removal while a host holds its handle, which only Device Fundamentals' PnP tests produce, and
  they run without a host); the campaign passes 12 of 12 under Driver Verifier.
  *IOCTL input (2026-11-10):* the host interface built its input slice from the request's
  `SystemBuffer` before checking the control code, under a comment that assumed
  `METHOD_BUFFERED`; for another method that field is not the request's buffer ("The I/O manager
  doesn't provide any system buffers or MDLs" for `METHOD_NEITHER`). Nothing read it for those
  codes, and the I/O manager leaves it null for them, so no request from user mode could misuse
  it; an IRP built by other kernel code could. Any code but the six (all `METHOD_BUFFERED`) now
  goes to the unknown-code refusal before `SystemBuffer` is read; without a system buffer the
  output length counts as 0, so the output-size checks refuse a request before it changes
  anything, and a reply is written only to a system buffer that exists. The independent review
  suggested the output length: an `ATTACH` would otherwise map the buffer, then fail to say where.
  *Stream buffer allocation (2026-11-10):* the stream checked that it had no buffer, then built
  and published one, in separate steps; PortCls documents no serialization of
  `AllocateAudioBuffer` and `AllocateBufferWithNotification` on a pin, and any process that opens
  the capture pin can send `KSPROPERTY_RTAUDIO_BUFFER`. Two requests at once could both build a
  partial MDL and a buffer reference, the second's replacing the first's, which leaked. An
  `allocating` flag, taken by compare-and-exchange for the whole allocation, now refuses a second
  one meanwhile (`STATUS_UNSUCCESSFUL`); `FreeAudioBuffer` takes the same flag, waiting for an
  allocation in progress, so that it cannot free a buffer published between its check and its
  release (the independent review found that window in the first version); and a free with an
  MDL that is not the stream's releases nothing (it released the stream's own buffer, which the
  audio engine might still map).
  *Stream buffer zeroing (2026-11-10):* a stream's allocation zeroed only the bytes asked for,
  while its client maps whole pages: a buffer of 3840 bytes (shared mode) showed the 256 bytes
  after it too, which an earlier, larger stream (exclusive mode) may have filled with the host's
  audio, and a request for 4 bytes showed the rest of its page. The shared buffer is zeroed for
  each host, not for each stream. Low: a process that can open the capture pin can record the
  microphone anyway, but the assurance case says buffers are zeroed when allocated. The stream
  now zeroes the whole pages it hands out (`BufferRef::zero_pages`).
  *A late stream release (2026-11-10):* the host link's stream state is one for the adapter,
  and a stream's release called `stream_closed` and cleared the buffer geometry. One capture pin
  at a time does not order a stream object's last `Release` before the next stream's creation
  (PortCls documents no such order): a late release would stop the new stream's cursor and clear
  its geometry, so the host's `ADVANCE`s were refused and the new stream got no packets until it
  was reopened (availability only). `stream_opened` now hands the stream a token (a counter,
  never 0), and `stream_closed`, `set_buffer` and `set_state` carry it and act, under a
  `stream_lock` of their own (`PASSIVE_LEVEL`, never in the real-time path), only for the
  current stream; a new stream also starts without the previous one's buffer geometry (the
  independent review: the old stream's late `FreeAudioBuffer` is now refused). Left as it is:
  the audio engine's notification event has the same one-slot shape, keyed by the event, and
  relies on PortCls unregistering it before the stream's last release, which its documentation
  implies (a new stream's registration would otherwise fail, an availability issue only).
  *The host interface's power policy (2026-11-10):* with no function driver, the host
  interface's PDO is its own power policy owner, and it acted as none: it never declared a
  device power state (`PoSetPowerState`), and completed a system power request without asking
  for the matching device one. Nothing failed for it (no hardware), but the power manager's
  records were wrong, and Microsoft's rules for a raw device's bus driver were not followed.
  The PDO now declares D0 at start and D3 at a (surprise) removal, declares each device power
  request it receives, and turns a system power request (set or query) into a device power
  request of the same kind for itself (`PoRequestPowerIrp`: D0 in the working state, D3 in any
  other, as its capabilities report), sent even when the device is already in that state,
  marked pending first and completed from that request's completion routine (a system
  set-power request with success, as it cannot fail; a query with its device query's status).
  A guarded mutex serializes each declaration with the check that allows it, and a removal
  marks the device removed before declaring D3, so that a device power request handled during
  a removal cannot declare D0 after it (Microsoft's pages use a remove lock for the same end).
  The dispatch runs at `PASSIVE_LEVEL` (`DO_POWER_PAGABLE`), the completion routine at
  `PASSIVE_LEVEL` or `DISPATCH_LEVEL`; never in the real-time path. The independent review
  found the departures from Microsoft's pages in the first version (no device request when
  already in the state, a system query answered without a device query, no serialization with
  a removal, `PoRequestPowerIrp`'s result tested with `NT_SUCCESS` rather than against
  `STATUS_PENDING`). The campaign checks the declared state (`DEVPKEY_Device_PowerData`): D0
  after the install, D3 while disabled, D0 after enabling it again; but the previous driver
  passes those checks too (measured in the VM): the PnP manager records D0 at a start and D3 at
  a removal on its own, so they guard against a wrong declaration and show nothing of this
  change. What this change adds is the system power path, which runs at each shutdown and
  restart of the VM (S5), the only system power change a Hyper-V guest has: a restart under
  Driver Verifier with the driver installed completes without a bug check. Sleep and
  hibernation wait for M7's real machine.
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
  *Microsoft's pages, read 2026-11-09 ("Driver signing options" and "Attestation sign Windows
  drivers", both updated 2026-04-14; "Register for the Microsoft Windows Hardware Developer
  Program", updated 2026-08-13):*
  - attestation is still titled "for testing scenarios" ("For testing purposes only, you can
    submit your drivers for attestation signing"), but none of the restrictions listed blocks
    hostmic: no Windows Update for retail audiences (hostmic is a root-enumerated device
    installed from GitHub releases); Windows 10 Desktop and later only (Windows Server 2016 and
    later loads only HLK-tested drivers); not Windows Certified, yet "Windows trusts the
    driver", so it loads with Secure Boot on;
  - registration requires an organization: an EV certificate (to register only, not to sign the
    driver), a Microsoft Entra ID global administrator, company details (D-U-N-S number or
    manual entry), a legal contact with authority to sign agreements, then a questionnaire about
    the business. An EV certificate is issued to a legal entity, so this path needs one (a sole
    proprietorship, if the certificate authority accepts it for EV);
  - **still to read**: the Code Signing Agreement accepted at registration, which decides
    whether attestation-signed drivers may be distributed to users. If not, WHCP (HLK testing)
    is the only path.
  *The release workflow's rules (2026-11-09, from a survey of open-source driver CI — windows-
  drivers-rs, Windows-driver-samples, ovpn-dco-win, WinFsp, virtio-win — and of GitHub's,
  zizmor's, SLSA's and Scorecard's guidance):*
  - **No cache at all**: `cache-mode: none` at workflow level, no `actions/cache`, no setup
    action with caching; the CI's caches (written by `main`, restored by pull requests) never
    reach a release. Everything is rebuilt from pinned sources: the WDK from NuGet with its
    SHA-512 checked, cargo-wdk `--locked`, the driver `--locked`. zizmor (1.31 or later)
    checks the cache rule; SLSA Build L3 requires that a build's output not depend on a cache
    another build could write.
  - **Two jobs**: a build job with a read-only token and no secret; a publishing job that
    downloads its artifact, attests and publishes, and alone holds `contents: write`,
    `id-token: write` and `attestations: write`. Never an artifact or a cache from a pull
    request.
  - **A `release` environment**: deployment restricted to `v*` tags, a required reviewer (the
    maintainer: a deliberate confirmation step, even alone), no administrator bypass; its
    secrets are released only after approval.
  - **What is built**: the release profile, packaged without test signing (`--sign-mode off`,
    since Microsoft regenerates the catalog), with the `.pdb` that attestation submissions
    require; the CI already builds and checks it on every pull request.
  - **What is published**: the package, an SBOM and the provenance (`actions/attest`) as
    release assets, the provenance also as an `*.intoto.jsonl` asset (Scorecard's
    Signed-Releases check reads assets, not GitHub's attestation store); release notes; the
    checksums.
  - The badge requirements on releases (below, "OpenSSF Best Practices badge") apply as well.
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
  coverage, dynamic analysis of the unsafe code (`dynamic_analysis_unsafe`: Driver Verifier,
  M6), reproducible builds, signed tags and hardening (M6).
- **Update 2026-11-09:** with fuzzing in CI (M4), passing's `dynamic_analysis` is Met.
  `dynamic_analysis_unsafe` stays Unmet, with an updated justification: fuzzing covers the
  safe validation code, not the kernel's unsafe code. The percentages do not change: both
  criteria are suggested ones.
- **M4's close (2026-11-09):** no further criterion is met. What remains needs a release
  (release notes, signed tags, reproducible builds: M7), a second maintainer (bus factor,
  access continuity, non-author approval), a coverage measurement (80% statements), or M6
  (Driver Verifier for `dynamic_analysis_unsafe`, hardening).

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

- Endpoint naming: the device name is fixed in the INF; the endpoint name per line. Thin Audio
  Gateway names its endpoints at the customer's choice; a name chosen by the host would suit
  hostmic's users.
- Formats beyond 48 kHz, 16-bit, stereo: other rates (44.1 to 192 kHz), 24-bit and 32-bit
  float, more channels, as professional virtual cables offer.
- The `PROPOSEDATAFORMAT` probe: Virtual Audio Cable's log shows the endpoint builder
  (`svchost`) sending the same bare 64-byte `KSDATAFORMAT` and Virtual Audio Cable refusing it
  as too small (0x52 bytes required), while refusing it cost hostmic its endpoint in M3. The
  status returned may be what matters; to investigate, not needed for M4.
- Windows 10 support: keep it if it costs nothing (WaveRT is fine there; ACX would not be).
- PortCls and Kernel Streaming bindings upstream: `wdk-sys` is organized by API family behind
  Cargo features (`gpio`, `hid`, `parallel-ports`, `spb`, `storage`, `usb` in 0.5.2) and has
  none for audio; no windows-drivers-rs issue mentions PortCls or audio (searched
  2026-11-09). `hostmic-sys`'s generated bindings (MIT OR Apache-2.0, from Microsoft's headers,
  depending on nothing else in hostmic) could become an `audio` feature there, where Rust
  driver authors would find them and where they would follow the WDK; its C helper
  (`map_user.c`) and the driver's COM plumbing are hostmic's own and would stay. Not a
  repository of hostmic's: it would double the CI and releases for one maintainer while the
  bindings' surface may still change (M6). After M7, with the bindings proven by a signed
  driver: open a discussion on windows-drivers-rs (the maintainer posts it).
- ARM64: later. The same driver rebuilt for `aarch64`, with the same signing requirement, and
  testing needs ARM64 hardware (Hyper-V on x64 does not run Windows on ARM): after signing,
  not before.
- Signing programme: whether OSSign reopens applications, accepts the project and how long it
  takes; a user base to show by then (a usable public release would help).
- A cross-platform host crate (after M4, with M5): one API for "a virtual microphone fed by
  this application", backed by the hostmic driver on Windows and by a PipeWire source node on
  Linux (an application can already appear there as a microphone, `media.class=Audio/Source`,
  with no driver and no signing); macOS later, where virtual devices are Audio Server plug-ins
  in user space (BlackHole) and distribution needs an Apple Developer ID and notarization.
  Ported drivers for Linux or macOS would bring little: free virtual devices already exist
  there, and only Windows lacks an open-source, host-fed microphone. A portable crate, though,
  would let an application such as Résonnance keep one code path on every platform, and could
  bring Rust audio developers to the project before the Windows driver is signed.

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
