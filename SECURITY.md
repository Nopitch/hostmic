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

This is a spare-time project: expect an acknowledgement within a week. Once a fix is ready, the
advisory is published with credit to the reporter, unless they prefer otherwise.

## Scope

In scope: the driver (IOCTL handling, buffer mapping, anything reachable from user mode) and the
host library. Out of scope: issues that require administrator rights or test signing to be
enabled, since those already grant full control of the machine.
