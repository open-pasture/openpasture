# Security

openpasture sends signed boundaries to collars on live animals. A forged or altered boundary,
a stolen collar key or a hijacked update can move or stress a herd, put animals on a road, or
leave them without a fence. Please report anything that could lead there privately, and give us
time to fix it before you publish.

## Report a vulnerability

Use GitHub's private reporting:
[Report a vulnerability](https://github.com/open-pasture/openpasture/security/advisories/new)
(the **Security** tab, then **Report a vulnerability**). Only the maintainers see the report.

Don't open a public issue, pull request or discussion for a vulnerability.

Include what you can: the version (`openpasture version`, or the app's About), how it is run
(desktop app, server binary, Docker), the steps to reproduce, and what an attacker gains. A
proof of concept against your own install is welcome. Never test against a farm that isn't
yours.

We acknowledge a report within 3 working days, and send a first assessment within 10. We fix
problems that can affect animals or a collar's fence first. We credit you in the advisory unless
you ask us not to.

## Supported versions

Only the latest release gets security fixes. The desktop app offers updates itself; servers
and Docker images need upgrading by hand (see `docs/SELF-HOSTING.md`).

## Scope

In scope, in this repository:

- The server (`openpasture serve`, the Docker image): authentication, roles and tokens, the
  collar endpoints, report ingest, boundary and config signing, the MCP server, texting and
  webhooks (Twilio, email, Web Push), imports and exports, the SQL console.
- Collar key handling: issuing, storing and rotating collar keys and the server signing key.
- Anything that lets a boundary reach a collar without the checks it should pass: farm edge,
  roads, hazards, a person's approval.
- The desktop app and its updater (signature checks on updates).
- The release pipeline: anything that could get unsigned or altered code into a release.
- `op-protocol` and `op-geo`, the Apache-2.0 crates the firmware and other clients use.

Collar firmware, the device protocol spec and the hardware live in
[open-pasture/opencollar](https://github.com/open-pasture/opencollar); report those there. If
you're unsure where a problem belongs, report it here and we'll move it.

Out of scope: problems that need an attacker who already controls the server's machine or its
data directory, reports from automated scanners without a demonstrated impact, and denial of
service by sheer volume of traffic.
