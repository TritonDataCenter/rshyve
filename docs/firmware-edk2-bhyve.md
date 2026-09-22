# FreeBSD edk2-bhyve firmware

The VMM uses the `bhyve` flavor of FreeBSD's edk2 port as its UEFI firmware.
Firmware blobs are fetched during deployment and are not committed to this
repository. Run the pinned fetcher on an illumos host:

```console
# tools/fetch-edk2-bhyve.sh /opt/fw
```

The script installs `BHYVE_UEFI_CODE.fd` without modification and installs the
variable store as `BHYVE_UEFI_VARS.pristine.fd`. The pristine file is a template
for making a private writable copy for each VM. Nothing may map the template
read-write because guest-controlled variable writes would then affect every VM
that uses it.

## Package pin and trust model

The package is fetched from this immutable, content-addressed URL:

```text
https://pkg.freebsd.org/FreeBSD:14:amd64/latest/All/Hashed/edk2-bhyve-g202508_2%7E2%24edif9c4h.pkg
```

`All/<name>.pkg` is a rotating symlink. `All/Hashed/` is keyed by the package's
BLAKE2b sum and preserves the selected package bytes. The fetcher verifies the
following SHA-256 pins, plus the CODE and VARS size pins, before installing
either firmware file:

| Artifact | SHA-256 | Size |
| --- | --- | ---: |
| Package | `c6f7510e483b5db6e3f5d8a9f6e8384b2088a3fe528e0e3c21f64e2d6342e11b` | not separately pinned |
| CODE | `98a24cc7f8d436c5212e9800ccaedc2ca7411fca8b78dfafab0ca37b0a3de266` | 3,653,632 bytes |
| VARS | `5d2ac383371b408398accee7ec27c8c09ea5b74a0de0ceea6513388b15be5d1e` | 540,672 bytes |

This pin is trust-on-first-use. FreeBSD signs `packagesite.yaml`, but the
FreeBSD signing-key fingerprint is not available on illumos. The `.pub` key
shipped by the repository is therefore self-asserted in this environment. We
verify pinned package and firmware bytes, not signatures.

## Provenance and redistribution

| Field | Value |
| --- | --- |
| Package | `edk2-bhyve-g202508_2` |
| License | BSD-3-Clause |
| License logic | `single` |
| License permissions | `dist-mirror dist-sell pkg-mirror pkg-sell auto-accept` |
| Builder | `poudriere-git-3.4.8` |
| Build time | `2026-07-30T08:09:47Z` |
| Ports tree | `ab34388330a3b237e53b906e927989e240177443` |
| Port revision | `d69944f5e177117e1e8b0e3041bd44374f88d10f` |
| Upstream | TianoCore `edk2-stable202508` |
| CPE | `cpe:2.3:a:tianocore:edk2:g202508` |

The FreeBSD port permissions allow redistribution of both the distfile and the
binary package. The full BSD-3-Clause terms are recorded in
[`LICENSES/BSD-3-Clause-edk2.txt`](../LICENSES/BSD-3-Clause-edk2.txt). The
package's `usr/local/share/licenses/edk2-bhyve-g202508_2/BSD3CLAUSE` file is
only a stub directing readers to the standard license text.

## Module-list delta

Compared with the illumos firmware build, the FreeBSD firmware uniquely
contains:

- `Tcg2Dxe`, `Tcg2Pei`, `Tcg2ConfigDxe`, `Tcg2ConfigPei`,
  `Tcg2PlatformDxe`, `Tcg2PlatformPei`, `TcgDxe`, `TcgPei`, and
  `TpmMmioSevDecryptPei`
- `SecureBootConfigDxe`
- `Ip6`, `Udp6`, `Dhcp6`, and `Mtftp6`

The illumos firmware uniquely contains `Shell`, `HttpBootDxe`, `HttpDxe`,
`HttpUtilitiesDxe`, `DnsDxe`, and `tftpDynamicCommand`.

## Flash geometry

The two files occupy exactly 4 MiB at the top of the 32-bit physical address
space:

| Region | Address range | Size |
| --- | --- | ---: |
| VARS | `0xFFC00000..0xFFC83FFF` | 540,672 bytes |
| CODE | `0xFFC84000..0xFFFFFFFF` | 3,653,632 bytes |
| Total | `0xFFC00000..0xFFFFFFFF` | 4,194,304 bytes |

The fetcher computes this window from the pinned file sizes when it runs. A
future firmware version can change the split, so consumers must not infer the
window from a package version alone.

## Use with rshyve

rshyve can boot two UEFI code images:

- `/usr/share/bhyve/uefi-rom.bin`, the illumos bhyve firmware. It
  includes the EFI Internal Shell.
- `/opt/fw/BHYVE_UEFI_CODE.fd`, this FreeBSD firmware. It has the TPM
  and Secure Boot modules and no EFI Internal Shell.

The bootrom grammar is `-l bootrom,<rom>[,<varfile>]`. A varfile is
writable guest state, so give each VM a private copy. rshyve takes an
exclusive lock on the varfile and refuses to start a second VM on it.

To reset the firmware state of a stopped VM, copy the pristine template
and restrict the copy:

```bash
cp /opt/fw/BHYVE_UEFI_VARS.pristine.fd /var/db/vmm/<vm>/VARS.fd
chmod 600 /var/db/vmm/<vm>/VARS.fd
```

This procedure resets all UEFI variables. A BitLocker-protected guest
asks for its recovery key on the next boot.

`VARS.fd` and the vTPM state directory are one unit of VM state. If you
lose one, the other is useless. Snapshots and backups must capture both.

rshyve also accepts a varfile of the correct size that is filled with
`0xFF`. It is erased flash, and the firmware formats it on the first
boot. There is no `--bootvars-reset` flag, on purpose. A guest reset
re-executes rshyve with the same argv, so such a flag would erase the
variables on every reboot.

The EFI Shell `reset` command applies only to the illumos firmware. With
the FreeBSD firmware, use a control socket and send
`{"command":"reset"}`. If you need a shell, put `ShellX64.efi` at
`\EFI\BOOT\BOOTX64.EFI` on a small FAT image and attach it as a second
read-only NVMe namespace. The same image can carry Secure Boot
key-enrollment files.
