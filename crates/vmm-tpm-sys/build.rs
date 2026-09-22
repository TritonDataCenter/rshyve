// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Drive the autotools build of libtpms.
//!
//! `third_party/libtpms` is a submodule pinned to pristine upstream. The
//! build copies it into `OUT_DIR`, applies the illumos patches from
//! `third_party/libtpms-illumos/patches`, runs `autogen.sh && configure
//! && gmake` once, and links against the static archives. The submodule
//! is never modified, so the patch set is the complete record of what
//! this project changes.
//!
//! # Which OpenSSL libtpms links against
//!
//! A stock SmartOS platform image ships no `libcrypto.so.3`. Its
//! OpenSSL is `libcrypto-smartos.so.3`, whose exports are almost all
//! renamed with a `sunw_` prefix so that a zone's pkgsrc OpenSSL and
//! the platform's cannot collide in one process. A binary that records
//! `NEEDED libcrypto.so.3` therefore cannot start on a stock node.
//!
//! On SmartOS, libtpms links against the platform library the same way
//! every other platform binary does: force-include a generated
//! `#pragma redefine_extname` header so plain `EVP_*`/`AES_*` calls
//! bind to the `sunw_`-prefixed names, and resolve `-lcrypto` through
//! a link farm whose one entry points at the platform library. `ld`
//! records that library's SONAME, so the binary resolves crypto from
//! base `/lib` with no `LD_LIBRARY_PATH` and no pkgsrc.
//!
//! Anywhere else (a Linux CI runner, a dev host) pkg-config still
//! decides, unchanged.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

include!("src/platform_flags.rs");

/// The platform OpenSSL shipped in a SmartOS platform image.
const SMARTOS_CRYPTO: &str = "/lib/amd64/libcrypto-smartos.so.3";

/// How libtpms reaches OpenSSL.
enum Crypto {
    /// SmartOS platform OpenSSL, reached through a generated symbol
    /// prefix header and a link farm.
    SmartOs { header: PathBuf, linkfarm: PathBuf },
    /// Whatever pkg-config points at.
    PkgConfig,
}

