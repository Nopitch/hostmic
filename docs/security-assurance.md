# Security: what to expect, and why to trust it

hostmic is a Windows kernel-mode driver. This document states what users can and cannot expect
from it in terms of security (its security requirements), then argues why the design and the
development process meet them (an assurance case). It covers the state during milestone M6
(hardening) and says what later milestones change; [PLAN.md](../PLAN.md) has the roadmap,
[SECURITY.md](../SECURITY.md) the vulnerability process and the security rules.

## 1. What you can and cannot expect

### You can expect

- **No path from user mode to kernel memory.** No input reachable from user mode (Kernel
  Streaming property requests and data formats, the host interface's IOCTLs) lets a process
  read or write kernel memory, or memory of another process.
- **Only the attached host gets the buffer.** From M4, the audio buffer, which the driver owns,
  is mapped into one process only besides the audio engine: the host that attached through the
  host interface. The mapping is undone when that host detaches, when its handle is closed by
  whichever process holds it last, and when the host's process ends, even while another process
  holds a duplicate of its handle. Opening the host interface is restricted to the system,
  administrators and interactive users (of any session), enforced by the I/O manager on a
  device object of its own; opening it for writing, which attaching needs, to processes of at
  least medium integrity, enforced by the driver.
- **Malformed input is rejected, not trusted.** Unsupported formats, pins, property sizes and
  values are refused with an error status; the driver accepts one exactly specified format.
- **A stable system.** The driver does no allocation, takes no lock and does no I/O in its
  real-time path; arithmetic on positions and sizes cannot overflow or panic (a panic in the
  kernel is a crash).
- **No data leaves the machine, nothing is stored.** The driver has no network code, writes no
  files, keeps no audio beyond the ring buffer, and uses no cryptography.
- **Compatibility with Windows' memory protections** (Memory Integrity / HVCI): no executable
  pool, no writable and executable mappings.

### You cannot expect

- **Production use.** hostmic is experimental and has no release. Test builds are test-signed:
  loading them requires test signing, which lowers the whole system's protection. Use a virtual
  machine only.
- **Audio confidentiality on the machine.** "Microphone (hostmic)" is a regular Windows
  microphone: any application that the Windows privacy settings allow to use the microphone can
  record what the host plays into it, as with a physical microphone. Like every Windows audio
  device, its Kernel Streaming filters can be opened by local users and by the audio service.
- **Authentication of the audio.** The driver does not check what the host writes: whatever the
  attached host plays is what applications record. Choose the host you run.
- **Availability guarantees.** A host can stop feeding audio (the microphone then reports
  unplugged); one host at a time can attach, first come: when none is attached, a process of
  at least medium integrity of any interactive user, in any session (fast user switching, a
  second Remote Desktop session), can be that host. Any process allowed to open the host
  interface, for reading only included, keeps it from being disabled, uninstalled or updated
  without a reboot while its handle is open. The device belongs to the machine, like a physical microphone; an
  administrator who wants fewer users to reach it restricts the host interface's device class
  through its `Security` registry property (section 2.6).
- **Protection against administrators or kernel code.** Anyone who can load drivers or enable
  test signing already controls the machine; such attacks are out of scope (SECURITY.md).

## 2. Assurance case

### 2.1 Threat model

| Actor | Trust | What they could try |
|---|---|---|
| Unprivileged local process or user | untrusted | Malformed Kernel Streaming requests or formats; from M4, opening the host interface, malformed IOCTLs, racing the host, mapping or keeping the buffer |
| Host application (from M4) | trusted with its own audio only | Malformed IOCTLs, invalid positions, dying mid-stream to leave dangling state |
| Windows audio engine and PortCls | trusted (part of the OS) | Unusual but valid call sequences: state changes, buffer reallocation, concurrent position queries |
| Recording applications | untrusted | Only reach the driver through the audio engine |
| Contributors, dependencies, CI | partially trusted | Malicious or vulnerable code, leaked credentials, tampered build |
| Administrators, kernel code | out of scope | Already in control of the machine |

**Assets:** kernel memory integrity and confidentiality, system stability, other processes'
memory, the integrity of the audio stream for the attached host, the integrity of the source and
of the build.

### 2.2 Trust boundaries and attack surface

1. **User mode → kernel, through Kernel Streaming**: property handlers
   (`PROPOSEDATAFORMAT`, jack descriptions), data-range intersection, stream creation with a
   client-supplied data format. Each handler checks instance and value sizes before reading or
   writing, from the sizes Kernel Streaming has checked (section 2.6); a new stream's data
   format is parsed by `hostmic_driver_core::format`, pure Rust with no kernel call,
   unit-tested on every truncation of a valid format.
2. **User mode → kernel, through the host interface** (M4): a raw child device of the adapter
   with its own security descriptor (`IoCreateDeviceSecure`, `FILE_DEVICE_SECURE_OPEN`); opens
   relative to another handle refused; opens asking for any write right refused unless the
   caller's process token, and its impersonation token if any, are at least of medium integrity
   (the descriptor can carry no integrity label, and a low-integrity process could open it for
   writing: M6); its requests, IRPs and fast I/O alike, never reach
   PortCls. IOCTL input validation in pure Rust, unit-tested and fuzzed (PLAN.md, M4); user
   buffers copied once (`METHOD_BUFFERED`), then validated. `ATTACH` references the host's
   event only for a user-mode request in the requesting process, with `EVENT_MODIFY_STATE`;
   one host at a time; only the attached handle, in the host's own process, detaches or
   advances the position (a duplicated handle in another process cannot);
   `ADVANCE` is checked against the stream's packed generation and position and committed with
   one compare-and-exchange, so a request computed for an earlier state cannot move the
   position; the interface refuses removal while a handle to it is open, so no request reaches
   a deleted device, and after its surprise removal it refuses `ATTACH` and `ADVANCE` on the
   handles still open (`DETACH` and the reads stay, so that a host can let go). A control code
   other than the interface's six is refused before the request's buffer is read. Refused
   requests are counted (`GET_STATS`) and logged, at a bounded rate, since any process allowed
   to open the interface can send them in a loop; the counters therefore say how many requests
   were refused, not who sent them. A process at low integrity can still open the interface
   for reading. `GET_STREAM` and `GET_STATS` tell any opener whether, and how long, the
   microphone is in use, as the Windows audio settings already do, and `GET_STATS` also the
   function, reason and status of the last refused request, whichever process sent it (another
   process's refused `ATTACH`, for instance); `GET_INFO` is static. The integrity check is made
   at the open: a medium-integrity process that hands its write handle to a low-integrity one
   lets that one attach.
3. **Shared memory** (M4): one buffer of whole pages, owned by the driver, from zeroed pages
   (`MmAllocatePagesForMdlEx`) and zeroed again for each host, so that neither kernel data nor
   a previous host's audio reaches a host. It is mapped into the attached host (readable and
   writable, never executable, through a C function of a few lines that catches the mapping's
   structured exception, which Rust cannot) and, through a partial MDL per stream, into the
   audio engine; the whole pages of a stream's buffer are zeroed when it is allocated (the
   audio engine maps whole pages, beyond the bytes it asked for). The driver produces no audio
   and never reads it. The pages are
   reference-counted by the adapter,
   each stream buffer and the host's mapping, so none is freed while mapped. A user mapping is
   undone in its own process: on `DETACH`, on the handle's last close from any process
   (attaching to the host's), and from a process notification routine when the host's process
   exits; attaching and detaching are serialized, so the notification returns only once the
   mapping is gone.
4. **PortCls → miniport**: COM-style calls, possibly concurrent; reference counting and the
   real-time design below. A stream's buffer allocation and release serialize themselves (a
   second allocation at the same time is refused), and the host link acts on a stream's
   requests only while it is the current stream (a token per stream), so that a late release
   of a stream cannot stop the next one. What the driver assumes of PortCls and `ks.sys`
   without checking it is listed in section 2.6.
