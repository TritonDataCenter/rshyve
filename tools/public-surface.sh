#!/usr/bin/env bash
#
# Public-surface guard.
#
# Fails if a file in this tree, or a commit message in a range, carries
# private infrastructure detail, a workstation-specific path, a personal
# identifier, or an unexpectedly large binary. This is a regression
# guard for the scrub done before publication, not a secret scanner:
# gitleaks covers credentials.
#
#   tools/public-surface.sh                    scan the tree
#   tools/public-surface.sh --commits <range>  scan commit messages too
#
# Exit status: 0 clean, 1 a rule found something, 2 the check could not
# run.
#
# The tracked rules are generic: private address ranges, home
# directories, key-name shapes, mail addresses. Site-specific literals
# (a build host address, a storage host name, a zone id) must never be
# spelled in this file, because this file is public. They are read from
# `.public-surface.local` in the tree root (gitignored, one extended
# regex per line, `#` comments) and from the PUBLIC_SURFACE_PATTERNS
# environment variable (same format, newline separated), so a private
# checkout and a CI secret can both carry them.
#
# WHY every command is checked and there is a third exit status: a scan
# that cannot list or read the files must never report clean, and a
# failed `git grep` must never read as "no hits". The file list is built
# once, from the git index in a checkout and from the filesystem
# otherwise, and the list must hold the sentinel files below.
#
# Every exception below is deliberate and carries a reason. Add one only
# when the occurrence is correct, and say why.

set -o errexit
set -o nounset
set -o pipefail

die() {
    echo "public-surface: cannot run: $*" >&2
    exit 2
}

cd "$(dirname "${BASH_SOURCE[0]}")/.." || die "no tree above ${BASH_SOURCE[0]}"

