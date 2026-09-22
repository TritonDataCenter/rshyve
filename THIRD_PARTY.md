# Third-party provenance

Every copied, forked, vendored, or redistributed component in this
repository, with its upstream revision, license, and local changes.
`NOTICE` carries the attribution text required for redistribution; this
file carries the detail needed to audit or refresh a component.

## Derived source: Propolis

| | |
|---|---|
| Upstream | https://github.com/oxidecomputer/propolis |
| License | MPL-2.0 |
| Copyright | Oxide Computer Company |
| Form | Source copied and adapted into this tree |

**This project would not exist in its current form without Propolis.**
The entire bhyve and viona kernel interface, the address-space manager,
the PS/2 device models, and parts of the PCI, UART, and VM-exit layers
originate there.

| | |
|---|---|
| Measured against Propolis revision | `b428c40efae0be950e996f9dfd5ab8d02db59184` (2026-08-06) |
| Files at or above 30% similarity | 19 |
| Significant lines in those files | 3300 |
| Verbatim / derived / partial | 6 / 6 / 7 |

Reproduce every number and every row below with:

```sh
tools/propolis-derivation.py /path/to/propolis
```

The tool prints the revision it measured. Record it in the table above
whenever these rows are regenerated, because the scores move as upstream
moves. Similarity is an ordered sequence ratio over non-trivial lines, so
it reflects real derivation rather than shared Rust boilerplate. Every
file listed carries an attribution header naming its upstream
counterpart.

Copied essentially verbatim (95% and above):

| File | Similarity | Upstream |
|---|---|---|
| `crates/vmm-hid/src/ps2/keyboard/scan_code_2.rs` | 100% | `lib/propolis/src/hw/ps2/keyboard/scan_code_2.rs` |
| `crates/vmm-hid/src/ps2/keyboard/scan_code_1.rs` | 100% | `lib/propolis/src/hw/ps2/keyboard/scan_code_1.rs` |
| `crates/bhyve-api/sys/src/enums.rs` | 100% | same path upstream |
| `crates/vmm-hid/src/ps2/keysym.rs` | 99% | `crates/rfb/src/keysym.rs` |
| `crates/bhyve-api/sys/src/vmm_data.rs` | 99% | same path upstream |
| `crates/bhyve-api/sys/src/ioctls.rs` | 96% | same path upstream |

Substantially derived (60% to 95%):

| File | Similarity | Upstream |
|---|---|---|
| `crates/vmm-hid/src/ps2/keyboard/mod.rs` | 94% | `lib/propolis/src/hw/ps2/keyboard/mod.rs` |
| `crates/viona-api/src/ffi.rs` | 92% | same path upstream |
| `crates/bhyve-api/sys/src/structs.rs` | 91% | same path upstream |
| `crates/bhyve-api/src/lib.rs` | 78% | same path upstream |
| `crates/vmm-core/src/aspace.rs` | 72% | `lib/propolis/src/util/aspace.rs` |
| `crates/vmm-devices/src/migrate.rs` | 69% | `lib/propolis/src/migrate.rs` |

Partly derived (30% to 60%):

| File | Similarity | Upstream |
|---|---|---|
| `crates/vmm-devices/src/pci/bits.rs` | 51% | `lib/propolis/src/hw/pci/bits.rs` |
| `crates/vmm-core/src/exits.rs` | 49% | `lib/propolis/src/exits.rs` |
| `crates/vmm-hid/src/ps2/ctrl.rs` | 44% | `lib/propolis/src/hw/ps2/ctrl.rs` |
| `crates/viona-api/src/lib.rs` | 42% | same path upstream |
| `crates/vmm-devices/src/pci/bar.rs` | 41% | `lib/propolis/src/hw/pci/bar.rs` |
| `crates/vmm-devices/src/uart/uart16550.rs` | 35% | `lib/propolis/src/hw/uart/uart16550.rs` |

