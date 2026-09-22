# rust-bhyve

A userspace virtual machine monitor (VMM) for illumos bhyve, written in
Rust. It runs on SmartOS and other illumos distributions, on x86_64.

**Experimental.** This is not production-supported software, and no
release is published. Live migration is not authenticated and does not
carry FPU/XSAVE state. Read [SECURITY.md](SECURITY.md) before you run an
untrusted guest on it.

## The binaries

- **`rshyve`** takes the command line that the SmartOS bhyve zone brand
  builds, so a zone can run it in place of `/usr/sbin/bhyve`. It boots
  UEFI firmware or a Linux kernel directly, and supplies virtio, NVMe,
  an AHCI CD-ROM, a VNC framebuffer, a vTPM, PCI passthrough, a control
  socket and live migration. It is not all of bhyve: some flags and
  device options are refused, and some are accepted and ignored.
- **`firehyve`** is a microVM over the same crates. It boots a Linux
  kernel directly and supplies virtio devices and a serial console only:
  no firmware, no migration, no control socket. It refuses at startup
  most options that it cannot honor. The options that the bhyve zone
  brand puts on every zone (a boot ROM, extra serial backends, a frame
  buffer and an xHCI tablet) are accepted, logged and ignored.
- **`fhrun`** runs a Linux binary inside a firehyve microVM, like a
  child process.

[docs/features.md](docs/features.md) lists what each binary supports,
with its limits.

## Quick start

```bash
rshyve -c 2 -m 2G \
  -s 4,virtio-blk,/dev/zvol/rdsk/zones/disk0 \
  -s 6,virtio-net-viona,net0 \
  -l bootrom,/usr/share/bhyve/uefi-rom.bin \
  -l com1,stdio \
  myvm
```

## Build

The binaries run only on illumos. Build them on an illumos host:

```bash
git clone --recurse-submodules https://github.com/TritonDataCenter/rshyve
cd rust-bhyve
cargo build --workspace --locked --release
cargo test --workspace --locked --no-fail-fast
```

The unit tests also run on Linux and macOS. [docs/testing.md](docs/testing.md)
gives the prerequisites, how to boot a test guest, and the test harness
scripts.

To use `rshyve` for the zones on a SmartOS node, install it on the node
and point the zone at it. [docs/platform.md](docs/platform.md) tells how.
No prebuilt platform image is published.

## Status

Tested on SmartOS with bhyve API v18. [docs/features.md](docs/features.md#tested-configurations)
lists the configurations that were tested and the known caveats.
[docs/roadmap.md](docs/roadmap.md) lists the open work.

## Documentation

- [Architecture](docs/architecture.md): workspace layout, key types, data flow
- [Feature reference](docs/features.md): what each binary supports, with limits
- [CLI reference](docs/cli.md): flags and control commands
- [Devices](docs/devices.md): PCI, LPC and kernel-emulated devices
- [Live migration](docs/migration.md): protocol, state and the orchestrator contract
- [Metadata agent](docs/mdata.md): guest configuration over COM2
- [Platform integration](docs/platform.md): running rshyve for SmartOS zones
- [UEFI firmware](docs/firmware-edk2-bhyve.md): the FreeBSD edk2 firmware and variable stores
- [fhrun](docs/fhrun.md): the manifest and the exit codes
- [Testing](docs/testing.md): build, unit tests, test guests, harnesses
- [Roadmap](docs/roadmap.md): open work
- [firehyve boot measurements](docs/firehyve-whitepaper.md): a draft measurement report
- [Open security findings](docs/security/open-findings.md)

## Built on Propolis

This project uses code from [Propolis](https://github.com/oxidecomputer/propolis),
the bhyve VMM of Oxide Computer Company. The bhyve and viona ioctl
bindings, the address-space manager and the PS/2 device models come from
Propolis with few changes. Parts of the PCI, UART and VM-exit layers are
derived from it. Both projects use MPL-2.0.

Each derived file has a header that names its upstream file.
[THIRD_PARTY.md](THIRD_PARTY.md) lists the files with their measured
similarity, and `tools/propolis-derivation.py` measures them again
against a Propolis checkout.

## Contributing

[CONTRIBUTING.md](CONTRIBUTING.md) covers the build, what CI proves and
what it does not, the repository gates, and the commit style.
[CHANGELOG.md](CHANGELOG.md) records the changes that an operator would
notice.

## Security

Report vulnerabilities privately. See [SECURITY.md](SECURITY.md).

## License

MPL-2.0. The full text is in [LICENSE](LICENSE).

Parts of this code are copied or derived from
[Propolis](https://github.com/oxidecomputer/propolis), Copyright Oxide
Computer Company, under MPL-2.0. libtpms is vendored under the 3-clause
BSD license. [NOTICE](NOTICE) has the attribution, and
[THIRD_PARTY.md](THIRD_PARTY.md) has the exact provenance.
