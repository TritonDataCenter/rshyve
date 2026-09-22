# Changelog

Notable changes per release, newest first. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[semantic versioning](https://semver.org/spec/v2.0.0.html) once there is
a version to follow.

No release has been published. Until one is, `Unreleased` is the whole
file and `git log` is the detailed record. Add an entry here for anything
a user or an operator would notice: a new device, a changed flag, a
changed default, a removed feature, or a security fix.

## Unreleased

### Added

- `firehyve`, a microVM binary over the same crates as `rshyve`: direct
  kernel boot, virtio devices, stdio serial console, no UEFI, migration,
  control socket or passthrough.
- `fhrun`, which runs a Linux binary inside a `firehyve` microVM like a
  child process.
- virtio-fs, virtio-vsock and virtio-console devices; PCI, CPU and memory
  hot-plug behind `--hotplug`.
- A vTPM 2.0 over libtpms, with a TCG PTP CRB, a TPM2 ACPI table and
  file-backed NV state.
- Repository gates: public-surface, unsafe-comment, CI-coverage,
  dependency-boundary and test-parity, each with a CI self-test.

### Changed

- `rust-bhyve` is the project name. Earlier documents called it `vmmnew`
  or `vmm`.

### Security

- Unresolved findings are tracked in
  [docs/security/open-findings.md](docs/security/open-findings.md). The
  ones that gate production use today: live migration has no peer
  authentication or encryption (OSF-1), migration does not transfer
  FPU/XSAVE state (OSF-2), and PCI passthrough has not been audited
  (OSF-5).
- The vendored libtpms is not affected by CVE-2026-6726 or
  CVE-2026-6727. libtpms no longer compiles if its own RSA decrypt code,
  which CVE-2026-6727 affects, would be built.
