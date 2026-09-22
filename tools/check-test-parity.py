#!/usr/bin/env python3
"""Compare the tests CI compiles with the tests illumos compiles.

The product platform is illumos and no CI runner is illumos, so a test
behind `cfg(target_os = "illumos")` runs on no machine in CI, and a test
behind `cfg(not(target_os = "illumos"))` never runs on the product
platform. Both mistakes are invisible: the gate compiles, the suite is
green, and the test count is the only thing that moved. This script
counts both sides instead of reasoning about the gates.

How a test list is obtained for a target that cannot be executed.
`cargo test -- --list` needs a binary that runs, so it answers for the
runner only. `--emit=mir` does not link and does not run, and the test
harness rustc builds puts every test name in the MIR as
`StaticTestName(const "...")`. So `cargo check --tests` with that emit
gives the compiler's own answer for any target, illumos included.

Why that answer can be trusted. The MIR reader is checked against ground
truth on every run: the script builds and lists the tests for the
machine it runs on, then asserts the MIR reader returns exactly the same
names for that target. On a Linux runner the checked side IS the CI side
of the comparison. On the illumos build host the checked side is the
illumos side. A reader that stops working fails the run.

What this does NOT prove:

  * That an illumos-only test passes. It is compiled here, never run.
    Run this script on the build host to list what illumos really has.
  * Anything about doc tests. `--list` does not report them and the test
    harness MIR does not hold them.
  * Anything about the packages in UNREACHABLE below. Their C-compiling
    dependencies stop the illumos check before rustc sees any Rust.

usage:
  tools/check-test-parity.py                    compare, check the file
  tools/check-test-parity.py --write-expected   record the delta as it is
  tools/check-test-parity.py --check-reader     check the reader and stop

Run the last form on the illumos build host. It compares the MIR reader
with the illumos test harness itself, which is the one check no runner
can do. Run it after a toolchain upgrade.

A macOS host cannot read the two targets this compares. usdt builds its
probe macro for the host, and on a macOS host the macro then writes
macOS assembly into every crate that has a probe. The comparison needs
a Linux or illumos machine.
"""

from __future__ import annotations

import argparse
import glob
import json
import os
import re
import subprocess
import sys
from pathlib import Path

# The target CI runs the test suite on. The comparison is against this
# and not against the machine the script runs on, so the answer does not
# change with the developer's laptop.
CI_TARGET = "x86_64-unknown-linux-gnu"
ILLUMOS_TARGET = "x86_64-unknown-illumos"

# Packages the illumos check cannot reach from a runner that has no
# illumos C toolchain. cargo runs a build script before rustc sees any
# Rust, so a dependency that compiles C for the target stops the check.
# Their illumos code is compiled on the build host and nowhere else.
# A package that leaves this set must join the comparison, and a new
# package must do one or the other, so the lists are asserted below.
UNREACHABLE = {
    "vmm_migrate": "zstd-sys compiles C for the target",
    "rshyve": "zstd-sys and the vendored libtpms compile C for the target",
    "vmm_tpm": "the vendored libtpms needs an illumos host",
    "vmm_tpm_sys": "the vendored libtpms needs an illumos host",
}

# rustc writes one of these for every `#[test]` it keeps after cfg.
TEST_NAME_RE = re.compile(r'StaticTestName\(const "([^"\\]*)"\)')

# A name computed at run time. The harness supports it, this reader
# cannot see through it, so it is refused rather than under-counted.
DYN_NAME = "DynTestName"

# `deps/libfoo-0123456789abcdef.rmeta` and `deps/foo-0123456789abcdef`
# both end in the unit hash, which names the MIR file beside them.
UNIT_HASH_RE = re.compile(r"-([0-9a-f]{8,32})$")
DROPPED_SUFFIXES = (".rmeta", ".rlib", ".d", ".exe")

EXPECTED_HEADER = """\
# Tests that only one target compiles.
#
# Written by tools/check-test-parity.py, checked by the same script in
# CI. Every line is a test that runs on one platform and not the other,
# so every line is a deliberate decision that somebody has to make:
#
#   illumos-only  the CI suite never runs it. Only the build host can.
#   ci-only       the product platform never runs it.
#
# Add a line here when you mean it, and say why above it. If a test
# appears here that you did not intend to gate, fix the gate instead.
#
# Columns: side, package:kind:target, test name.
"""

# --write-expected keeps every other line of the file, so a reason
# written above a line survives. New lines land under this one.
NEW_MARKER = "# Recorded by --write-expected. Say why, then move it up."


def die(message: str) -> None:
    print(f"check-test-parity: {message}", file=sys.stderr)
    raise SystemExit(1)


def run(argv: list[str], **kwargs) -> subprocess.CompletedProcess:
    return subprocess.run(argv, check=True, text=True, **kwargs)


