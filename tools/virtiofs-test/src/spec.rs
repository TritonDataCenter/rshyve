// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What to run inside a container image, and as whom.
//!
//! `tools/virtiofs-container-stage.sh` derives this from the image's
//! OCI config, and the fields keep the OCI names. Only the fields the
//! guest uses are declared. Serde ignores any other field.
//!
//! The type does not depend on how the spec arrives, so a change of
//! transport (for example to fw_cfg) changes only the caller.

use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
pub struct ContainerSpec {
    #[serde(default)]
    pub entrypoint: Vec<String>,
    #[serde(default)]
    pub cmd: Vec<String>,
    /// `KEY=VALUE` strings, the form the OCI config uses.
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub workdir: Option<String>,
    /// `uid`, `uid:gid`, `name`, or `name:group`.
    #[serde(default)]
    pub user: Option<String>,
}

impl ContainerSpec {
    pub fn parse(raw: &str) -> Result<Self, String> {
        serde_json::from_str(raw).map_err(|e| format!("parse spec: {e}"))
    }

    /// Full argv. OCI joins the two: `cmd` supplies default arguments to
    /// `entrypoint`, and stands alone when `entrypoint` is empty.
    pub fn argv(&self) -> Result<Vec<String>, String> {
        let mut argv = self.entrypoint.clone();
        argv.extend(self.cmd.iter().cloned());
        if argv.is_empty() {
            return Err("image declares neither entrypoint nor cmd".into());
        }
        Ok(argv)
    }

    /// Environment for the process, with a default PATH if the image
    /// declares none. With no PATH, a bare command name does not
    /// resolve.
    pub fn environment(&self) -> Vec<String> {
        let mut env = self.env.clone();
        if !env.iter().any(|e| e.starts_with("PATH=")) {
            env.push(
                "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
                    .to_string(),
            );
        }
        env
    }

    pub fn workdir(&self) -> &str {
        match self.workdir.as_deref() {
            Some(w) if !w.is_empty() => w,
            _ => "/",
        }
    }

    /// Resolve `user` against the image's own account files.
    ///
    /// `root` is the path of the image. Names resolve there, not in the
    /// initramfs, which has no accounts.
    pub fn resolve_user(&self, root: &Path) -> Result<(u32, u32), String> {
        let spec = match self.user.as_deref() {
            Some(u) if !u.is_empty() => u,
            _ => return Ok((0, 0)),
        };
        let (user, group) = match spec.split_once(':') {
            Some((u, g)) => (u, Some(g)),
            None => (spec, None),
        };

        let (uid, primary_gid) = match user.parse::<u32>() {
            Ok(uid) => (uid, None),
            Err(_) => {
                let (uid, gid) = lookup_passwd(root, user)?;
                (uid, Some(gid))
            }
        };

        let gid = match group {
            Some(g) => match g.parse::<u32>() {
                Ok(gid) => gid,
                Err(_) => lookup_group(root, g)?,
            },
            // No group: the account's primary group, or root for a bare
            // numeric uid, as runc does.
            None => primary_gid.unwrap_or(0),
        };
        Ok((uid, gid))
    }
}

/// Find `name` in `<root>/etc/passwd`, returning its uid and gid.
fn lookup_passwd(root: &Path, name: &str) -> Result<(u32, u32), String> {
    let path = root.join("etc/passwd");
    let body = std::fs::read_to_string(&path)
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    for line in body.lines() {
        let f: Vec<&str> = line.split(':').collect();
        if f.len() >= 4 && f[0] == name {
            let uid = f[2]
                .parse()
                .map_err(|_| format!("bad uid for {name}: {}", f[2]))?;
            let gid = f[3]
                .parse()
                .map_err(|_| format!("bad gid for {name}: {}", f[3]))?;
            return Ok((uid, gid));
        }
    }
    Err(format!("user '{name}' is not in the image's /etc/passwd"))
}