fn main() {
    let workspace_root = workspace_root();
    let src = workspace_root.join("third_party").join("libtpms");
    assert!(
        src.join("configure.ac").is_file(),
        "libtpms submodule is empty at {}. Run:\n    \
         git submodule update --init third_party/libtpms",
        src.display()
    );
    // Without this check the failure comes from automake as
    // "'pkgconfig_DATA' is used but 'pkgconfigdir' is undefined", which
    // names neither pkg-config nor OpenSSL.
    assert!(
        !pkg_config(&["--version"]).is_empty(),
        "pkg-config is missing. libtpms needs it twice: autoreconf \
         reads its pkg.m4 for the PKG_INSTALLDIR macro, and this build \
         script asks it where OpenSSL is. See docs/testing.md."
    );
    let patches = workspace_root
        .join("third_party")
        .join("libtpms-illumos")
        .join("patches");

    let out_dir =
        PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR set by cargo"));
    // Copy the vendored source into OUT_DIR so autoreconf can write
    // generated files (configure, Makefile.in, …) without polluting
    // the checked-in tree.
    let work_src = out_dir.join("libtpms-src");
    let build_dir = out_dir.join("libtpms-build");

    println!("cargo:rerun-if-changed={}", src.display());
    println!("cargo:rerun-if-changed={}", patches.display());
    println!("cargo:rerun-if-changed={SMARTOS_CRYPTO}");
    // Named because this file `include!`s it. A rerun-if-changed line
    // anywhere turns off cargo's own "any file in the package" rule.
    println!("cargo:rerun-if-changed=src/platform_flags.rs");

    let crypto = select_crypto(&out_dir);
    let target_os = env::var("CARGO_CFG_TARGET_OS")
        .expect("CARGO_CFG_TARGET_OS set by cargo");
    let cflags = base_cflags(&target_os);

    // The stamp records how the archive in OUT_DIR was built. OUT_DIR
    // survives a change to this file, so a bare "already built" marker
    // would keep handing back an archive linked against the wrong
    // library, or built with options this file no longer passes.
    let marker = build_dir.join(".vmm-tpm-built.stamp");
    // The patches are part of what was built: without them here, a new
    // patch reruns this script but leaves the old archive in place.
    let fingerprint = format!(
        "{}\ncflags {cflags}\nconfigure {}\npatches\n{}",
        crypto.fingerprint(),
        CONFIGURE_ARGS.join(" "),
        patch_set_text(&patches),
    );
    if std::fs::read_to_string(&marker).ok().as_deref() != Some(&fingerprint) {
        copy_tree(&src, &work_src);
        apply_patches(&patches, &work_src);
        // Wipe the build directory as well: an autotools build tree
        // hard-codes its srcdir, so Makefiles left by an earlier
        // configure point at a path that may no longer exist.
        let _ = std::fs::remove_dir_all(&build_dir);
        std::fs::create_dir_all(&build_dir).expect("mkdir libtpms-build");
        run_autogen(&work_src);
        run_configure(&work_src, &build_dir, &crypto, &target_os, &cflags);
        run_make(&build_dir);
        std::fs::write(&marker, &fingerprint).expect("write marker");
    }

    let lib_dir = build_dir.join("src").join(".libs");
    println!("cargo:rustc-link-search=native={}", lib_dir.display());

    match &crypto {
        Crypto::SmartOs { linkfarm, .. } => {
            // Only the link farm goes on the search path. Naming the
            // pkgsrc directory as well would let `ld` pick pkgsrc's
            // libcrypto.so first and silently record the wrong NEEDED
            // entry. tools/check-needed.sh is the backstop for that.
            println!("cargo:rustc-link-search=native={}", linkfarm.display());
        }
        Crypto::PkgConfig => {
            // OpenSSL lives outside `/usr/lib` on illumos hosts
            // (typically /opt/local/lib via pkgsrc). Surface its
            // directory to rustc so `ld` finds libcrypto.
            for dir in pkg_config(&["--libs-only-L", "openssl"])
                .split_whitespace()
                .filter_map(|a| a.strip_prefix("-L"))
            {
                println!("cargo:rustc-link-search=native={dir}");
            }
        }
    }

    // libtpms.a is a convenience archive that already bundles tpm12 +
    // tpm2 object code. Linking the per-version archives separately
    // would cause "multiply-defined symbol" errors on illumos `ld`.
    println!("cargo:rustc-link-lib=static=tpms");
    println!("cargo:rustc-link-lib=dylib=crypto");

    // Expose the include path to dependents.
    println!("cargo:include={}", src.join("include").display());
}

impl Crypto {
    /// Identify the OpenSSL this archive was built against, so a
    /// cached OUT_DIR from a different choice is rebuilt rather than
    /// reused.
    fn fingerprint(&self) -> String {
        match self {
            Crypto::SmartOs { header, .. } => {
                let syms = std::fs::read_to_string(header)
                    .expect("read generated prefix header")
                    .lines()
                    .count();
                format!("smartos {SMARTOS_CRYPTO} {syms}")
            }
            Crypto::PkgConfig => format!(
                "pkg-config {} {}",
                pkg_config(&["--modversion", "openssl"]),
                pkg_config(&["--libs-only-L", "openssl"])
            ),
        }
    }
}

/// Pick the OpenSSL the vendored libtpms builds against, and prove the
/// choice works before anything is compiled against it.
fn select_crypto(out_dir: &Path) -> Crypto {
    // A cross build cannot run the probe below, and a SmartOS host says
    // nothing about the target. Take the platform path only for a native
    // build.
    let native = env::var("HOST") == env::var("TARGET");
    if !(native && Path::new(SMARTOS_CRYPTO).is_file()) {
        return Crypto::PkgConfig;
    }

    let header = out_dir.join("sunw_prefix.h");
    let linkfarm = out_dir.join("smartos-crypto");
    gen_prefix_header(&header);
    make_linkfarm(&linkfarm);
    check_abi(out_dir, &header, &linkfarm);
    Crypto::SmartOs { header, linkfarm }
}

