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

## Workflow

1. Open an issue to discuss anything non-trivial before writing code.
2. Fork, create a branch, open a pull request against `main`.
3. The required checks must pass. Pull requests are squash-merged by the maintainer.

## Licensing of contributions

By contributing, you agree that your contribution is licensed under the license of the crate it
belongs to: the Microsoft Public License (MS-PL) for the driver, MIT OR Apache-2.0 for the host
library (see [README.md](README.md#licensing)).

## Code of conduct

This project follows the [Contributor Covenant](CODE_OF_CONDUCT.md).