The tool also reports `crates/vmm-core/src/lib.rs` at 32%. That file has
22 significant lines, all of them `pub mod` and `pub use` declarations,
and it carries no upstream attribution header because it is not derived.
It is a boilerplate coincidence at the reporting threshold, listed here
so a reader who runs the tool is not left wondering.

Derived but below the threshold, hand-added:

| File | Upstream | Why it is under 30% |
|---|---|---|
| `crates/vmm-hid/src/ps2/kbd.rs` | `lib/propolis/src/hw/ps2/ctrl.rs` (`PS2Kbd`) | Split out of a file upstream keeps whole |
| `crates/vmm-hid/src/ps2/mouse.rs` | `lib/propolis/src/hw/ps2/ctrl.rs` (`PS2Mouse`) | Same split |
| `crates/dladm/src/sys.rs` | same path upstream | Rewritten around the same libdladm declarations |
| `crates/vmm-core/src/intr_pins.rs` | `lib/propolis/src/intr_pins.rs` | Diverged; the pin trait and kernel calls are ours |

Splitting `PS2Kbd` and `PS2Mouse` across three files puts each below the
per-file reporting threshold, so the tool does not list them. They are
derived all the same, and all four files carry an attribution header.

### Licensing

Both projects are MPL-2.0, so the licenses are compatible and the copy
is licensed. MPL-2.0 section 3.4 notice preservation is satisfied: every
derived file retains its MPL Exhibit A header, and now also names its
upstream source.

Only `keysym.rs` carries an Oxide copyright line, because it is the only
one that had one upstream. Propolis states a copyright on 13 of its 317
Rust files; the others we copied had none to preserve. Nothing was
stripped.

### Shared defects

A fix made here can apply upstream too, because parts of this tree are
copies. Anything of that kind goes through the coordinated-disclosure
process in `SECURITY.md`, and is described in this file only once
upstream has fixed it or has agreed to publication. So this file names
no trigger and no upstream line number, whatever its state today.

## Submodule: libtpms

| | |
|---|---|
| Upstream | https://github.com/stefanberger/libtpms |
| Revision | `521c51073fe6f7c56023db78e56961fcaf7906e8` (untagged `master`, 2026-03-13) |
| License | BSD-3-Clause |
| Copyright | IBM Corporation 2006-2011; Microsoft Corporation 2010-2022; Trusted Computing Group and contributors 2022-2025 |
| Location | `third_party/libtpms` (submodule, pinned to the revision above) |
| License text | `LICENSES/BSD-3-Clause-libtpms.txt` (source not redistributed here) |
| Local changes | `third_party/libtpms-illumos/patches/`, applied at build time |
| Linkage | Static, via `crates/vmm-tpm-sys/build.rs` |

The submodule is pristine upstream, so there is no vendored copy to
audit. Confirm the pin and that the patches still apply:

```sh
git -C third_party/libtpms rev-parse HEAD    # must match the revision above
git submodule status third_party/libtpms     # a leading '+' means drift
```

`crates/vmm-tpm-sys/build.rs` copies the submodule into `OUT_DIR` and
applies every patch there, failing the build if one does not apply. The
submodule is never modified, so the patch set below is the complete and
mechanically enforced statement of what this project changes.

`PORTING_NOTES.md` previously recorded the source as "v0.11.0". No such
upstream tag exists; 0.11.0 is the in-development version string in
`configure.ac`. The revision above was recovered by exhaustive diff and
is authoritative.

### Local patches

Both live in `third_party/libtpms-illumos/patches/` and are applied to
the `OUT_DIR` copy by `build.rs`:

1. `0001-illumos-strchr.patch`: `src/tpm2/RuntimeProfile.c` uses
   `strchr()` instead of the legacy `index()`, which illumos does not
   declare under the default feature-test macros.
2. `0002-illumos-endian.patch`: adds a `__sun` / `__illumos__` arm to
   the endianness cascade in `TpmProfile_Common.h`.

