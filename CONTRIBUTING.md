# Contributing to hostmic

Thanks for your interest! hostmic is at an early, experimental stage: read [PLAN.md](PLAN.md)
first, since it is the source of truth for what is being built and in which order.

## Ground rules

- **Never test the driver on your everyday machine.** Use a virtual machine with test signing
  enabled and a snapshot to roll back to. A kernel bug means a blue screen.
- **One notion per pull request**, small enough to review carefully. Kernel code is reviewed line
  by line.
- **Every `unsafe` block carries a `// SAFETY:` comment** stating the invariant that makes it
  sound.
- **Real-time paths** (anything called per audio period) never allocate, lock or do I/O.
- **Security issues go through private reporting**, never public issues: see
  [SECURITY.md](SECURITY.md).
- Everything is in English: code, comments, documentation, commit messages.

## Coding standards

- **Rust code** follows the [Rust Style Guide](https://doc.rust-lang.org/style-guide/) as
  rustfmt applies it, with its default configuration, and the
  [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/) for public items.
- **Lints**: clippy's `all` group is denied and `pedantic` enabled for every crate; in the
  driver, `unsafe_op_in_unsafe_fn`, `undocumented_unsafe_blocks` and
  `multiple_unsafe_ops_per_block` are forbidden (one unsafe operation per block, each with its
  `// SAFETY:` comment). An exception is a local `#[allow(…, reason = "…")]` that says why.
- **Documentation**: every public item has a doc comment (`missing_docs`); comments explain
  why, not what.
- **GitHub Actions workflows** pass [zizmor](https://docs.zizmor.sh/): least-privilege
  permissions, actions pinned by full commit SHA, no untrusted data expanded in scripts.

CI enforces all of it on every pull request (`cargo fmt --all --check`,
`cargo clippy --all-targets --locked -- -D warnings`, zizmor).

## Maintainer availability

hostmic has one maintainer, working on it in spare time ([GOVERNANCE.md](GOVERNANCE.md)). Expect a
first answer to an issue or a pull request within **7 days**: an acknowledgement, a question, or
a reason why it does not fit. A review can take longer; a pull request is never left without an
answer. Security reports follow their own timeline ([SECURITY.md](SECURITY.md)).

## What helps most now

hostmic is experimental ([PLAN.md](PLAN.md) has the roadmap and what comes next). The most
useful contributions today:

- **Testing in a virtual machine** on Windows versions and hardware set-ups the maintainer does
  not have, following [docs/test-vm.md](docs/test-vm.md), and reporting what you measured
  (`hostmic-ctl status`, `capture-check`).
- **Reviewing kernel code**: pull requests that touch the driver are read line by line, and a
  second pair of eyes is welcome on any of them.
- **Documentation**: what was unclear, wrong or missing when you set it up.
- **Hosts**: applications that feed the microphone, and feedback on the host interface
  (`hostmic-abi`) before the host library (M5) settles it.

Issues labeled [`good first issue`](https://github.com/Nopitch/hostmic/labels/good%20first%20issue)
are small and self-contained. Questions go to
[Discussions](https://github.com/Nopitch/hostmic/discussions), bugs to issues.

## Workflow

1. Open an issue to discuss anything non-trivial before writing code.
2. Fork, create a branch, open a pull request against `main`.
3. The required checks must pass. Pull requests are squash-merged by the maintainer.

## Build and test

Requirements (Windows 10 or 11, x64), the same as the `driver` CI job:

- Rust through [rustup](https://rustup.rs/); the toolchain version is pinned in
  `rust-toolchain.toml` and installed automatically, with rustfmt and clippy.
- The MSVC build tools and the Windows SDK: Visual Studio (Community is enough) or Build Tools
  with the "Desktop development with C++" workload.
- The Windows Driver Kit 10.0.26100 (CI installs it from NuGet, `Microsoft.Windows.WDK.x64`
  10.0.26100.6584).
- LLVM for bindgen: `libclang.dll`, found through `LIBCLANG_PATH` (for example
  `C:\Program Files\LLVM\bin`).
- `cargo install cargo-wdk --version 0.2.0 --locked`.

Rust dependencies are crates from crates.io, pinned in `Cargo.lock` and fetched by Cargo.

- Build and package the driver: `cargo wdk build` (cargo-wdk 0.2.0). The test-signed package
  lands in `target/debug/hostmic_driver_package/`. Keep `target/` inside the checkout:
  `wdk-build` reads the WDK configuration from the workspace above it.
- Lint: `cargo fmt --all --check` and `cargo clippy --all-targets --locked -- -D warnings`.
- Unit tests: `cargo test --locked`. CI runs all of the above on every pull request.
- The driver itself is only tested in a virtual machine: [docs/test-vm.md](docs/test-vm.md).
- Fuzzing: `fuzz/` holds [cargo-fuzz](https://rust-fuzz.github.io/book/cargo-fuzz.html)
  targets for the validation of untrusted input. They need the nightly toolchain pinned in
  `.github/workflows/fuzz.yml` and cargo-fuzz 0.13.2 (on Windows, also MSVC's AddressSanitizer,
  see the cargo-fuzz book); for example `cargo +nightly-2026-10-26 fuzz run attach --
  -max_total_time=60`, and `cargo fuzz list` for the targets. The `fuzz` CI job runs each
  target for 60 s on every pull request and 10 minutes every week.

## Tests

New functionality comes with automated tests. Logic that needs no kernel call lives in a crate
that runs on the build machine, such as `hostmic-driver-core`, and gets unit tests there; when
it handles input from user mode or from PortCls, it also gets a fuzz target in `fuzz/`. Code
that only runs in the kernel gets a reproducible procedure in [docs/test-vm.md](docs/test-vm.md),
with what to expect. A pull request says how it was tested.

## Licensing of contributions

By contributing, you agree that your contribution is licensed under the license of the crate it
belongs to: the Microsoft Public License (MS-PL) for the driver, MIT OR Apache-2.0 for the host
library (see [README.md](README.md#licensing)).

**Every commit is signed off** under the [Developer Certificate of Origin 1.1](https://developercertificate.org/):
the `Signed-off-by: Your Name <your@email>` line certifies that you wrote the change or have
the right to submit it under the project's licenses. Add it with `git commit -s` (or
`git commit --amend -s`, `git rebase --signoff main` for existing commits). The required `dco`
check rejects a pull request with an unsigned commit.

## Code of conduct

This project follows the [Contributor Covenant](CODE_OF_CONDUCT.md).