5. **Supply chain**: repository, CI, dependencies, and later the release signing.

### 2.3 Secure design principles applied

- **Least privilege**: the host interface open to administrators and interactive users only,
  for writing to processes of at least medium integrity;
  the buffer mapped into the attached host only; CI tokens read-only by default with no secrets.
- **Economy of mechanism**: one format, one host, one stream; no lock in the real-time path,
  a single writer of the position (the attached host's `ADVANCE`, one compare-and-exchange)
  and atomics; small, static descriptor tables.
- **Fail-safe defaults**: unknown formats, pins and requests are rejected; without a host, the
  microphone reports unplugged instead of streaming silence.
- **Complete mediation**: every request is checked when it arrives, nothing is cached from an
  earlier check.
- **Open design**: all code, design and tests are public (PLAN.md, this document).
- **Separation of privilege**: changes to main need a pull request with required checks (format,
  lint, tests, build, fuzzing, secret scanning, workflow audit, dependency review, DCO,
  CodeQL).
- **Defense in depth**: a memory-safe language, with every `unsafe` block justified by a
  `// SAFETY:` comment (enforced by clippy), plus static analysis, fuzzing, VM tests and
  Driver Verifier.

### 2.4 Common weaknesses and how they are countered

| Weakness (CWE) | Countermeasure |
|---|---|
| Improper input validation (CWE-20) | Allowlist checks of formats and sizes in pure, unit-tested Rust (`format::is_supported`); IOCTL validation the same way; both fuzzed |
| Out-of-bounds read or write (CWE-125, CWE-787) | Size checks before every access to a caller's buffer, from the sizes Kernel Streaming has checked (section 2.6); ring writes through bounded slices; unaligned-safe reads and writes |
| Integer overflow (CWE-190) | 128-bit intermediates for clock arithmetic, saturating and checked operations; overflow checks in every build, release included (an overflow stops the system with a bug check naming its place instead of wrapping) |
| Use after free (CWE-416) | Atomic reference counting with `Arc`-like orderings, including the shared buffer's pages (the adapter, each stream buffer and the host's mapping hold one); rundown protection (`EX_RUNDOWN_REF`) on the objects a request may use while another thread lets them go (the host's event, the audio engine's notification event, the topology port's jack events): letting one go waits until no request holds it |
| Race conditions (CWE-362) | One writer of each stream's position (the attached host's `ADVANCE`, committed with one compare-and-exchange on the packed generation and position, so a request computed for an earlier state is refused), positions published with release/acquire atomics, attaching and detaching serialized by a lock, state changes serialized by PortCls; a stream's buffer allocation and release serialized by a flag taken by compare-and-exchange; the host link's per-stream calls keyed by the stream's token, under a lock of their own, never in the real-time path |
| Double fetch, TOCTOU on user memory (CWE-367) | User input copied once, then validated (M4) |
| Kernel memory disclosure (CWE-200) | Buffers zeroed when allocated; the shared buffer made of whole zeroed pages, zeroed again for each host, and the whole pages of each stream's buffer zeroed when it is allocated; no uninitialized structure returned |
| Denial of service by a crash (CWE-248, CWE-400) | No panic on any input in the core logic; allocation only when a stream is created, never per period |
| Hard-coded or leaked credentials (CWE-798) | No secrets in CI; secret scanning in CI, in pre-commit and with GitHub push protection |
| Vulnerable or malicious dependencies (CWE-1395) | Pinned dependencies, required dependency review, Dependabot, actions pinned by commit SHA |

