# hostmic

Open-source Windows virtual microphone driver fed by a user-mode host, in Rust. **`PLAN.md` is
the source of truth**: read it before proposing anything, update it when a decision changes.

## Rules

- Everything in English: code, comments, docs, commit messages, pull requests.
- One notion per pull request. Explain the why before the code.
- Every `unsafe` block has a `// SAFETY:` comment (enforced by clippy:
  `undocumented_unsafe_blocks`, `multiple_unsafe_ops_per_block`).
- Real-time paths (anything called per audio period): no allocation, no lock, no I/O.
- **The driver is only ever loaded in a test VM** (Hyper-V, test signing on, snapshot first),
  never on the maintainer's everyday machine (Secure Boot, anti-cheats).
- Before using a crate API, read its docs on docs.rs for the version in `Cargo.lock`.
- Rust is pinned in `rust-toolchain.toml`; bump it on purpose only.

## Build

`cargo wdk build` (cargo-wdk 0.2.0) from the repository root, with LLVM's `bin` on `PATH` and
`LIBCLANG_PATH` set to it. Output: `target/debug/hostmic_driver_package/` (`.sys`, `.inf`,
`.cat`, test certificate). `cargo make` is not used: it needs symbolic links (Developer Mode).
Lints: `cargo fmt --all --check` and `cargo clippy --all-targets -- -D warnings`.

## Git and CI

- Branches and pull requests; the maintainer merges them himself (`Protect main` ruleset:
  required checks `zizmor`, `secrets`, `driver`, `dependencies` and `dco`, high or critical
  CodeQL alerts blocking, squash only, code-owner review, admin bypass through a pull request).
  Never stack pull requests.
- Every commit is signed off (`git commit -s`, Developer Certificate of Origin): the `dco` check
  rejects unsigned commits.
- Merge command for the maintainer (PowerShell), waiting for the head and the 5 required checks:
  `$h = gh pr view <n> --repo Nopitch/hostmic --json headRefOid -q .headRefOid; while ([int](gh pr checks <n> --repo Nopitch/hostmic --required --json name --jq length 2>$null) -lt 5) { Start-Sleep 10 }; gh pr checks <n> --repo Nopitch/hostmic --required --watch --fail-fast; if ($LASTEXITCODE -eq 0) { gh pr merge <n> --repo Nopitch/hostmic --squash --delete-branch --admin --match-head-commit $h }`
- Public repository: GitHub-hosted runners only, never self-hosted. Actions pinned by full
  commit SHA (enforced by the repository settings).
- `Protect tags`: `v*` tags can be neither moved nor deleted.
