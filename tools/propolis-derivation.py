#!/usr/bin/env python3
"""Measure how much of this tree is derived from Propolis.

Produces the inventory recorded in THIRD_PARTY.md, so the attribution
can be re-checked rather than taken on trust. Run it against a Propolis
checkout:

    tools/propolis-derivation.py ~/src/propolis

Similarity is an ordered sequence ratio over non-trivial lines. Blank
lines, comments, attributes, and anything under 12 characters are
dropped first, because otherwise `}` and common `use` lines make every
Rust file look 30% similar to every other one.

Scores move as upstream moves, so the measured Propolis revision is
printed with the table and must be recorded beside it in THIRD_PARTY.md.
"""

import argparse
import difflib
import os
import subprocess
import sys

# Below this, a match is boilerplate coincidence rather than derivation.
REPORT_THRESHOLD = 0.30
VERBATIM = 0.95
DERIVED = 0.60

# Cheap prefilter before the O(n*m) ordered comparison.
PREFILTER = 0.25


def significant(lines):
    out = []
    for line in lines:
        s = line.strip()
        if not s or s.startswith("//") or s.startswith("#["):
            continue
        if len(s) < 12:
            continue
        out.append(s)
    return out


def upstream_revision(root):
    """The Propolis revision measured, so the table can be reproduced."""
    got = subprocess.run(
        ["git", "-C", root, "rev-parse", "HEAD"],
        capture_output=True,
        text=True,
    )
    if got.returncode != 0:
        return None
    return got.stdout.strip() or None


def load_upstream(root):
    files = {}
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if d not in ("target", ".git")]
        for name in filenames:
            if not name.endswith(".rs"):
                continue
            path = os.path.join(dirpath, name)
            try:
                text = open(path, errors="replace").read()
            except OSError:
                continue
            sig = significant(text.splitlines())
            if sig:
                files[os.path.relpath(path, root)] = sig
    return files


def our_files(rev):
    listing = subprocess.run(
        ["git", "ls-files", "*.rs"], capture_output=True, text=True, check=True
    ).stdout.split()
    for path in listing:
        if path.startswith("third_party/"):
            continue
        show = subprocess.run(
            ["git", "show", f"{rev}:{path}"], capture_output=True, text=True
        )
        if show.returncode == 0:
            yield path, significant(show.stdout.splitlines())


def best_match(mine, upstream):
    best, score = None, 0.0
    mine_set = set(mine)
    for name, theirs in upstream.items():
        overlap = len(mine_set & set(theirs)) / len(mine)
        if overlap < PREFILTER:
            continue
        ratio = difflib.SequenceMatcher(None, mine, theirs, autojunk=False).ratio()
        # A reordered verbatim copy still scores low on ratio, so take
        # whichever measure is higher once overlap is near total.
        combined = max(ratio, overlap if overlap > 0.9 else 0.0)
        if combined > score:
            best, score = name, combined
    return best, score


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("propolis", help="path to a Propolis checkout")
    ap.add_argument("--rev", default="HEAD", help="git revision to measure")
    args = ap.parse_args()

    root = os.path.expanduser(args.propolis)
    if not os.path.isdir(root):
        sys.exit(f"not a directory: {root}")

    upstream = load_upstream(root)
    if not upstream:
        sys.exit(f"no Rust sources found under {root}")

    propolis_rev = upstream_revision(root)

    rows = []
    for path, mine in our_files(args.rev):
        if len(mine) < 20:
            continue
        name, score = best_match(mine, upstream)
        if score >= REPORT_THRESHOLD:
            rows.append((score, path, len(mine), name))
    rows.sort(reverse=True)

    print(f"measured against Propolis {propolis_rev or '(not a git checkout)'}\n")
    print(f"{'score':>5}  {'kind':<9} {'file':<52} {'lines':>5}  upstream")
    for score, path, count, name in rows:
        kind = (
            "verbatim" if score >= VERBATIM
            else "derived" if score >= DERIVED
            else "partial"
        )
        print(f"{score:>5.0%}  {kind:<9} {path:<52} {count:>5}  {name}")

    total = sum(r[2] for r in rows)
    print(
        f"\n{len(rows)} files above {REPORT_THRESHOLD:.0%}, "
        f"{total} significant lines\n"
        f"{sum(1 for r in rows if r[0] >= VERBATIM)} verbatim, "
        f"{sum(1 for r in rows if DERIVED <= r[0] < VERBATIM)} derived, "
        f"{sum(1 for r in rows if r[0] < DERIVED)} partial"
    )


if __name__ == "__main__":
    main()
