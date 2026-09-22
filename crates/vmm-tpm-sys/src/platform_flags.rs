// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Per-target compiler and linker flags for the vendored libtpms build.
//
// `build.rs` includes this file, and `lib.rs` compiles it again under
// `cfg(test)`. A developer host never builds libtpms for illumos, so
// the tests below are the only place the illumos flags are checked away
// from the build host.
//
// Both callers take `target_os` from cargo's `CARGO_CFG_TARGET_OS`.

/// Base `CFLAGS` for a libtpms build.
///
/// A feature-test macro selects which declarations a libc header shows,
/// so it belongs to one libc and not to every build. illumos keeps
/// `dprintf`/`vdprintf` behind the POSIX macros and its own extensions
/// behind `__EXTENSIONS__`, so the product build needs all three. The
/// same POSIX macros put Darwin libc into strict POSIX mode, where
/// `asprintf` and `vasprintf` are not declared and libtpms does not
/// compile. Other targets get what upstream libtpms builds with: no
/// feature macros at all. The libtpms sources that need `_GNU_SOURCE`
/// on glibc define it themselves.
///
/// `-fPIC` is not optional: rshyve links this static archive into a
/// position-independent executable.
fn base_cflags(target_os: &str) -> String {
    let mut flags = String::from("-O2 -fPIC");
    if matches!(target_os, "illumos" | "solaris") {
        flags.push_str(
            " -D_POSIX_C_SOURCE=200809L -D_XOPEN_SOURCE=700 -D__EXTENSIONS__",
        );
    }
    flags
}

/// The linker option that records `dir` as a runtime search path.
///
/// One autoconf check builds a small program and runs it, so that
/// program must find libcrypto with no `LD_LIBRARY_PATH` set. The
/// illumos and GNU linkers spell the option `-R`. The Darwin linker
/// rejects `-R` outright, which fails the check for every compiler
/// probe after it, and spells the option `-rpath`.
fn runpath_flag(target_os: &str, dir: &str) -> String {
    if target_os == "macos" {
        format!("-Wl,-rpath,{dir}")
    } else {
        format!("-Wl,-R,{dir}")
    }
}

#[cfg(test)]
mod tests {
    use super::{base_cflags, runpath_flag};

    /// illumos is the product. Losing one of these macros breaks the
    /// build there and nowhere a developer would notice.
    #[test]
    fn illumos_keeps_its_feature_test_macros() {
        let flags = base_cflags("illumos");
        let want = [
            "-O2",
            "-fPIC",
            "-D_POSIX_C_SOURCE=200809L",
            "-D_XOPEN_SOURCE=700",
            "-D__EXTENSIONS__",
        ];
        for flag in want {
            assert!(
                flags.split(' ').any(|got| got == flag),
                "illumos CFLAGS lost {flag}: {flags}",
            );
        }
        assert_eq!(base_cflags("solaris"), flags, "solaris tracks illumos");
    }

    /// The same macros hide `asprintf`/`vasprintf` in the Darwin
    /// headers, so on a macOS developer host they break the build.
    #[test]
    fn other_targets_get_no_illumos_feature_macros() {
        for target_os in ["macos", "linux", "freebsd", "windows"] {
            let flags = base_cflags(target_os);
            for illumos_only in
                ["_POSIX_C_SOURCE", "_XOPEN_SOURCE", "__EXTENSIONS__"]
            {
                assert!(
                    !flags.contains(illumos_only),
                    "{target_os} CFLAGS carry the illumos macro \
                     {illumos_only}: {flags}",
                );
            }
            assert!(flags.split(' ').any(|got| got == "-fPIC"), "{flags}");
            assert!(flags.split(' ').any(|got| got == "-O2"), "{flags}");
        }
    }

    #[test]
    fn runpath_matches_the_target_linker() {
        assert_eq!(
            runpath_flag("illumos", "/opt/local/lib"),
            "-Wl,-R,/opt/local/lib"
        );
        assert_eq!(runpath_flag("linux", "/usr/lib"), "-Wl,-R,/usr/lib");
        assert_eq!(runpath_flag("macos", "/opt/lib"), "-Wl,-rpath,/opt/lib");
    }
}
