# Security policy

hostmic is a **kernel-mode driver**. A bug in it can crash the whole machine or, worse, let an
unprivileged program read or write kernel memory. Security reports are taken seriously.

## Supported versions

None yet: the project is experimental and unsigned. Do not install it on a machine you care
about; use a virtual machine with test signing enabled.

## Reporting a vulnerability

**Please do not open a public issue.** Use GitHub's private reporting instead:
<https://github.com/Nopitch/hostmic/security/advisories/new> (on this repository, the
**Security** tab, then **Report a vulnerability**). Include:

- what an attacker can do (crash, information disclosure, privilege escalation…);
- the Windows version and the hostmic commit;
- a proof of concept or the steps to reproduce it, if you have them.

The report reaches the maintainer privately (see [GOVERNANCE.md](GOVERNANCE.md)).

## Coordinated disclosure

This is a spare-time project, with these commitments:

- **Acknowledgement** within 7 days of the report.
- **Assessment** (confirmed or not, severity) shared with the reporter within 14 days.
- **Fix** within 90 days of the report for a confirmed vulnerability, sooner for critical ones;
  the reporter is kept informed and may test the fix.
- **Disclosure**: the GitHub security advisory is published, with a CVE requested through
  GitHub when the issue warrants one, as soon as the fix is available, or at 90 days in
  agreement with the reporter. The reporter is credited unless they prefer otherwise.

Published advisories are listed at <https://github.com/Nopitch/hostmic/security/advisories>.
A vulnerability in a dependency that does not affect hostmic is recorded there too, with a
statement of why it is not exploitable (a VEX statement).

## Scope

In scope: the driver (IOCTL handling, buffer mapping, anything reachable from user mode) and the
host library. Out of scope: issues that require administrator rights or test signing to be
enabled, since those already grant full control of the machine.

What users can and cannot expect from the driver, its threat model and the reasons to trust
its design are in [docs/security-assurance.md](docs/security-assurance.md).

## Secrets and credentials

- **CI uses no repository secrets.** Workflows get a `GITHUB_TOKEN` restricted per job to the
  permissions they need (read-only by default); Scorecard publishes with a short-lived OIDC
  token. No long-lived token is stored in GitHub.
- **Nothing secret is committed**: `.gitignore` excludes keys and certificates, a pre-commit
  hook and the required `secrets` CI check scan for secrets, and GitHub push protection is on.
  The test-signing certificate is generated on the build machine, used only in the test VM, and
  never committed or published.
- **Release signing credentials** (PLAN.md, M7) are held only by the maintainer, outside the
  repository and outside CI logs (hardware token or password manager); release provenance uses
  GitHub's keyless OIDC attestations, so no signing key lives in CI.
- **Rotation**: a credential is revoked and replaced immediately if it may have been exposed,
  when someone with access leaves (GOVERNANCE.md), and otherwise when it expires.
- Access to any credential follows least privilege and is listed in GOVERNANCE.md.

## Dependencies (software composition analysis)

Dependencies come from standard sources only: crates from crates.io, pinned in `Cargo.lock`;
GitHub Actions pinned by full commit SHA; the Windows Driver Kit from Microsoft's NuGet package,
pinned by version. Dependabot raises alerts and update pull requests.

- **Every pull request** is checked by the required `dependencies` CI check (GitHub dependency
  review): a change that adds or updates a dependency with a known vulnerability of any
  severity, or with a malware advisory, is blocked. An exception needs a written statement in
  the pull request that the vulnerability does not affect hostmic (recorded as a VEX statement,
  see above).
- **Remediation thresholds** for findings on existing dependencies: critical and high within
  14 days, medium within 30 days, low at the next release.
- **License findings**: dependencies must be under licenses compatible with the project's
  (permissive OSI licenses such as MIT, Apache-2.0, BSD, ISC, Zlib, Unicode); any other license
  is removed or replaced before merging. The `dependencies` check enforces it on every pull
  request with an allow-list of SPDX identifiers (`.github/workflows/security.yml`); adding a
  license to the list is a decision of its own, in its own pull request.
- **Before any release**, there is no open dependency finding (vulnerability or license) above
  low severity, unless it is documented as not affecting hostmic.

## Static analysis (SAST)

CodeQL (Rust and GitHub Actions), clippy and zizmor analyse every pull request.

- Code-scanning findings of **high or critical** security severity block merging (repository
  ruleset), as do clippy warnings and zizmor findings (required checks).
- **Medium** findings are fixed, or dismissed with a written justification, before merging;
  **low** findings before the next release.
- A finding dismissed as a false positive or as not exploitable keeps its justification in
  GitHub code scanning.
