# Metadata agent (mdata)

rshyve has a built-in SmartOS metadata agent for tests and standalone
use. It runs on COM2 only when the command line gives no `-l com2`
backend. firehyve has no metadata agent.

On a SmartOS node the zone brand passes `-l com2,socket,<path>`. rshyve
then listens on that Unix socket (mode 0600), the metadata agent in the
global zone connects to it, and the built-in agent does not run. A bare
`-l com2,<path>` opens a character device, not a socket.

## Protocol

The agent implements the Joyent metadata protocol, version 2, with a
version 1 fallback:

1. The guest opens `/dev/ttyS1` (Linux) or `/dev/term/b` (SmartOS).
2. The guest sends `\n`. The agent answers `invalid command\n`.
3. The guest sends `NEGOTIATE V2\n`. The agent answers `V2_OK\n`.
4. Each later request is a V2 frame.

```
V2 <body_len> <crc32_hex> <reqid> <command> [<base64_arg>]\n
```

- The body is everything after the CRC field: `<reqid> <command>
  [<base64_arg>]`. `body_len` is its length in bytes.
- `crc32_hex` is the CRC32 of the body.
- `reqid` is 8 hex digits.

The agent checks the length, the CRC and the request ID. A frame that
fails a check gets `FAILURE` with request ID `00000000`.

| Command | V2 | V1 | Result |
|---------|:--:|:--:|--------|
| `GET <key>` | yes | yes | The value of the key |
| `KEYS` | yes | yes | All key names |
| `PUT`, `DELETE` | yes | no | `FAILURE`: the store is read-only |

Lines longer than 4096 bytes are refused.

## Keys

| Key | Source |
|-----|--------|
| `sdc:hostname` | the VM name (positional argument) |
| `sdc:uuid` | `-U`, or the VM name if `-U` is absent |
| `sdc:nics` | `--mdata-nics` (JSON) |
| `sdc:resolvers` | `--mdata-resolvers` (JSON array). Absent when the flag is absent |
| `root_authorized_keys` | `--mdata-ssh-keys` |
| `root_pw` | `--mdata-root-pw-file`, or `--mdata-root-pw` |
| `sdc:maintain_resolvers` | always `true` |
| `sdc:routes` | always `[]` |
| `sdc:dns_domain`, `user-script`, `user-data`, `sdc:vendor-data`, `sdc:operator-script` | always empty |

Use `--mdata-root-pw-file`. Every process that can list processes can
read a `--mdata-root-pw` value from the process arguments.

## NIC JSON format

`--mdata-nics` takes a JSON array in the SmartOS `sdc:nics` form. The
cloud-init SmartOS datasource reads the `ips` and `gateways` arrays:

```json
[{
  "interface": "net0",
  "mac": "02:08:20:aa:bb:cc",
  "ips": ["192.0.2.5/24"],
  "gateways": ["192.0.2.1"],
  "primary": true
}]
```

## Guest compatibility

- **cloud-init SmartOS datasource.** It finds SmartOS through the SMBIOS
  product name `SmartDC*`, negotiates V2, and reads the `sdc:*` keys.
- **mdata-client**, the C client from smartos-live and the Rust client
  from monitor-reef, over the serial port.

cloud-init's `ds-identify` checks the DMI product name for `SmartDC*`.
The VMM sets the SMBIOS Type 1 product to `SmartDC HVM` and the
manufacturer to `Joyent` by default. `-B` changes them.

## UART notes

The UART FIFOs are 256 bytes, larger than the 16 bytes of a real
16550A, so that a whole V2 frame fits. The agent paces its reply so
that it does not overflow the receive FIFO while the guest drains it.

The agent drops bytes outside printable ASCII (0x20 to 0x7E) and ignores
`\r`. UEFI firmware writes escape sequences to COM2 while it probes the
port at boot.
