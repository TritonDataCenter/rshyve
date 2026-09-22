# Contributing

This is a hypervisor. A guest is hostile by assumption, the release
profile is `panic = "abort"`, and any panic reachable from guest input is
a denial of service. Read [SECURITY.md](SECURITY.md) before the first
patch, and [docs/architecture.md](docs/architecture.md) to find your way
around.

## Build and test

Clone with the libtpms submodule:

```bash
git clone --recurse-submodules https://github.com/TritonDataCenter/rshyve
# already cloned:  git submodule update --init third_party/libtpms
```

```bash
cargo build --workspace --locked --release   # on an illumos host
cargo test --workspace --locked --no-fail-fast
```

Pass `--no-fail-fast`. Without it the run stops at the first crate that
fails and the partial output still reads like a completed pass.

Prerequisites: a Rust toolchain at or above the `rust-version` in the
root `Cargo.toml`, autoconf, automake, libtool, pkg-config, gmake, gcc
and OpenSSL development files. A macOS or Linux host needs the same set
with `make` in place of `gmake`. DTrace is not needed to build.

## What runs where

The binaries run a VM only on illumos: `bhyve-api` binds the illumos
ioctls and `viona-api` needs libdladm. The workspace compiles and its
unit tests run on Linux and macOS too, so most work can be done there.

`vmm-tpm-sys` runs a real autotools build of the vendored libtpms on
every host. Do not replace it with a stub build script. A stub hides the
OpenSSL link path, and that path once gave a binary that no platform
image could load ([docs/platform.md](docs/platform.md#openssl-and-the-vtpm)).

CI runs on Linux and cannot boot a guest. It checks formatting, clippy
on every workspace member, the tests in debug and release (the binaries
and the vTPM crates included), an illumos-target type-check of every
crate that can cross-compile, and the repository gates below. It does
not prove that the VMM boots anything.

Two things only a real host proves. Both found guest-visible bugs that a
green test run missed:

- The illumos build host runs `cargo clippy --workspace --all-targets --
  -D warnings` and the full test suite. `rshyve`'s
  `cfg(target_os = "illumos")` bodies are linted there and nowhere else,
  because its C dependencies cannot cross-compile on the CI runner.
- A live guest. [docs/testing.md](docs/testing.md) tells how to boot a
  test VM with each binary and how to run the virtio-fs, hotplug and
  boot-profiling harnesses.

Do not run a harness script on a node that you do not control. The
scripts run as root in the global zone.

## Repository gates

Each one runs in CI and each has a self-test that watches it fail, because
a gate nobody has seen fail is not known to work.

| Gate | What it holds |
|---|---|
| `tools/public-surface.sh` | No private infrastructure detail, workstation path, personal identifier or oversized binary in the tree, and none in the commit messages of the pushed range. Site-specific literals belong in an untracked `.public-surface.local`, never in a tracked file. |
| `tools/check-safety-comments.py` | Every new `unsafe` block carries a `// SAFETY:` comment above it. `tools/safety-budget.txt` records the per-crate count of blocks that do not yet, and it may only go down. Lower it with `--write`. |
| `tools/check-ci-crates.sh` | Every workspace member is named by a clippy job, and every portable crate by the test, MSRV and cross-check jobs. A new crate fails this until a job names it. |
| `tools/check-boundary-bans.sh` | `firehyve` has no dependency path to the parity-only crates. |
| `tools/check-test-parity.py` | illumos and CI compile the same set of tests, apart from the gated ones recorded with a reason. |
| `cargo deny` | Advisories, licenses, sources and duplicate versions, against `deny.toml` and `deny-firehyve.toml`. |

## Code

- Comments explain WHY. A comment that restates the code, argues with
  itself, or narrates a past review is worse than no comment.
- Every value a guest controls is arithmetic on untrusted input: use
  `checked_add`/`checked_mul`, and no indexing that a guest can push out
  of range.
- All guest memory access goes through `PhysMap::lookup()` and
  `SubMapping`, which are volatile and alignment-checked.
- `slog` for logging. No `eprintln!` in a production path.
- No `let _ =` on a path that changes state.
- New `unsafe` needs a `// SAFETY:` comment saying what makes it sound.
- Files stay under about 1000 lines. Split rather than grow.
- `cargo fmt` before committing; `rustfmt.toml` sets 80 columns.

## Tests

A behavioural change gets a test that fails before it and passes after.
Drive a guest-facing path through `PhysMap::new_anon` and the real
register or queue entry points, the way the existing device tests do. Do
not assert on source text.

## Commits and pull requests

One commit per logical change. Imperative subject, 60 characters or less,
no trailing period. A body of one to three lines saying WHY, not WHAT.
Reference a finding id (`OSF-n`) when a commit closes or narrows one.

A pull request should say what a reviewer cannot see from the diff: the
trigger a fix removes, what was measured, and what is still unproven.