### License exception inside the vendored tree

`m4/ax_check_linker_flag.m4` inside the libtpms submodule is
GPL-2.0-or-later
with the Autoconf Macro exception (Copyright 2008 Guido U. Draheim,
2011 Maarten Bosmans). The exception means it does not affect the
license of the build output. It is the only GPL-licensed file in the
tree and is called out here so a license scan of this repository has an
explanation to match against. It reaches a build host through the
submodule, not through this repository's own tracked files.

### CERT VU#431093

CERT VU#431093 lists two libtpms CVEs. This build is not affected by
either:

| CVE | Why this build is not affected |
|---|---|
| CVE-2026-6726 (CVSS 8.5) | The pinned revision has the upstream fix "tpm2: Initialize a whole OBJECT before using it" (libtpms 0.8.5 and later). `FindEmptyObjectSlot()` in `src/tpm2/TPMCmd/tpm/src/subsystem/Object.c` zeroes the whole slot before reuse, so a reused slot never exposes freed key material. CERT records the vendor statement that libtpms is not affected. |
| CVE-2026-6727 (CVSS 8.3) | The affected `OaepDecode()` has one caller, the reference `CryptRsaDecrypt()` in `src/tpm2/TPMCmd/tpm/src/crypt/CryptRsa.c`. libtpms compiles it only when `USE_OPENSSL_FUNCTIONS_RSA` is 0. This build sets it to 1, so guest RSA decryption runs through OpenSSL `EVP_PKEY_decrypt()`. CERT records the vendor statement that only a build without OpenSSL RSA functions is affected. |

Patch `third_party/libtpms-illumos/patches/0003-require-openssl-rsa.patch`
keeps CVE-2026-6727 out of the build: it adds an `#error` to `CryptRsa.c`
that fires whenever `USE_OPENSSL_FUNCTIONS_RSA` is not 1, after every
compiler flag has resolved. The build stamp includes the patch set, so a
cached build cannot skip it. Check both CVEs again when you re-vendor
libtpms.

## Referenced specification: VIRTIO 1.3

| | |
|---|---|
| Upstream | https://docs.oasis-open.org/virtio/virtio/v1.3/csd01/virtio-v1.3-csd01.html |
| Copyright | OASIS Open 2023. All Rights Reserved. |

Not redistributed. The virtio device models are written against this
specification and cite it by section number.

## Fetched at build time: edk2-bhyve firmware

| | |
|---|---|
| Upstream | FreeBSD edk2-bhyve package |
| License | BSD-3-Clause, text at `LICENSES/BSD-3-Clause-edk2.txt` |
| Fetcher | `tools/fetch-edk2-bhyve.sh` |

Not vendored. The fetch script pins the package URL and verifies
SHA-256 and size for both the package and the extracted `CODE`/`VARS`
images before installing them. `docs/firmware-edk2-bhyve.md` records the
provenance table.

## Cargo dependencies

The dependency graph is pinned by `Cargo.lock`. Every package in it
declares an SPDX license expression; there is no GPL or AGPL. The spread
is predominantly MIT/Apache-2.0, with Unicode-3.0, BSD-2-Clause, Zlib,
and this project's own MPL-2.0 crates.

MIT, Apache-2.0, BSD, and Zlib all require their notices to accompany
binary redistribution. Any published binary artifact must ship a
generated third-party license bundle alongside it.

`deny.toml` holds the license and source policy and CI runs
`cargo deny check advisories bans licenses sources` against it. Still
required before broad distribution: a generated SBOM published with each
artifact.

## Fetched at need: the fast-boot guest kernel

`kernel/build.sh` verifies the Linux tarball against a SHA-256 recorded
per kernel version and pins the Alpine build container by digest. It is
a development convenience for the fast-boot experiment, not part of the
VMM build, and its output is gitignored.
