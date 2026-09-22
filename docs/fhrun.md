# fhrun

`fhrun` runs a Linux binary like a host process, inside a firehyve
microVM. It builds an initramfs with `/init` (the `fhrun-init`
program), the payload at `/app`, and a JSON spec at
`/firehyve-spec.json`. Then it starts the VMM and waits for it.

fhrun does not parse every value. `memory`, for example, goes to the
VMM as given, so `--check` does not find a bad size. A misspelled
manifest key is ignored.

## Commands

```
fhrun <manifest.json>                        boot it
fhrun --check <manifest.json>                validate only
fhrun --print-argv <manifest.json>           print the VMM program and argv, one per line
fhrun --emit-initramfs <manifest.json> <out.cpio>
```

## Manifest

| key | required | default | meaning |
|---|---|---|---|
| `name` | yes | | VM name, 1..=127 chars, no NUL |
| `bin` | yes | | host path to the payload binary |
| `kernel` | yes | | host path to the kernel; the boot protocol is read from the file |
| `init` | yes | | host path to the static `fhrun-init` |
| `args` | no | `[]` | argv[1..]; argv[0] is `bin`'s basename |
| `env` | no | `{}` | environment for the payload |
| `workdir` | no | `/` | working directory in the guest |
| `vcpus` | no | `1` | vCPU count |
| `memory` | no | `"128M"` | memory size, parsed by the VMM |
| `extra_files` | no | `{}` | relative in-guest path to host path |
| `net` | no | | one NIC, always `eth0` |
| `nics` | no | `[]` | more NICs, `eth1` and up; 4 in total at most |
| `consoles` | no | `[]` | virtio-console channels; 2 at most |
| `guest_metadata` | no | | opaque JSON forwarded to the guest spec |
| `vmm` | no | `"firehyve"` | VMM binary; the key `firehyve` is an alias |
| `kernel_extra_cmdline` | no | `""` | appended to the fhrun cmdline |

### NICs

Each NIC in `net` or `nics` has these keys:

| key | required | meaning |
|---|---|---|
| `vnic` | yes | host VNIC name |
| `mac` | yes | `xx:xx:xx:xx:xx:xx`. Checked for form only: the VNIC sets the address |
| `ip` | yes | guest IPv4 address in CIDR form |
| `gateway` | no | default gateway. Only the first NIC with one sets the default route |
| `role` | no | an opaque label |

### consoles

Each entry is a generic virtio-console channel with no protocol
semantics:

| key | default |
|---|---|
| `socket_path` | `<runtime-dir>/console<N>.sock` |
| `guest_device` | `/dev/hvc<N>` |
| `role` | none; an opaque label |

`socket_path` is host-side and is stripped before the spec reaches the
guest. `guest_device` and `role` are forwarded. A caller that speaks its
own protocol over a channel tags it with `role` and puts its payload in
`guest_metadata`; fhrun forwards both without reading either.

There is no implicit console. An empty or absent `consoles` list means
the VM gets no virtio-console device.

## Emitted command line

```
-c <vcpus> -m <memory> --kernel <kernel> --initrd <initramfs>
--cmdline <cmdline> --no-track-dirty -l com1,stdio
-s <7+i>,virtio-net-viona,<vnic>       one per NIC, slots 7..=10
-s <15+i>,virtio-console,<socket>      one per console, slots 15..=16
<name>
```

`<cmdline>` is `console=ttyS0 earlyprintk=ttyS0 root=/dev/ram0
init=/init rdinit=/init panic=-1 fhrun=1 tsc=reliable`, then
`kernel_extra_cmdline`. A manifest can add to it and cannot remove from
it.

The console device config is a bare socket path. A `key=` prefix would
become part of the path the device binds.

`--no-track-dirty` is unconditional: an fhrun VM never migrates, so it
should not pay for a dirty bitmap.

## Exit codes

fhrun exits with the payload's status, as a shell does:

| code | meaning |
|---|---|
| `0..=255` | the exit code of the payload |
| `128 + n` | the payload died from signal `n` |
| `1` | fhrun failed: a bad manifest, a VMM that did not start, or a VMM that stopped before init reported the payload |
| `2` | no arguments |

`fhrun-init` stays PID 1. It forks the payload, reaps it and every
orphan, and writes progress lines to `/dev/ttyS1` (COM2). The last line
is the result:

```
fhrun-init: payload exit <code>
fhrun-init: payload signal <n>
```

firehyve copies COM2 to its stdout. fhrun reads that pipe, copies every
byte to its own stdout, and keeps the last result line. Then init powers
the VM off. The last line wins because init writes it after the payload
has gone. A payload that prints a result line of its own can only set
its own exit status, which it can do anyway.

If no result line arrives, fhrun exits `1` and reports the VMM exit on
stderr. The VMM exit code does not pass through. firehyve exits `0` on a
guest reset (a kernel panic under `panic=-1`), `1` on a poweroff, `2` on
a halt, `3` on a triple fault and `4` on a guest fault. None of these
means that the payload succeeded.

## Signals

fhrun sends SIGINT and SIGTERM to the VMM as SIGTERM. The VMM then stops
the VM at once with `VM_SUSPEND_POWEROFF`: the guest is not asked to
shut down, and no result line is written, so fhrun exits `1`. fhrun
removes its temporary directory. It blocks both signals from before it
starts the VMM until it knows the VMM pid, so a Ctrl-C in that window
is not lost.

SIGKILL has no such path. fhrun dies without removing
`$TMPDIR/fhrun-*/initramfs.cpio`, which holds the manifest `env`, and
the VMM keeps running: illumos has no parent-death signal and fhrun
records the child pid nowhere a sweeper could read it.

## fhrun-init

`tools/fhrun-init` is a separate Cargo workspace with its own lockfile,
named in the root manifest's `exclude` list. It cross-compiles to
`x86_64-unknown-linux-musl`:

```sh
rustup target add x86_64-unknown-linux-musl
cd tools/fhrun-init
cargo build --release --target x86_64-unknown-linux-musl
```

The resulting binary is what a manifest's `init` field points at. CI
formats, lints and tests it in the `guest init (musl)` job, because no
root-workspace job reaches an excluded package.

Its `GuestSpec` declares only the fields that it uses. It ignores the
other fields, `metadata` included. The payload receives only its argv
and environment. A payload that needs `guest_metadata` must read
`/firehyve-spec.json` itself. The test `guest_spec_json_matches_init_contract` in
`tools/fhrun/src/manifest.rs` holds the key names on the host side,
because a rename cannot fail at compile time across the workspace
boundary.
