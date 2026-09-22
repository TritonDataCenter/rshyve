#!/usr/bin/env python3
"""Require a `// SAFETY:` comment above every `unsafe` block.

This is a ratchet: `tools/safety-budget.txt` records how many
undocumented blocks each crate still has, and the number may only go
down.

    tools/check-safety-comments.py            check against the budget
    tools/check-safety-comments.py --list     print every undocumented block
    tools/check-safety-comments.py --write    rewrite the budget from the tree

`--write` exists so lowering the budget is one command, not hand editing.
It refuses to raise any entry, because that is the check's whole purpose.

What counts as documented: the comment lines immediately above the block,
with no blank line between, hold one starting `SAFETY:` after the `//`.
An `unsafe fn` declaration is not a block and is not counted; its callers
are what need the justification, and each of those is a block.
"""

import argparse
import os
import pathlib
import re
import subprocess
import sys

BUDGET = pathlib.Path("tools/safety-budget.txt")

# `unsafe {` and `unsafe impl`/`unsafe fn` are different things. Only the
# block form opens a scope a comment can justify. `r#unsafe` is not a
# keyword use, and the word inside a string or comment is not either,
# which is why the line is stripped of both first.
BLOCK = re.compile(r"(?<![\w.])unsafe\s*\{")
SAFETY = re.compile(r"^\s*(//|///|//!)\s*SAFETY\b")
COMMENT = re.compile(r"^\s*(//|/\*|\*)")


PRUNE = {".git", "target", "node_modules", "third_party"}


def sources():
    """Every Rust file this tree owns, and how the list was built.

    A checkout answers from the index. An extracted tarball, which is what
    the illumos build host has, is walked instead. A listing that fails
    must never be read as "no files": that is how a gate reports clean on
    a tree it never looked at.
    """
    got = subprocess.run(
        ["git", "ls-files", "*.rs"], capture_output=True, text=True
    )
    if got.returncode == 0 and got.stdout.strip():
        paths = [p for p in got.stdout.split() if not p.startswith("third_party/")]
        return paths, "the git index"

    paths = []
    for dirpath, dirnames, filenames in os.walk("."):
        dirnames[:] = [d for d in dirnames if d not in PRUNE]
        paths += [
            os.path.relpath(os.path.join(dirpath, f), ".")
            for f in filenames
            if f.endswith(".rs")
        ]
    if not paths:
        sys.exit(f"check-safety-comments: no Rust sources under {os.getcwd()}")
    return sorted(paths), "a filesystem walk"


def crate_of(path):
    """The workspace member a file belongs to, as the budget names it."""
    parts = pathlib.PurePosixPath(path).parts
    if parts[0] in ("crates", "bin", "tools") and len(parts) > 1:
        # crates/bhyve-api/sys is its own member.
        if parts[:2] == ("crates", "bhyve-api") and parts[2:3] == ("sys",):
            return "crates/bhyve-api/sys"
        return "/".join(parts[:2])
    return parts[0]


def strip_noise(line):
    """Drop string literals and any trailing comment.

    A `//` inside a string would otherwise hide the rest of the line, and
    the word `unsafe` inside a comment or a message must not be counted.
    """
    out = []
    i = 0
    quote = None
    while i < len(line):
        c = line[i]
        if quote is None:
            if c in ('"', "'"):
                quote = c
                i += 1
                continue
            if line.startswith("//", i):
                break
            out.append(c)
        else:
            if c == "\\":
                i += 2
                continue
            if c == quote:
                quote = None
        i += 1
    return "".join(out)


def undocumented(path):
    """Every unsafe block in `path` with no SAFETY comment above it."""
    lines = pathlib.Path(path).read_text(errors="replace").splitlines()
    hits = []
    for n, line in enumerate(lines):
        if not BLOCK.search(strip_noise(line)):
            continue
        # Walk up over the contiguous comment block directly above.
        j = n - 1
        documented = False
        while j >= 0 and COMMENT.match(lines[j]):
            if SAFETY.match(lines[j]):
                documented = True
                break
            j -= 1
        if not documented:
            hits.append((n + 1, line.strip()))
    return hits


def measure():
    counts = {}
    detail = []
    paths, mode = sources()
    for path in paths:
        hits = undocumented(path)
        if not hits:
            continue
        counts[crate_of(path)] = counts.get(crate_of(path), 0) + len(hits)
        detail.extend((path, n, text) for n, text in hits)
    return counts, detail, mode


def read_budget():
    if not BUDGET.exists():
        sys.exit(f"no budget at {BUDGET}; run with --write to create it")
    budget = {}
    for raw in BUDGET.read_text().splitlines():
        line = raw.split("#", 1)[0].strip()
        if not line:
            continue
        name, _, count = line.rpartition(" ")
        if not name or not count.isdigit():
            sys.exit(f"{BUDGET}: cannot read '{raw}'")
        budget[name.strip()] = int(count)
    return budget


def write_budget(counts, budget, seeding):
    # Seeding records the tree as it is. After that, a rewrite may only
    # lower a number, because raising one is what the check exists to
    # stop.
    if not seeding:
        for name, count in counts.items():
            if count > budget.get(name, 0):
                sys.exit(
                    f"refusing to raise the budget for {name} "
                    f"from {budget.get(name, 0)} to {count}: "
                    "document the new blocks instead"
                )
    body = [
        "# Undocumented `unsafe` blocks per workspace member.",
        "#",
        "# tools/check-safety-comments.py holds these numbers down. A new",
        "# unsafe block needs a `// SAFETY:` comment above it; documenting",
        "# an old one lowers a number here. Nothing may raise one.",
        "#",
        "# Regenerate with: tools/check-safety-comments.py --write",
        "",
    ]
    body += [f"{name} {counts[name]}" for name in sorted(counts)]
    BUDGET.write_text("\n".join(body) + "\n")
    print(f"{BUDGET}: {sum(counts.values())} undocumented blocks in {len(counts)} members")


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--list", action="store_true", help="print every undocumented block")
    ap.add_argument("--write", action="store_true", help="rewrite the budget from the tree")
    args = ap.parse_args()

    if not pathlib.Path("Cargo.toml").is_file():
        sys.exit("run from the repository root")

    counts, detail, mode = measure()

    if args.list:
        for path, n, text in detail:
            print(f"{path}:{n}: {text}")

    if args.write:
        seeding = not BUDGET.exists()
        write_budget(counts, {} if seeding else read_budget(), seeding)
        return

    budget = read_budget()
    status = 0
    for name in sorted(set(counts) | set(budget)):
        have = counts.get(name, 0)
        want = budget.get(name, 0)
        if have > want:
            print(
                f"{name}: {have} undocumented unsafe blocks, budget {want}. "
                f"Add `// SAFETY:` above the new ones.",
                file=sys.stderr,
            )
            status = 1
        elif have < want:
            print(
                f"{name}: {have} undocumented unsafe blocks, budget {want}. "
                "Lower it: tools/check-safety-comments.py --write",
                file=sys.stderr,
            )
            status = 1
    if status == 0:
        print(
            f"check-safety-comments: {sum(counts.values())} undocumented "
            f"unsafe blocks from {mode}, all within budget"
        )
    return status


if __name__ == "__main__":
    sys.exit(main() or 0)