/// Find `name` in `<root>/etc/group`, returning its gid.
fn lookup_group(root: &Path, name: &str) -> Result<u32, String> {
    let path = root.join("etc/group");
    let body = std::fs::read_to_string(&path)
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    for line in body.lines() {
        let f: Vec<&str> = line.split(':').collect();
        if f.len() >= 3 && f[0] == name {
            return f[2]
                .parse()
                .map_err(|_| format!("bad gid for {name}: {}", f[2]));
        }
    }
    Err(format!("group '{name}' is not in the image's /etc/group"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmd_supplies_arguments_to_entrypoint() {
        let s = ContainerSpec::parse(
            r#"{"entrypoint":["/docker-entrypoint.sh"],
                "cmd":["nginx","-g","daemon off;"]}"#,
        )
        .expect("parse");
        assert_eq!(
            s.argv().expect("argv"),
            vec!["/docker-entrypoint.sh", "nginx", "-g", "daemon off;"]
        );
    }

    #[test]
    fn cmd_alone_is_the_argv() {
        let s = ContainerSpec::parse(r#"{"cmd":["/bin/sh"]}"#).expect("parse");
        assert_eq!(s.argv().expect("argv"), vec!["/bin/sh"]);
    }

    #[test]
    fn an_image_with_no_command_is_an_error() {
        let s = ContainerSpec::parse("{}").expect("parse");
        assert!(s.argv().is_err());
    }

    /// With no PATH a bare command name does not resolve, and an image
    /// can declare none.
    #[test]
    fn path_is_supplied_when_the_image_omits_it() {
        let s = ContainerSpec::parse(r#"{"env":["TZ=UTC"]}"#).expect("parse");
        let env = s.environment();
        assert!(env.iter().any(|e| e == "TZ=UTC"));
        assert!(env.iter().any(|e| e.starts_with("PATH=")));
    }

    #[test]
    fn a_declared_path_is_kept() {
        let s = ContainerSpec::parse(r#"{"env":["PATH=/opt/bin"]}"#)
            .expect("parse");
        assert_eq!(s.environment(), vec!["PATH=/opt/bin"]);
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let s = ContainerSpec::parse(
            r#"{"cmd":["/x"],"ExposedPorts":{"80/tcp":{}},"Labels":null}"#,
        )
        .expect("parse");
        assert_eq!(s.argv().expect("argv"), vec!["/x"]);
    }

    #[test]
    fn workdir_defaults_to_root() {
        assert_eq!(ContainerSpec::default().workdir(), "/");
        let s = ContainerSpec::parse(r#"{"workdir":""}"#).expect("parse");
        assert_eq!(s.workdir(), "/");
    }

    #[test]
    fn numeric_user_needs_no_account_files() {
        let s = ContainerSpec::parse(r#"{"user":"1000:2000"}"#).expect("parse");
        assert_eq!(
            s.resolve_user(Path::new("/nonexistent")).expect("resolve"),
            (1000, 2000)
        );
        let s = ContainerSpec::parse(r#"{"user":"1000"}"#).expect("parse");
        assert_eq!(
            s.resolve_user(Path::new("/nonexistent")).expect("resolve"),
            (1000, 0)
        );
    }

    #[test]
    fn no_user_means_root() {
        assert_eq!(
            ContainerSpec::default()
                .resolve_user(Path::new("/nonexistent"))
                .expect("resolve"),
            (0, 0)
        );
    }

    #[test]
    fn named_user_resolves_against_the_image() {
        let dir = std::env::temp_dir().join("vfs-spec-test");
        std::fs::create_dir_all(dir.join("etc")).expect("mkdir");
        std::fs::write(
            dir.join("etc/passwd"),
            "root:x:0:0:root:/root:/bin/sh\nnginx:x:101:102:nginx:/:/sbin/nologin\n",
        )
        .expect("write passwd");
        std::fs::write(dir.join("etc/group"), "tape:x:26:\n")
            .expect("write group");

        let s = ContainerSpec::parse(r#"{"user":"nginx"}"#).expect("parse");
        assert_eq!(s.resolve_user(&dir).expect("resolve"), (101, 102));

        let s =
            ContainerSpec::parse(r#"{"user":"nginx:tape"}"#).expect("parse");
        assert_eq!(s.resolve_user(&dir).expect("resolve"), (101, 26));

        let s = ContainerSpec::parse(r#"{"user":"ghost"}"#).expect("parse");
        assert!(s.resolve_user(&dir).is_err());

        std::fs::remove_dir_all(&dir).ok();
    }
}
