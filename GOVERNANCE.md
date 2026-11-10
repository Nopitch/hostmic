# Governance

hostmic is a single-maintainer project. This file says who has access to what, who does what,
and how access is granted. [PLAN.md](PLAN.md) records technical decisions; this file records
people and permissions.

## Members and access to sensitive resources

| Member | Role | Access |
|---|---|---|
| [Nopitch](https://github.com/Nopitch) | Maintainer, owner | GitHub repository administration (settings, rulesets, Actions configuration), merging to `main`, `v*` tags and releases, private security advisories and vulnerability reports, the OpenSSF Best Practices badge entry, and, once releases are signed (PLAN.md, M7), the release signing account |

No one else has write access to the repository, its settings, its CI configuration or its
security reports. The list is kept up to date in this file whenever access changes.

## Roles and responsibilities

- **Maintainer.** Sets the direction (PLAN.md); reviews contributors' pull requests and decides
  what is merged; keeps CI, dependencies and repository settings secure; handles vulnerability
  reports as described in [SECURITY.md](SECURITY.md); prepares releases; tests the driver in the
  test VM before merging driver changes.
- **Contributors.** Anyone may open issues and pull requests, following
  [CONTRIBUTING.md](CONTRIBUTING.md) and the [code of conduct](CODE_OF_CONDUCT.md).
- **Security reporters.** Report vulnerabilities privately, as described in SECURITY.md.

## Granting access

Escalated permissions (write access, maintenance or administration of the repository, access
to security reports or to release signing) are granted only:

- to someone with a track record of reviewed contributions to the project;
- after the maintainer has reviewed that person's contributions and their GitHub account, with
  two-factor authentication required by GitHub;
- at the lowest level that their role needs (least privilege), recorded in the table above in
  the same change.

Access is reviewed whenever a role changes and at least once a year, and removed when it is no
longer needed or after a year of inactivity.

## Decisions

The maintainer decides, after discussion in issues or pull requests when a change is
non-trivial. Decisions that change the project's direction are recorded in PLAN.md.
