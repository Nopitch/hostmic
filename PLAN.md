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
- **M6, hardening:** security review against the rules above, Driver Verifier, HVCI, sleep and
  resume, host crash, several hours of endurance; M4's carried-over items (above).
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
