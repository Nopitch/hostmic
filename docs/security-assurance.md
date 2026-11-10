# Security: what to expect, and why to trust it

hostmic is a Windows kernel-mode driver. This document states what users can and cannot expect
from it in terms of security (its security requirements), then argues why the design and the
development process meet them (an assurance case). It covers the state after milestone M5 and
says what later milestones change; [PLAN.md](../PLAN.md) has the roadmap,
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
  administrators and interactive users, enforced by the I/O manager on a device object of its
  own; opening it for writing, which attaching needs, to processes of at least medium integrity,
  enforced by the driver.
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
  unplugged); one host at a time can attach.
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
   writing; data formats are parsed by `hostmic_driver_core::format`, pure Rust with no kernel
   call, unit-tested on every truncation of a valid format.
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
   a deleted device. Refused requests are counted (`GET_STATS`) and logged, at a bounded rate,
   since any process allowed to open the interface can send them in a loop. `GET_STREAM` and
   `GET_STATS` tell such a process whether, and how long, the microphone is in use, as the
   Windows audio settings already do.
3. **Shared memory** (M4): one buffer of whole pages, owned by the driver, from zeroed pages
   (`MmAllocatePagesForMdlEx`) and zeroed again for each host, so that neither kernel data nor
   a previous host's audio reaches a host. It is mapped into the attached host (readable and
   writable, never executable, through a C function of a few lines that catches the mapping's
   structured exception, which Rust cannot) and, through a partial MDL per stream, into the
   audio engine; the driver produces no audio and never reads it. The pages are
   reference-counted by the adapter,
   each stream buffer and the host's mapping, so none is freed while mapped. A user mapping is
   undone in its own process: on `DETACH`, on the handle's last close from any process
   (attaching to the host's), and from a process notification routine when the host's process
   exits; attaching and detaching are serialized, so the notification returns only once the
   mapping is gone.
4. **PortCls → miniport**: COM-style calls, possibly concurrent; reference counting and the
   real-time design below.
5. **Supply chain**: repository, CI, dependencies, and later the release signing.

### 2.3 Secure design principles applied

- **Least privilege**: the host interface open to administrators and the interactive user only;
  the buffer mapped into the attached host only; CI tokens read-only by default with no secrets.
- **Economy of mechanism**: one format, one host, one stream; no lock in the real-time path,
  a single writer (the stream's timer) and atomics; small, static descriptor tables.
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
| Out-of-bounds read or write (CWE-125, CWE-787) | Size checks before every access to a caller's buffer; ring writes through bounded slices; unaligned-safe reads and writes |
| Integer overflow (CWE-190) | 128-bit intermediates for clock arithmetic, saturating and checked operations; overflow checks in every build, release included (an overflow stops the system with a bug check naming its place instead of wrapping) |
| Use after free (CWE-416) | Atomic reference counting with `Arc`-like orderings, including the shared buffer's pages (the adapter, each stream buffer and the host's mapping hold one); timers deleted and DPCs flushed before a stream is freed; the notification event dropped and callbacks flushed before unregistration returns |
| Race conditions (CWE-362) | One writer per stream (its timer), positions published with release/acquire atomics, state changes serialized by PortCls |
| Double fetch, TOCTOU on user memory (CWE-367) | User input copied once, then validated (M4) |
| Kernel memory disclosure (CWE-200) | Buffers zeroed when allocated; the shared buffer made of whole zeroed pages, zeroed again for each host; no uninitialized structure returned |
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
  (`tools/capture-check`: frequency, level, discontinuities), run as a campaign of eleven
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
  raw device).
- The host crate `hostmic` (M5) under the same `unsafe` lints; its writes into the shared
  buffer are volatile, never through a Rust reference, and its pure parts (the clock, the
  ring's wrap, sample conversion) are unit-tested in CI.
- Review of every change through pull requests, an independent review of the PortCls adapter
  (M3), and of every kernel and host-crate pull request since M4.

### 2.6 Known gaps and plans

- A run with Memory Integrity on (M7: virtualization-based security
  requires Secure Boot, which test signing needs off, so it does not run in the test VM;
  Driver Verifier's code integrity checks and the CI check of the binaries stand in for it,
  section 2.5). Sleep and hibernation on a real machine (M7: a Hyper-V guest has no sleep
  state; the device's own idle transitions to D3 and back are tested in the VM, section 2.5).
  The host interface's
  IOCTLs are fuzzed only in their pure validation code, not in the kernel code that calls it:
  Microsoft's fuzzers leave that raw device out, and a fuzzer of our own is planned with the
  security review (M6).

- A single maintainer: no second human reviewer yet (GOVERNANCE.md).
- Release signing and provenance attestations (M7).