/// Emit a `#pragma redefine_extname` header from the platform
/// library's own export list, so libtpms' plain `EVP_*`/`AES_*` calls
/// bind to the `sunw_`-prefixed names the platform actually exports.
///
/// Deriving the list from the library rather than vendoring illumos-
/// extra's `openssl3/sunw_prefix.h` keeps the mangling in step with
/// whatever platform image the build host runs.
fn gen_prefix_header(header: &Path) {
    let out = Command::new("/usr/bin/nm")
        .args(["-DPg", SMARTOS_CRYPTO])
        .output()
        .expect("spawn nm");
    assert!(out.status.success(), "nm on {SMARTOS_CRYPTO} failed");
    let text = String::from_utf8_lossy(&out.stdout);
    let mut names: Vec<&str> = text
        .lines()
        .filter_map(|line| {
            let mut field = line.split_whitespace();
            let name = field.next()?;
            // Functions, initialized data and bss. Anything else is
            // not an entry point libtpms can call.
            if !matches!(field.next()?, "T" | "D" | "B") {
                return None;
            }
            name.strip_prefix("sunw_")
        })
        .collect();
    names.sort_unstable();
    names.dedup();
    assert!(
        names.len() > 1000,
        "implausibly few sunw_ exports ({}) in {SMARTOS_CRYPTO}: \
         refusing to build a half-mangled libtpms",
        names.len()
    );

    let mut buf = String::from(
        "/* Generated by vmm-tpm-sys/build.rs from the platform \
         libcrypto. Do not edit. */\n\
         #ifndef _VMM_SUNW_PREFIX_H\n#define _VMM_SUNW_PREFIX_H\n",
    );
    for name in &names {
        buf.push_str(&format!("#pragma redefine_extname {name} sunw_{name}\n"));
    }
    buf.push_str("#endif\n");
    std::fs::write(header, buf).expect("write sunw_prefix.h");
}

/// A directory holding one `libcrypto.so` symlink at the platform
/// library, so both libtpms' configure probes and rustc's `-lcrypto`
/// resolve to it rather than to pkgsrc.
fn make_linkfarm(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).expect("mkdir linkfarm");
    std::os::unix::fs::symlink(SMARTOS_CRYPTO, dir.join("libcrypto.so"))
        .expect("symlink platform libcrypto");
}

