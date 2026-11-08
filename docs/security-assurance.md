# Security: what to expect, and why to trust it

hostmic is a Windows kernel-mode driver. This document states what users can and cannot expect
from it in terms of security (its security requirements), then argues why the design and the
development process meet them (an assurance case). It covers the state after milestone M3 and
says what later milestones change; [PLAN.md](../PLAN.md) has the roadmap,
[SECURITY.md](../SECURITY.md) the vulnerability process and the security rules.

## 1. What you can and cannot expect

### You can expect

- **No path from user mode to kernel memory.** No input reachable from user mode (Kernel
  Streaming property requests and data formats today, the host's IOCTLs from M4) lets a process
  read or write kernel memory, or memory of another process.
- **Only the attached host gets the buffer.** From M4, the client's audio buffer is mapped into
  one process only, the host that attached through the host interface, and unmapped when that
  host detaches or dies. Opening the host interface is restricted to administrators and the
  interactive user.
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

1. **User mode → kernel, through Kernel Streaming** (today): property handlers
   (`PROPOSEDATAFORMAT`, jack descriptions), data-range intersection, stream creation with a
   client-supplied data format. Each handler checks instance and value sizes before reading or
   writing; data formats are parsed by `hostmic_driver_core::format`, pure Rust with no kernel
   call, unit-tested on every truncation of a valid format.
2. **User mode → kernel, through the host interface** (M4): IOCTL input validation in pure Rust,
   unit-tested and fuzzed (PLAN.md, M4); user buffers copied once, then validated.
3. **Shared memory** (M4): the ring buffer mapped into the host; the driver never reads data back
   from it to make decisions.
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
  `// SAFETY:` comment (enforced by clippy), plus static analysis, fuzzing, VM tests and,
  planned, Driver Verifier.

### 2.4 Common weaknesses and how they are countered

| Weakness (CWE) | Countermeasure |
|---|---|
| Improper input validation (CWE-20) | Allowlist checks of formats and sizes in pure, unit-tested Rust (`format::is_supported`); IOCTL validation the same way; both fuzzed |
| Out-of-bounds read or write (CWE-125, CWE-787) | Size checks before every access to a caller's buffer; ring writes through bounded slices; unaligned-safe reads and writes |
| Integer overflow (CWE-190) | 128-bit intermediates for clock arithmetic, saturating and checked operations; overflow checks in tests and debug builds |
| Use after free (CWE-416) | Atomic reference counting with `Arc`-like orderings; timers deleted and DPCs flushed before a stream is freed; the notification event dropped and callbacks flushed before unregistration returns |
| Race conditions (CWE-362) | One writer per stream (its timer), positions published with release/acquire atomics, state changes serialized by PortCls |
| Double fetch, TOCTOU on user memory (CWE-367) | User input copied once, then validated (M4) |
| Kernel memory disclosure (CWE-200) | Buffers zeroed when allocated; no uninitialized structure returned |
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
  (`tools/capture-check`: frequency, level, discontinuities).
- Review of every change through pull requests, and an independent review of the PortCls
  adapter (M3).

### 2.6 Known gaps and plans

- Driver Verifier and an HVCI run in the test VM, sleep and resume, host crash and endurance
  tests (M6). Fuzzing covers the pure validation code, not the kernel code that calls it.

- A single maintainer: no second human reviewer yet (GOVERNANCE.md).
- Release signing and provenance attestations (M7).
