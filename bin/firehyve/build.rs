use std::process::Command;

fn find_git() -> &'static str {
    for path in ["/opt/tools/bin/git", "/usr/bin/git", "git"] {
        if Command::new(path).arg("--version").output().is_ok() {
            return match path {
                "/opt/tools/bin/git" => "/opt/tools/bin/git",
                "/usr/bin/git" => "/usr/bin/git",
                _ => "git",
            };
        }
    }
    "git"
}

fn main() {
    let output = Command::new(find_git())
        .args(["rev-parse", "--short", "HEAD"])
        .output();

    let commit = match output {
        Ok(o) if o.status.success() => {
            String::from_utf8_lossy(&o.stdout).trim().to_string()
        }
        _ => "unknown".to_string(),
    };

    println!("cargo:rustc-env=FIREHYVE_GIT_COMMIT={commit}");

    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs/");
}
