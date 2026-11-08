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

## Tests

New functionality comes with automated tests. Logic that needs no kernel call lives in a crate
that runs on the build machine, such as `hostmic-driver-core`, and gets unit tests there. Code
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
