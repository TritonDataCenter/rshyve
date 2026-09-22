# Security policy

## Status

This is experimental software. It is not production-supported, and it
has not completed a security qualification. Do not run untrusted guests
on it.

## What this project defends

`rshyve` and `firehyve` are virtual machine monitors. The security
boundary that matters is **guest to host**: a guest must not be able to read or write host memory outside
its own address space, execute code in the VMM process, escape its zone,
or crash the VMM.

The release profile sets `panic = "abort"`. So **a panic that
guest-controlled input can cause is a denial of service, and we treat it
as a security bug**. Guest input includes port I/O, MMIO, descriptor
rings, and every device command queue.

Other boundaries, from the most hardened to the least:

| Boundary | Current state |
|---|---|
| Guest to host | The primary boundary. Report anything that crosses it. |
| Control socket to VMM | Unix socket, owner-only, with peer credential authorization, bounded requests, and deadlines. Local root is already trusted. |
| Migration peer to VMM | **Unauthenticated and unencrypted.** See below. |
| VNC client to VMM | Local Unix socket, owner-only. Legacy DES challenge auth, no transport encryption. |

## Known gaps

These gaps are known. They are not eligible for disclosure credit, but
reports that make one of them worse are welcome.

- **Live migration has no peer authentication or transport encryption**
  (OSF-1). Guest RAM, CPU state and device state cross the wire in clear
  text, and the destination accepts the first connection. Migrate only
  over a trusted channel that is authenticated by other means.
- **Live migration does not transfer FPU/XSAVE state** (OSF-2). A guest
  that uses floating-point or vector registers across a migration can
  see corrupt values.
- **PCI passthrough** gives the guest access to physical device
  configuration space, and it is not fully audited (OSF-5, OSF-11).
- **virtio-fs runs in the VMM process** (OSF-10). A writable share gives
  the guest the VMM's credentials on that directory tree.

[docs/security/open-findings.md](docs/security/open-findings.md) is the
current register.

## Reporting a vulnerability

Report privately. Do not open a public issue for a suspected
vulnerability.

Use GitHub's private vulnerability reporting on this repository
(Security tab, "Report a vulnerability"). It creates a private advisory
visible only to the maintainers.

Please include:

- the affected revision,
- the guest or peer input that triggers it,
- what boundary you believe it crosses,
- a reproducer if you have one.

## Response

- Acknowledgement within 5 business days.
- An initial assessment, with a severity and a fix target, within 15
  business days.
- Default embargo of 90 days from acknowledgement, extendable by
  agreement if a fix needs coordination with upstream projects.

Some of this code comes from
[Propolis](https://github.com/oxidecomputer/propolis). If a defect also
exists upstream, we coordinate with Oxide Computer Company before public
disclosure.

## Supported versions

No release has been published. Until one is, only the `master` branch
receives fixes.