def host_triple() -> str:
    out = run(["rustc", "-vV"], stdout=subprocess.PIPE).stdout
    for line in out.splitlines():
        if line.startswith("host: "):
            return line[len("host: ") :].strip()
    die("rustc -vV printed no host triple")
    raise AssertionError  # unreachable, keeps the type checker honest


def workspace_packages(root: Path) -> dict[str, str]:
    """Package id to package name, for every workspace member.

    cargo leaves the name out of the id when the directory already
    carries it, so the id cannot be split for a name. This map is the
    only reliable way from an artifact back to its package.
    """
    out = run(
        ["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"],
        cwd=root,
        stdout=subprocess.PIPE,
    ).stdout
    return {p["id"]: p["name"] for p in json.loads(out)["packages"]}


def compared_packages(root: Path) -> list[str]:
    """Every workspace package the illumos check can reach.

    A new package joins the comparison with no edit here. A package that
    cannot be reached must say so in UNREACHABLE, and a name in
    UNREACHABLE that no longer exists is an error, so neither list can
    rot without a failure.
    """
    members = set(workspace_packages(root).values())
    stale = sorted(set(UNREACHABLE) - members)
    if stale:
        die(f"UNREACHABLE names packages the workspace does not have: {stale}")
    return sorted(members - set(UNREACHABLE))


def unit_key(artifact: dict, names: dict[str, str]) -> str:
    target = artifact["target"]
    package = names.get(artifact["package_id"], artifact["package_id"])
    return f"{package}:{','.join(target['kind'])}:{target['name']}"


def unit_mir(artifact: dict, unit: str) -> Path:
    """The MIR dump rustc wrote for this compilation unit."""
    paths = list(artifact.get("filenames") or [])
    if artifact.get("executable"):
        paths.append(artifact["executable"])
    for raw in paths:
        path = Path(raw)
        stem = path.name
        for suffix in DROPPED_SUFFIXES:
            if stem.endswith(suffix):
                stem = stem[: -len(suffix)]
                break
        match = UNIT_HASH_RE.search(stem)
        if not match:
            continue
        found = glob.glob(str(path.parent / f"*-{match.group(1)}.mir"))
        if len(found) > 1:
            die(f"{unit}: several MIR dumps: {sorted(found)}")
        if found:
            return Path(found[0])
    die(
        f"{unit}: no MIR dump beside {paths}. "
        "rustc did not honour --emit=mir, or the target directory is stale. "
        "Delete the target directory and run again."
    )
    raise AssertionError  # unreachable


def read_mir_tests(path: Path, unit: str) -> list[str]:
    text = path.read_text(encoding="utf-8", errors="replace")
    if DYN_NAME in text:
        die(
            f"{unit}: {path.name} holds a {DYN_NAME}. This reader counts only "
            "names fixed at compile time, so the count would be short. Teach "
            "the reader about the macro that made it before trusting a run."
        )
    return sorted(TEST_NAME_RE.findall(text))


def is_test_unit(artifact: dict) -> bool:
    """A unit rustc built with --test, from a target the suite runs.

    Benches and examples are left out on both sides so the two targets
    are compared over the same set of units.
    """
    if not artifact.get("profile", {}).get("test"):
        return False
    kinds = set(artifact["target"]["kind"])
    return bool(kinds & {"lib", "bin", "test"})


def cargo_inventory(
    root: Path,
    target: str,
    packages: list[str],
    names: dict[str, str],
    target_dir: Path,
    link: bool,
) -> tuple[dict[str, list[str]], dict[str, str]]:
    """Compile the test harnesses for `target` and read their MIR.

    With `link` the test binaries are also produced, so the caller can
    ask them for the ground truth list. Without it the units stop at
    metadata, which is what makes a target nobody can execute readable.
    """
    emit = "link,dep-info,mir" if link else "metadata,mir"
    argv = ["cargo"]
    # `--tests` and not `--all-targets`: it selects the same units as
    # `cargo test`, so the two sides compare like with like. A lib with
    # `test = false` and the default `bench = true` is built with --test
    # by --all-targets and not by cargo test, and that one asymmetry
    # would read as every test in the crate appearing on one side only.
    argv += ["test", "--no-run"] if link else ["check", "--tests"]
    argv += ["--locked", "--target", target, "--message-format", "json"]
    for name in packages:
        argv += ["-p", name]

    env = dict(os.environ)
    env["RUSTFLAGS"] = f"--emit={emit}"
    env["CARGO_TARGET_DIR"] = str(target_dir)
    # Settings that would win over the two above and leave the run
    # reading a directory it did not write, or no MIR at all.
    for name in ("CARGO_ENCODED_RUSTFLAGS", "CARGO_BUILD_RUSTFLAGS",
                 "CARGO_BUILD_TARGET_DIR", "CARGO_BUILD_TARGET"):
        env.pop(name, None)

    proc = subprocess.run(
        argv, cwd=root, env=env, text=True, stdout=subprocess.PIPE
    )
    if proc.returncode != 0:
        die(f"{' '.join(argv)} failed with status {proc.returncode}")

    wanted = set(packages)
    tests: dict[str, list[str]] = {}
    binaries: dict[str, str] = {}
    for line in proc.stdout.splitlines():
        if not line.startswith("{"):
            continue
        artifact = json.loads(line)
        if artifact.get("reason") != "compiler-artifact":
            continue
        if not is_test_unit(artifact):
            continue
        # Only a package named on the command line is built with --test,
        # so a unit that lands here and is not one of them means the
        # reader has lost track. Skipping it would drop its tests from
        # both counts and the delta would still look right.
        unit = unit_key(artifact, names)
        package = unit.split(":", 1)[0]
        if package not in wanted:
            die(f"{target}: unit {unit} is from no package this run asked for")
        tests[unit] = read_mir_tests(unit_mir(artifact, unit), unit)
        if artifact.get("executable"):
            binaries[unit] = artifact["executable"]
    if not tests:
        die(f"no test units found for {target}. The package list is wrong.")
    return tests, binaries