### 2.5 Evidence

- Unit tests of the pure logic (`hostmic-driver-core`), layout checks of the generated bindings
  against MSVC (`hostmic-sys`), run in CI on every pull request.
- Fuzzing with cargo-fuzz and AddressSanitizer of every validation of untrusted input (Kernel
  Streaming data formats, the host interface's requests, stream buffer sizes) and of the
  lock-free protocol between stream state changes and the host's position updates: 60 s per
  target on every pull request (the required `fuzz` check), 10 minutes every week.
- Clippy (pedantic, `-D warnings`), with `undocumented_unsafe_blocks` and
  `multiple_unsafe_ops_per_block` forbidden in the driver; CodeQL; zizmor.
- VM tests documented in [docs/test-vm.md](test-vm.md), including a measured recording
  (`tools/capture-check`: frequency, level, discontinuities), run as a campaign of twelve
  scenarios (`tools/vm/campaign.ps1`) on the debug and the release packages, among them the
  device idling to D3 through PortCls's inactivity timer and streaming again from it.
- Driver Verifier (M6, [docs/test-vm.md](test-vm.md), section 9): the campaign passes with the
  driver, `portcls.sys` and `ks.sys` under the standard rule classes, DDI compliance,
  PortCls's port/miniport rules and code integrity checks, and survives randomized allocation
  failures without a bug check. Its first run found the host mapping requested without
  `MdlMappingNoExecute`, a code integrity violation that a too-broad exception handler had
  hidden (PLAN.md, M6). Under it, 4 hours of recording with 16 spinning threads (the debug
  package) count 0 crackle, 0 packet skipped and 16 pool allocations in all, none per period.
- A check of the debug and release drivers in the `driver` CI job against Memory Integrity's
  header requirements (`.github/scripts/check-hvci.ps1`): no section both writable and
  executable, 4 KiB section alignment, NX compatible.
- Microsoft's Device Fundamentals tests under Driver Verifier (M6,
  [docs/test-vm.md](test-vm.md), section 10): on the adapter, PnP transitions with Microsoft's
  test filter (surprise removal among them), an IOCTL fuzzer on the Kernel Streaming filters in
  the kernel, an API fuzzer and a reinstall with I/O pass. They leave the host interface out (a
  raw device); `tools/ioctl-fuzz` covers it from user mode instead ([docs/test-vm.md](test-vm.md),
  section 6, and the campaign's `fuzz` scenario): every control code, method and access, sizes
  and invalid buffer addresses, bad event handles, other requests, opens by kind and
  integrity level, and threads and processes that attach, advance and die while others are
  killed. It found that a process at low integrity could open the host interface for
  writing, now refused (section 2.2, PLAN.md, M6).
- The host crate `hostmic` (M5) under the same `unsafe` lints; its writes into the shared
  buffer are volatile, never through a Rust reference, and its pure parts (the clock, the
  ring's wrap, sample conversion) are unit-tested in CI.
- Review of every change through pull requests, an independent review of the PortCls adapter
  (M3), and of every kernel and host-crate pull request since M4; in M6, four independent
  security reviews, one per trust boundary, whose ten findings were each fixed in a pull
  request of their own, reviewed again and tested in the VM (PLAN.md, M6).
- The host interface's system power path (M6): a restart of the VM under Driver Verifier,
  with the driver installed, completes without a bug check (a Hyper-V guest's only system
  power change).

### 2.6 Assumptions and accepted risks

What the driver relies on without checking it, and what was left as it is, on purpose, after
the M6 security review (PLAN.md, M6):

- **`ks.sys` checks the sizes of what a client passes.** In a format intersection the driver
  reads a client's whole `KSDATARANGE`, then a `KSDATARANGE_AUDIO` when the range's
  `FormatSize` allows it; for a new stream it reads the data format up to the `FormatSize` it
  declares. It relies on Kernel Streaming having checked that the buffers hold that many
  bytes, as Microsoft's audio samples (sysvad) do; sections 2.2 and 2.4's size checks start
  from those sizes. A new stream's format is then validated in pure Rust
  (`format::is_supported`), an intersection's ranges by checks in `wave.rs`.
- **PortCls's ordering of the streams' calls.** The audio engine's notification event has one
  slot for the adapter: the driver relies on the previous stream's event being unregistered,
  or that stream released, before the next stream registers its own, as in Microsoft's samples
  and implied by PortCls's documentation; otherwise the new registration fails (availability,
  not memory: a stream's release clears its own event and buffer). It also relies on PortCls
  serializing a stream's state changes, and the registrations and unregistrations of the
  event (the event is held under rundown protection, section 2.4, which two overlapping
  releases would misuse).
- **Any interactive session can attach.** First come, one host at a time (section 1). The
  host interface's device class, `{FEFD76E2-6428-43C6-A8E1-776C497C8CB4}`, takes an
  administrator's security descriptor from the class's `Security` registry property, as
  Microsoft documents for any device created with `IoCreateDeviceSecure`; no INF installs the
  class, so the administrator may have to create its key, and the descriptor applies when the
  host interface is created, at the adapter's next start. A device's own `Security` property
  (`SPDRP_SECURITY`) is the other way, and since the host interface keeps its devnode across
  reinstalls, it lasts. Neither is tested yet.
  [Issue #81](https://github.com/Nopitch/hostmic/issues/81) is to say which process holds the
  interface when an `ATTACH` is refused.
- **Openers see the stream's state and the last refusal, and can hold the device.** Any
  process allowed to open the host interface, a low-integrity one included (for reading), can
  read whether the microphone is in use and what the last refused request was, raise the
  refusal counters (section 2.2), and, while its handle is open, keep the host interface (and
  with it the adapter) from being removed without a reboot.
- **Not every log is rate-limited.** Refused host interface requests are (and refused
  `ADVANCE`s that only raced a state change are not logged). Logged each time: Kernel
  Streaming requests (format intersection, proposed formats, stream creation, buffer
  allocation and release, state changes), PortCls's queries for interfaces the driver does not
  implement, and a host's attach and detach, so a process that can open the microphone's
  filters, or one that can attach and close in a loop, can make the driver print without
  bound. `wdk::println!` formats on the stack and `DbgPrint`'s output is dropped by the default
  debug print filter: CPU time only.
  [Issue #77](https://github.com/Nopitch/hostmic/issues/77) replaces it with ETW events that
  are never per audio period.

### 2.7 Known gaps and plans

- A run with Memory Integrity on (M7: virtualization-based security
  requires Secure Boot, which test signing needs off, so it does not run in the test VM;
  Driver Verifier's code integrity checks and the CI check of the binaries stand in for it,
  section 2.5). Sleep and hibernation on a real machine (M7: a Hyper-V guest has no sleep
  state; the device's own idle transitions to D3 and back are tested in the VM, section 2.5).

- Static analysis of the kernel code is shallow: CodeQL's Rust queries have no driver rules,
  and Static Driver Verifier does not take Rust. Clippy's lints, the independent reviews,
  fuzzing and Driver Verifier carry the kernel code's evidence, not CodeQL.
- **Not checked yet, and important: a host that clones itself.** A host, which any
  medium-integrity interactive process can become, could clone its own address space after
  attaching (`RtlCloneUserProcess`, `NtCreateProcessEx` with itself as parent,
  `PssCaptureSnapshot` with `PSS_CAPTURE_VA_CLONE`). The driver undoes the mapping in the
  host's process only. Whether Windows copies a locked-pages mapping into the clone is not
  known here; if it does, the clone would keep reading the shared buffer after the host
  detaches (the next host's audio), and after the adapter's removal frees the pages, it would
  map freed physical memory, which breaks section 1's first two promises. To be tested in the
  VM, and fixed if so.
- Not checked: if an `IRP_MN_QUERY_ID` reply cannot be allocated, whether the PnP manager
  drops the host interface's devnode without removing it. The PDO would then never be
  deleted (the adapter's removal deletes only a child that was removed or never reported): it
  would keep the driver from unloading and hold one of the four host interface slots.
- A single maintainer: no second human reviewer yet (GOVERNANCE.md).
- Release signing and provenance attestations (M7).
