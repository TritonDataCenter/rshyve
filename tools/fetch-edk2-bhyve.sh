#!/bin/sh

set -eu

DEST=${1:-/opt/fw}

PKG_URL='https://pkg.freebsd.org/FreeBSD:14:amd64/latest/All/Hashed/edk2-bhyve-g202508_2%7E2%24edif9c4h.pkg'
PKG_SHA256='c6f7510e483b5db6e3f5d8a9f6e8384b2088a3fe528e0e3c21f64e2d6342e11b'
CODE_SHA256='98a24cc7f8d436c5212e9800ccaedc2ca7411fca8b78dfafab0ca37b0a3de266'
CODE_SIZE=3653632
VARS_SHA256='5d2ac383371b408398accee7ec27c8c09ea5b74a0de0ceea6513388b15be5d1e'
VARS_SIZE=540672

WORK=$(mktemp -d "${TMPDIR:-/var/tmp}/fetch-edk2-bhyve.XXXXXX")
STAGE=

cleanup()
{
	rm -rf "$WORK"
	if [ -n "$STAGE" ]; then
		rm -rf "$STAGE"
	fi
}
trap cleanup 0 1 2 3 15

verify_sha256()
{
	label=$1
	path=$2
	expected_sha256=$3
	actual_sha256=$(digest -a sha256 "$path")

	if [ "$actual_sha256" != "$expected_sha256" ]; then
		printf '%s SHA-256 mismatch: expected %s, got %s\n' \
		    "$label" "$expected_sha256" "$actual_sha256" >&2
		exit 1
	fi
}

verify_size()
{
	label=$1
	path=$2
	expected_size=$3
	actual_size=$(wc -c < "$path" | tr -d '[:space:]')

	if [ "$actual_size" -ne "$expected_size" ]; then
		printf '%s size mismatch: expected %s, got %s\n' \
		    "$label" "$expected_size" "$actual_size" >&2
		exit 1
	fi
}

PACKAGE="$WORK/edk2-bhyve.pkg"
EXTRACT="$WORK/extract"
CODE_REL='usr/local/share/edk2-bhyve/BHYVE_UEFI_CODE.fd'
VARS_REL='usr/local/share/edk2-bhyve/BHYVE_UEFI_VARS.fd'

/usr/bin/curl --fail --location --output "$PACKAGE" "$PKG_URL"
verify_sha256 package "$PACKAGE" "$PKG_SHA256"

mkdir "$EXTRACT"
# GNU tar strips leading slashes while extracting, but not while matching
# explicitly requested archive members.
/opt/local/bin/zstd -dqc "$PACKAGE" | \
    /opt/local/bin/tar -xf - -C "$EXTRACT"

CODE="$EXTRACT/$CODE_REL"
VARS="$EXTRACT/$VARS_REL"
verify_sha256 CODE "$CODE" "$CODE_SHA256"
verify_size CODE "$CODE" "$CODE_SIZE"
verify_sha256 VARS "$VARS" "$VARS_SHA256"
verify_size VARS "$VARS" "$VARS_SIZE"

# The shared VARS template must stay distinguishable from the per-VM
# writable copies.
mkdir -p "$DEST"
STAGE=$(mktemp -d "$DEST/.edk2-bhyve.XXXXXX")
cp "$CODE" "$STAGE/BHYVE_UEFI_CODE.fd"
cp "$VARS" "$STAGE/BHYVE_UEFI_VARS.pristine.fd"
chmod 0444 "$STAGE/BHYVE_UEFI_CODE.fd" \
    "$STAGE/BHYVE_UEFI_VARS.pristine.fd"
mv -f "$STAGE/BHYVE_UEFI_CODE.fd" "$DEST/BHYVE_UEFI_CODE.fd"
mv -f "$STAGE/BHYVE_UEFI_VARS.pristine.fd" \
    "$DEST/BHYVE_UEFI_VARS.pristine.fd"
rmdir "$STAGE"
STAGE=

flash_top=4294967296
flash_size=$((CODE_SIZE + VARS_SIZE))
flash_start=$((flash_top - flash_size))
vars_end=$((flash_start + VARS_SIZE - 1))
code_start=$((vars_end + 1))
code_end=$((code_start + CODE_SIZE - 1))

printf 'Installed firmware in %s\n' "$DEST"
printf 'VARS %s bytes: 0x%08X..0x%08X\n' \
    "$VARS_SIZE" "$flash_start" "$vars_end"
printf 'CODE %s bytes: 0x%08X..0x%08X\n' \
    "$CODE_SIZE" "$code_start" "$code_end"
printf 'Flash window %s bytes: 0x%08X..0x%08X\n' \
    "$flash_size" "$flash_start" "$code_end"