/// Compile and run one C program against the platform library through
/// the generated header.
///
/// The headers come from pkgsrc and the library from the platform
/// image, and the two track different OpenSSL releases. Within the
/// OpenSSL 3 series that is supported, but a build host whose pkgsrc
/// has moved to OpenSSL 4 would otherwise link cleanly and then
/// misbehave at run time. Catch the mismatch here, where the message
/// can say what to do about it, and prove the mangling resolves at the
/// same time.
fn check_abi(out_dir: &Path, header: &Path, linkfarm: &Path) {
    let probe_src = out_dir.join("smartos-crypto-probe.c");
    std::fs::write(&probe_src, ABI_PROBE_C).expect("write probe source");
    let probe_bin = out_dir.join("smartos-crypto-probe");

    let cc = env::var("CC").unwrap_or_else(|_| "cc".into());
    let mut cmd = Command::new(&cc);
    cmd.arg(&probe_src)
        .arg("-o")
        .arg(&probe_bin)
        .arg(format!("-include{}", header.display()));
    for flag in pkg_config(&["--cflags-only-I", "openssl"]).split_whitespace() {
        cmd.arg(flag);
    }
    cmd.arg(format!("-L{}", linkfarm.display())).arg("-lcrypto");
    let out = cmd.output().expect("spawn cc for the OpenSSL ABI probe");
    assert!(
        out.status.success(),
        "cannot build against {SMARTOS_CRYPTO}:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let out = Command::new(&probe_bin).output().expect("spawn ABI probe");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "the OpenSSL ABI probe failed against {SMARTOS_CRYPTO}. The \
         pkgsrc headers and the platform library disagree, so a vTPM \
         built here would not work. Build on a host whose pkgsrc \
         OpenSSL major version matches the platform image.\n{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // One line, so a build log always names the OpenSSL that was
    // actually used. The rest of the probe output only matters when it
    // fails, and it is in the panic message above.
    if let Some(versions) = stdout.lines().next() {
        println!("cargo:warning=vmm-tpm-sys: {versions}");
    }
}

/// Header/library agreement plus a known-answer test over the calls
/// libtpms actually makes. Prints one `version` line for the build log.
const ABI_PROBE_C: &str = r#"
#include <stdio.h>
#include <string.h>
#include <openssl/aes.h>
#include <openssl/bn.h>
#include <openssl/crypto.h>
#include <openssl/ec.h>
#include <openssl/evp.h>
#include <openssl/opensslv.h>
#include <openssl/param_build.h>
#include <openssl/rand.h>
#include <openssl/rsa.h>
#include <openssl/sha.h>

/* SHA-256("abc") */
static const unsigned char kat[32] = {
 0xba,0x78,0x16,0xbf,0x8f,0x01,0xcf,0xea,0x41,0x41,0x40,0xde,0x5d,0xae,0x22,0x23,
 0xb0,0x03,0x61,0xa3,0x96,0x17,0x7a,0x9c,0xb4,0x10,0xff,0x61,0xf2,0x00,0x15,0xad };

int main(void) {
    unsigned long hv = OPENSSL_VERSION_NUMBER;
    unsigned long lv = OpenSSL_version_num();
    unsigned char md[32], key[32] = {0}, rb[8];
    SHA256_CTX sha; AES_KEY ak; EVP_CIPHER_CTX *cc;
    EVP_KDF *kdf; OSSL_PARAM_BLD *bld; RSA *rsa; BIGNUM *bn; EC_KEY *ec;

    printf("openssl headers 0x%08lx, platform library 0x%08lx (%s)\n",
           hv, lv, OpenSSL_version(OPENSSL_VERSION));
    if ((hv >> 28) != (lv >> 28)) {
        printf("FAIL: OpenSSL major version differs between the headers "
               "and the platform library\n");
        return 1;
    }
    if (SHA256_Init(&sha) != 1) { puts("FAIL SHA256_Init"); return 1; }
    SHA256_Update(&sha, "abc", 3);
    SHA256_Final(md, &sha);
    if (memcmp(md, kat, sizeof kat) != 0) { puts("FAIL SHA256 KAT"); return 1; }
    if (AES_set_encrypt_key(key, 256, &ak) != 0) { puts("FAIL AES"); return 1; }
    cc = EVP_CIPHER_CTX_new(); if (!cc) { puts("FAIL EVP ctx"); return 1; }
    if (EVP_EncryptInit_ex(cc, EVP_aes_256_cbc(), NULL, key, key) != 1) {
        puts("FAIL EVP_EncryptInit_ex"); return 1; }
    EVP_CIPHER_CTX_free(cc);
    rsa = RSA_new(); if (!rsa) { puts("FAIL RSA"); return 1; } RSA_free(rsa);
    bn = BN_new(); if (!bn) { puts("FAIL BN"); return 1; } BN_free(bn);
    ec = EC_KEY_new(); if (!ec) { puts("FAIL EC"); return 1; } EC_KEY_free(ec);
    kdf = EVP_KDF_fetch(NULL, "HKDF", NULL);
    if (!kdf) { puts("FAIL EVP_KDF_fetch"); return 1; } EVP_KDF_free(kdf);
    bld = OSSL_PARAM_BLD_new();
    if (!bld) { puts("FAIL OSSL_PARAM_BLD_new"); return 1; }
    OSSL_PARAM_BLD_free(bld);
    if (RAND_bytes(rb, sizeof rb) != 1) { puts("FAIL RAND_bytes"); return 1; }
    puts("platform libcrypto resolves and passes the SHA-256 KAT");
    return 0;
}
"#;

fn run_autogen(src: &Path) {
    // libtpms' autogen.sh runs autoreconf in $srcdir, then runs
    // configure unless NOCONFIGURE is set. This script runs its own
    // out-of-tree configure.
    let status = Command::new("sh")
        .arg(src.join("autogen.sh"))
        .env("NOCONFIGURE", "1")
        .current_dir(src)
        .status()
        .expect("spawn autogen.sh");
    assert!(status.success(), "libtpms autogen.sh failed");
}

fn run_configure(
    src: &Path,
    build: &Path,
    crypto: &Crypto,
    target_os: &str,
    base: &str,
) {
    let configure = src.join("configure");
    assert!(
        configure.is_file(),
        "configure script not generated by autogen.sh at {}",
        configure.display()
    );

    let mut cflags = String::from(base);
    let mut ldflags = String::new();
    let mut libs = String::new();

    // libtpms' autoconf checks `AES_set_encrypt_key in -lcrypto` to
    // verify OpenSSL. On illumos hosts the headers live under
    // /opt/local or /opt/tools, not /usr/include, so configure needs
    // their path in CFLAGS. A platform image ships no OpenSSL headers,
    // so pkg-config supplies them in both modes.
    let includes = pkg_config(&["--cflags-only-I", "openssl"]);
    if !includes.is_empty() {
        cflags.push(' ');
        cflags.push_str(&includes);
    }

    match crypto {
        Crypto::SmartOs { header, linkfarm } => {
            cflags.push_str(&format!(" -include {}", header.display()));
            ldflags.push_str(&format!("-L{}", linkfarm.display()));
            libs.push_str("-lcrypto");
        }
        Crypto::PkgConfig => {
            let dirs = pkg_config(&["--libs-only-L", "openssl"]);
            ldflags.push_str(&dirs);
            // Mirror every -L as a runpath entry so the autoconf
            // "can run compiled program" check (which builds and
            // executes a tiny test binary) finds libcrypto at run time
            // without LD_LIBRARY_PATH set in the environment.
            for dir in
                dirs.split_whitespace().filter_map(|a| a.strip_prefix("-L"))
            {
                if !ldflags.is_empty() {
                    ldflags.push(' ');
                }
                ldflags.push_str(&runpath_flag(target_os, dir));
            }
            libs.push_str(&pkg_config(&["--libs-only-l", "openssl"]));

            // pkg-config may have no openssl.pc. Probe the common
            // illumos prefixes instead.
            if ldflags.is_empty() {
                for prefix in ["/opt/tools", "/opt/local", "/usr/local"] {
                    let inc = format!("{prefix}/include/openssl");
                    let lib = format!("{prefix}/lib");
                    if Path::new(&inc).is_dir()
                        && Path::new(&lib).join("libcrypto.so").exists()
                    {
                        cflags.push_str(&format!(" -I{prefix}/include"));
                        ldflags.push_str(&format!("-L{prefix}/lib"));
                        break;
                    }
                }
            }
        }
    }

    let mut cmd = Command::new(&configure);
    cmd.args(CONFIGURE_ARGS)
        .env("CFLAGS", &cflags)
        .current_dir(build);
    if !ldflags.is_empty() {
        cmd.env("LDFLAGS", &ldflags);
    }
    if !libs.is_empty() {
        cmd.env("LIBS", &libs);
    }
    let status = cmd.status().expect("spawn configure");
    assert!(status.success(), "libtpms configure failed");
}

/// Hardening is left enabled: this C code parses guest TPM commands.
/// Every flag it adds is behind an autoconf probe, so a compiler or
/// linker that refuses one drops that one and keeps the rest.
const CONFIGURE_ARGS: &[&str] = &[
    "--with-openssl",
    // Makes configure fail if OpenSSL is not the crypto library. The
    // RSA functions are probed separately: patch 0003 turns a failed
    // probe into a compile error (CVE-2026-6727).
    "--enable-use-openssl-functions",
    "--with-tpm2",
    "--enable-static",
    "--disable-shared",
];

/// Run pkg-config and return its trimmed stdout, or an empty string if
/// it is missing or fails.
fn pkg_config(args: &[&str]) -> String {
    let tool = env::var("PKG_CONFIG").unwrap_or_else(|_| "pkg-config".into());
    match Command::new(tool).args(args).output() {
        Ok(out) if out.status.success() => {
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }
        _ => String::new(),
    }
}

fn copy_tree(src: &Path, dst: &Path) {
    // Start from an empty copy, so no state from an earlier configure
    // run remains.
    if dst.exists() {
        std::fs::remove_dir_all(dst).expect("remove stale work_src");
    }
    // cp -R copies symlinks as symlinks.
    let status = Command::new("cp")
        .arg("-R")
        .arg(src)
        .arg(dst)
        .status()
        .expect("spawn cp");
    assert!(status.success(), "cp -R libtpms source failed");
}

/// Apply the illumos patches to the copy in `OUT_DIR`.
///
/// Patches apply in file name order, and every patch must apply. A
/// skipped patch gives a libtpms that does not compile on illumos, or
/// one that compiles but behaves differently from the reviewed tree.
fn apply_patches(patches: &Path, work_src: &Path) {
    for patch in &patch_files(patches) {
        let file = std::fs::File::open(patch)
            .unwrap_or_else(|e| panic!("open {}: {e}", patch.display()));
        let status = Command::new("patch")
            .arg("-p1")
            .arg("--batch")
            .current_dir(work_src)
            .stdin(file)
            .status()
            .expect("spawn patch");
        assert!(
            status.success(),
            "{} did not apply cleanly. The submodule revision and the \
             patch set have diverged; see third_party/libtpms-illumos/\
             PORTING_NOTES.md",
            patch.display()
        );
    }
}

/// The patch files, in the order they apply.
fn patch_files(patches: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(patches)
        .unwrap_or_else(|e| panic!("read {}: {e}", patches.display()))
        .map(|e| e.expect("read patch dir entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "patch"))
        .collect();
    files.sort();
    assert!(
        !files.is_empty(),
        "no patches found in {}: the illumos build needs them",
        patches.display()
    );
    files
}

/// Every patch's name and content, for the build stamp.
fn patch_set_text(patches: &Path) -> String {
    patch_files(patches)
        .iter()
        .map(|p| {
            let body = std::fs::read_to_string(p)
                .unwrap_or_else(|e| panic!("read {}: {e}", p.display()));
            format!("{}\n{body}", p.display())
        })
        .collect()
}

fn run_make(build: &Path) {
    // illumos ships `gmake`. macOS dev hosts use the system `make`.
    let make = if Command::new("gmake").arg("--version").output().is_ok() {
        "gmake"
    } else {
        "make"
    };
    let jobs = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);
    let status = Command::new(make)
        .arg(format!("-j{}", jobs))
        .current_dir(build)
        .status()
        .expect("spawn gmake/make");
    assert!(status.success(), "libtpms build failed");
}

fn workspace_root() -> PathBuf {
    // The workspace root is two levels above this crate's directory,
    // crates/vmm-tpm-sys.
    let crate_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    crate_dir
        .parent()
        .and_then(|p| p.parent())
        .map(PathBuf::from)
        .expect("workspace root")
}