def listed_tests(binary: str) -> list[str]:
    out = run(
        [binary, "--list", "--format", "terse"], stdout=subprocess.PIPE
    ).stdout
    return sorted(
        line[: -len(": test")] for line in out.splitlines() if line.endswith(": test")
    )


def check_reader(tests: dict[str, list[str]], binaries: dict[str, str]) -> None:
    """Assert the MIR reader agrees with the test harness itself."""
    missing = sorted(set(tests) - set(binaries))
    if missing:
        die(f"no test binary was built for {missing}")
    bad = []
    for unit, names in sorted(tests.items()):
        truth = listed_tests(binaries[unit])
        if truth != names:
            bad.append(
                (unit, sorted(set(truth) - set(names)), sorted(set(names) - set(truth)))
            )
    if bad:
        for unit, only_truth, only_mir in bad:
            print(f"  {unit}", file=sys.stderr)
            for name in only_truth:
                print(f"    the harness lists {name}, the MIR does not",
                      file=sys.stderr)
            for name in only_mir:
                print(f"    the MIR holds {name}, the harness does not",
                      file=sys.stderr)
        die(
            "the MIR reader disagrees with the test harness. Until it agrees "
            "its answer for illumos means nothing, so this run proves nothing."
        )


def flatten(tests: dict[str, list[str]]) -> set[tuple[str, str]]:
    return {(unit, name) for unit, names in tests.items() for name in names}


def parse_entry(raw: str) -> tuple[str, str, str] | None:
    """The recorded delta on one line, or None for a comment or a blank."""
    line = raw.split("#", 1)[0].strip()
    if not line:
        return None
    fields = line.split()
    if len(fields) != 3 or fields[0] not in ("illumos-only", "ci-only"):
        return ("", "", "")  # a shape the caller reports as an error
    return (fields[0], fields[1], fields[2])


def read_expected(path: Path) -> tuple[list[str], set[tuple[str, str, str]]]:
    if not path.exists():
        die(f"{path} is missing. Run with --write-expected to record it.")
    lines = path.read_text(encoding="utf-8").splitlines()
    entries = set()
    for number, raw in enumerate(lines, 1):
        entry = parse_entry(raw)
        if entry is None:
            continue
        if entry == ("", "", ""):
            die(f"{path}:{number}: expected 'illumos-only|ci-only unit name'")
        entries.add(entry)
    return lines, entries


