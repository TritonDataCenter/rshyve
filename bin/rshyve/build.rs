use std::process::Command;

fn find_git() -> &'static str {
    for path in ["/opt/tools/bin/git", "/usr/bin/git", "git"] {
        if Command::new(path).arg("--version").output().is_ok() {
            return path;
        }
    }
    "git"
}

fn main() {
    let git = find_git();

    let output = Command::new(git)
        .args(["rev-parse", "--short", "HEAD"])
        .output();

    let commit = match output {
        Ok(o) if o.status.success() => {
            String::from_utf8_lossy(&o.stdout).trim().to_string()
        }
        _ => "unknown".to_string(),
    };

    println!("cargo:rustc-env=VMM_GIT_COMMIT={commit}");

    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs/");

    // Do not emit a runpath. The pkgsrc gcc driver adds /opt/local/lib
    // to every binary it links, and vmm-tpm-sys finds OpenSSL in the
    // platform image.
}