commit_range=""
while (( $# > 0 )); do
    case "$1" in
        --commits)
            (( $# >= 2 )) || die "--commits needs a range"
            commit_range="$2"
            shift 2
            ;;
        *) die "unknown argument: $1" ;;
    esac
done

status=0

# Files that every copy of this tree holds. If the listing misses one,
# it read the wrong directory or read nothing.
SENTINELS=(Cargo.toml tools/public-surface.sh)

# Directories the filesystem walk skips. Build output is not part of the
# published tree and dwarfs it.
PRUNE_DIRS=(.git target node_modules)

# A large tracked binary is almost always an accident, for example a
# GPL-2.0 kernel image whose source is not tracked. Cargo.lock is text
# and legitimately large.
MAX_BYTES=1048576

# Vendored upstream C is excluded: this project does not rewrite it.
# This script is NOT excluded: it carries only generic patterns, and a
# literal added here by mistake must trip the guard like anywhere else.
BASE_EXCLUDES=(
    'third_party/*'
)

LOCAL_PATTERNS=.public-surface.local

# grep in batches, so an unpruned directory in filesystem mode cannot
# push one argument list past ARG_MAX.
BATCH=400

# excluded <path> <pattern>...
excluded() {
    local path="$1" pattern
    shift
    for pattern in "$@"; do
        # Unquoted on the right: these are globs, and `*` spans `/`, so
        # `third_party/*` covers the whole subtree.
        # shellcheck disable=SC2053
        if [[ "$path" == $pattern ]]; then
            return 0
        fi
    done
    return 1
}

# listed <wanted> <path>... An exact match, not a glob.
listed() {
    local want="$1" path
    shift
    for path in "$@"; do
        if [[ "$path" == "$want" ]]; then
            return 0
        fi
    done
    return 1
}

work="$(mktemp -d)" || die "mktemp failed"
trap 'rm -rf "$work"' EXIT

listing="$work/listing"
mode=""

# A checkout answers with its own root. A tarball unpacked inside some
# other repository answers with that repository, whose index does not
# hold these files, so `-ef` decides it rather than the exit status.
in_checkout=no
if top="$(git rev-parse --show-toplevel 2>/dev/null)" && [[ -n "$top" && "$top" -ef . ]]; then
    in_checkout=yes
    git ls-files -z > "$listing" || die "git ls-files failed in a checkout at $top"
    mode="the git index"
else
    prune=()
    for dir in "${PRUNE_DIRS[@]}"; do
        if (( ${#prune[@]} > 0 )); then
            prune+=(-o)
        fi
        prune+=(-name "$dir")
    done
    # Regular files only, and symbolic links are not followed, so
    # nothing here reads a device or a fifo.
    find . \( "${prune[@]}" \) -prune -o -type f -print0 > "$listing" \
        || die "find failed"
    mode="the filesystem"
    echo "public-surface: no checkout here, walking ${PWD} and skipping: ${PRUNE_DIRS[*]}" >&2
fi

if [[ -n "$commit_range" && "$in_checkout" != yes ]]; then
    die "--commits needs a checkout"
fi

all=()
while IFS= read -r -d '' path; do
    all+=("${path#./}")
done < "$listing"
(( ${#all[@]} > 0 )) || die "$mode listed no files"

for want in "${SENTINELS[@]}"; do
    if ! listed "$want" "${all[@]}"; then
        die "$mode holds no $want, so it did not list this tree"
    fi
done

# A tracked file deleted from the working tree, and a submodule, are
# both listed and neither can be read. Say so: a file dropped without a
# word is how a scan quietly stops covering something.
base=()
for path in "${all[@]}"; do
    if excluded "$path" "${BASE_EXCLUDES[@]}"; then
        continue
    fi
    if [[ -f "$path" && -r "$path" ]]; then
        base+=("$path")
    else
        echo "public-surface: not a readable file, skipped: $path" >&2
    fi
done
(( ${#base[@]} > 0 )) || die "every listed file was excluded or unreadable"

# One message per file, named by hash, so a hit names the commit. The
# tree scan cannot see a commit message.
commits=()
if [[ -n "$commit_range" ]]; then
    mkdir -p "$work/commits"
    git rev-list "$commit_range" > "$work/revs" \
        || die "git rev-list $commit_range failed"
    while IFS= read -r sha; do
        git log -1 --format=%B "$sha" > "$work/commits/$sha" \
            || die "git log $sha failed"
        commits+=("$work/commits/$sha")
    done < "$work/revs"
    echo "public-surface: scanning ${#commits[@]} commit message(s) in $commit_range" >&2
fi

# scan <label> <pattern> <ignore-regex> <path>...
# Greps the given files. Hits matching <ignore-regex> (empty for none)
# are dropped. Returns 0 clean, 1 hits, dies on a grep error.
scan() {
    local label="$1" pattern="$2" ignore="$3"
    shift 3
    local -a files=("$@")
    (( ${#files[@]} > 0 )) || die "the exceptions for '$label' leave nothing to scan"

    local raw="$work/raw" hits="$work/hits"
    : > "$raw"
    local -i first=0 rc=0 worst=0
    while (( first < ${#files[@]} )); do
        rc=0
        # /dev/null keeps the file name in the output when a batch holds
        # one file. grep answers 0 for a hit, 1 for none, and more than
        # 1 for a file it could not read or an option it does not have.
        grep -nIE -e "$pattern" -- "${files[@]:first:BATCH}" /dev/null >> "$raw" || rc=$?
        if (( rc > worst )); then
            worst=$rc
        fi
        first=$(( first + BATCH ))
    done
    if (( worst > 1 )); then
        die "grep answered $worst while checking '$label'"
    fi
    if [[ -n "$ignore" ]]; then
        grep -vE -e "$ignore" -- "$raw" > "$hits" || true
    else
        cp "$raw" "$hits"
    fi
    if [[ -s "$hits" ]]; then
        echo "FAIL: $label" >&2
        sed -e "s|^$work/commits/|commit |" -e 's/^/    /' "$hits" >&2
        return 1
    fi
    return 0
}

# report <rule> <pattern> <ignore-regex> [extra-path-exception ...]
report() {
    local rule="$1" pattern="$2" ignore="$3"
    shift 3
    local path
    local -a files=()
    for path in "${base[@]}"; do
        if ! excluded "$path" "$@"; then
            files+=("$path")
        fi
    done
    scan "$rule" "$pattern" "$ignore" "${files[@]}" || status=1
    if (( ${#commits[@]} > 0 )); then
        scan "$rule (commit message)" "$pattern" "$ignore" "${commits[@]}" || status=1
    fi
}

# RFC 1918 ranges, anchored on a full dotted quad so specification
# section numbers in comments do not match. 10.0.0.x is exempt: it is
# the conventional placeholder in the CLI and metadata examples and
# carries no information about anyone's network.
report "private network address" \
    '(^|[^0-9.])(10\.(0\.[1-9][0-9]{0,2}\.[0-9]{1,3}|[1-9][0-9]{0,2}\.[0-9]{1,3}\.[0-9]{1,3})|192\.168\.[0-9]{1,3}\.[0-9]{1,3}|172\.(1[6-9]|2[0-9]|3[01])\.[0-9]{1,3}\.[0-9]{1,3})([^0-9.]|$)' \
    ''

report "personal or workstation-specific path" \
    '/Users/[a-z]|~/works[p]ace|/home/[a-z][a-z0-9_-]*/|/export/hom[e]/' \
    ''

# A named key is a site detail. `id_rsa` alone is the OpenSSH default
# and says nothing.
report "site-specific SSH key name" \
    '[a-z0-9]+\.id_(rsa|ed25519|ecdsa)|id_(rsa|ed25519|ecdsa)-[a-z0-9]+' \
    ''

# LICENSES/: upstream license texts name their own contacts. A noreply
# address is a bot trailer, not a person.
report "personal identifier" \
    '[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+\.(com|io|net|org|dev)' \
    'git@github\.com|@example\.(com|org|net)|noreply@' \
    'LICENSES/*'

report "SSH host key verification disabled" \
    'StrictHostKeyChecking=n[o]|UserKnownHostsFile=/dev/nul[l]' \
    ''

# Site-specific literals, never spelled here. See the header.
site=()
if [[ -f "$LOCAL_PATTERNS" ]]; then
    while IFS= read -r line; do
        [[ -z "$line" || "$line" == \#* ]] && continue
        site+=("$line")
    done < "$LOCAL_PATTERNS"
fi
if [[ -n "${PUBLIC_SURFACE_PATTERNS:-}" ]]; then
    while IFS= read -r line; do
        [[ -z "$line" || "$line" == \#* ]] && continue
        site+=("$line")
    done <<< "$PUBLIC_SURFACE_PATTERNS"
fi
if (( ${#site[@]} > 0 )); then
    echo "public-surface: ${#site[@]} site-specific pattern(s) loaded" >&2
    for pattern in "${site[@]}"; do
        report "site-specific literal" "$pattern" '' "$LOCAL_PATTERNS"
    done
else
    echo "public-surface: no site-specific patterns ($LOCAL_PATTERNS absent, PUBLIC_SURFACE_PATTERNS unset)" >&2
fi

large="$work/large"
: > "$large"
for path in "${base[@]}"; do
    if [[ "$path" == Cargo.lock ]]; then
        continue
    fi
    size="$(wc -c < "$path")" || die "cannot read the size of $path"
    size="${size//[[:space:]]/}"
    if (( size > MAX_BYTES )); then
        printf '%s (%s bytes)\n' "$path" "$size" >> "$large"
    fi
done
if [[ -s "$large" ]]; then
    echo "FAIL: file larger than $MAX_BYTES bytes" >&2
    sed 's/^/    /' "$large" >&2
    status=1
fi

if (( status == 0 )); then
    printf 'public-surface: clean (%d files from %s' "${#base[@]}" "$mode"
    if (( ${#commits[@]} > 0 )); then
        printf ', %d commit messages' "${#commits[@]}"
    fi
    printf ')\n'
fi
exit "$status"