def write_expected(path: Path, found: set[tuple[str, str, str]]) -> tuple[int, int]:
    """Record the delta, keeping every line a person wrote.

    A regenerated file would drop the reason somebody left above a line,
    and the reason is the point of the file. So lines that still hold are
    kept exactly as they are, lines that no longer hold go, and the rest
    are appended for somebody to explain.
    """
    kept: list[str] = []
    have: set[tuple[str, str, str]] = set()
    dropped = 0
    if path.exists():
        for raw in path.read_text(encoding="utf-8").splitlines():
            entry = parse_entry(raw)
            if entry is None:
                if raw.strip() != NEW_MARKER:
                    kept.append(raw)
                continue
            if entry in found:
                kept.append(raw)
                have.add(entry)
            else:
                dropped += 1
    else:
        kept = EXPECTED_HEADER.splitlines()

    added = sorted(found - have)
    if added:
        kept.append("")
        kept.append(NEW_MARKER)
        kept += [f"{side:<12}  {unit}  {name}" for side, unit, name in added]
    while kept and not kept[-1].strip():
        kept.pop()
    path.write_text("\n".join(kept) + "\n", encoding="utf-8")
    return len(added), dropped


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--write-expected",
        action="store_true",
        help="record the delta that is there now instead of checking it",
    )
    parser.add_argument(
        "--expected",
        default="tools/test-parity.expected",
        help="the recorded delta, relative to the repository root",
    )
    parser.add_argument(
        "--target-dir",
        default=None,
        help="build directory to use (default: target/test-parity)",
    )
    parser.add_argument(
        "--ci-target",
        default=CI_TARGET,
        help="the target CI runs the suite on (default: %(default)s)",
    )
    parser.add_argument(
        "--illumos-target",
        default=ILLUMOS_TARGET,
        help="the product target (default: %(default)s)",
    )
    parser.add_argument(
        "--check-reader",
        action="store_true",
        help="check the MIR reader against this machine and stop, comparing nothing",
    )
    args = parser.parse_args()
    if args.ci_target == args.illumos_target:
        die("--ci-target and --illumos-target name the same target")

    root = Path(__file__).resolve().parent.parent
    if not (root / "Cargo.toml").is_file():
        die(f"no Cargo.toml at {root}")
    if args.target_dir:
        target_dir = Path(args.target_dir).resolve()
    else:
        target_dir = root / "target" / "test-parity"

    names = workspace_packages(root)
    packages = compared_packages(root)
    native = host_triple()
    print(f"packages compared: {len(packages)}")
    for name, why in sorted(UNREACHABLE.items()):
        print(f"  not compared: {name} ({why})")

    # The ground truth run doubles as one side of the comparison when
    # the machine is the machine CI uses.
    print(f"building and listing the tests for {native}")
    native_tests, binaries = cargo_inventory(
        root, native, packages, names, target_dir, link=True
    )
    check_reader(native_tests, binaries)
    counted = sum(len(found) for found in native_tests.values())
    print(
        f"  the MIR reader agrees with the harness on all {len(native_tests)} "
        f"units, {counted} tests"
    )
    if args.check_reader:
        print("check-test-parity: reader checked. No targets were compared.")
        return 0

    inventories = {native: native_tests}
    for target in (args.ci_target, args.illumos_target):
        if target in inventories:
            continue
        print(f"reading the tests {target} compiles")
        inventories[target], _ = cargo_inventory(
            root, target, packages, names, target_dir, link=False
        )

    # A unit is a compilation, not a cfg, so both targets must have the
    # same ones. If one side is short, its tests would read as absent
    # and the delta would name them all.
    ci_units = set(inventories[args.ci_target])
    illumos_units = set(inventories[args.illumos_target])
    if ci_units != illumos_units:
        die(
            "the two targets built different units: "
            f"{sorted(ci_units ^ illumos_units)}"
        )

    ci = flatten(inventories[args.ci_target])
    illumos = flatten(inventories[args.illumos_target])
    print(f"{args.ci_target}: {len(ci)} tests in {len(ci_units)} units")
    print(f"{args.illumos_target}: {len(illumos)} tests in {len(illumos_units)} units")
    if native not in (args.ci_target, args.illumos_target):
        print(
            f"note: the reader was checked against {native}. Neither side of "
            "this comparison was listed by a real harness on this machine."
        )

    found = {("ci-only", unit, name) for unit, name in ci - illumos}
    found |= {("illumos-only", unit, name) for unit, name in illumos - ci}
    only_illumos = sum(1 for side, _, _ in found if side == "illumos-only")
    print(
        f"delta: {len(found)} tests, of which {only_illumos} compile only on "
        "illumos and so run on no CI machine"
    )

    expected_path = root / args.expected
    if args.write_expected:
        added, dropped = write_expected(expected_path, found)
        print(f"{expected_path}: {added} lines added, {dropped} removed")
        return 0

    _, expected = read_expected(expected_path)
    new = sorted(found - expected)
    gone = sorted(expected - found)
    if not new and not gone:
        print("check-test-parity: the delta matches the recorded one")
        return 0

    for side, unit, name in new:
        print(f"  now {side}, not recorded: {unit} {name}", file=sys.stderr)
    for side, unit, name in gone:
        print(f"  recorded {side}, no longer: {unit} {name}", file=sys.stderr)
    print(
        f"\ncheck-test-parity: the set of platform-gated tests moved by "
        f"{len(new) + len(gone)} lines.\n"
        "A test that only illumos compiles runs on no CI machine. A test\n"
        "that illumos does not compile never runs on the product platform.\n"
        "Decide which you meant, then either fix the cfg or run\n"
        f"  tools/check-test-parity.py --write-expected\n"
        f"and commit {args.expected} with the reason in a comment.",
        file=sys.stderr,
    )
    return 1


if __name__ == "__main__":
    sys.exit(main())
