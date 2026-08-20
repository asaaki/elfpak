//! Runtime policy: everything that ELF analysis cannot prove.
//!
//! Presets are pure configuration. They never add hidden behaviour: every file
//! they contribute shows up in the bundle plan with a policy reason attached.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Preset {
    Minimal,
    Web,
}

impl std::str::FromStr for Preset {
    type Err = String;

    fn from_str(s: &str) -> Result<Preset, String> {
        match s {
            "minimal" => Ok(Preset::Minimal),
            "web" => Ok(Preset::Web),
            other => Err(format!(
                "unknown preset `{other}` (expected minimal or web)"
            )),
        }
    }
}

impl std::fmt::Display for Preset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Preset::Minimal => "minimal",
            Preset::Web => "web",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeFeature {
    CaCertificates,
    Tmp,
    PasswdGroup,
    Nsswitch,
    Tzdata,
}

impl RuntimeFeature {
    pub fn as_str(&self) -> &'static str {
        match self {
            RuntimeFeature::CaCertificates => "ca-certificates",
            RuntimeFeature::Tmp => "tmp",
            RuntimeFeature::PasswdGroup => "passwd-group",
            RuntimeFeature::Nsswitch => "nsswitch",
            RuntimeFeature::Tzdata => "tzdata",
        }
    }
}

/// The identity the packaged application is expected to run as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserSpec {
    pub uid: u32,
    pub gid: u32,
    pub name: String,
    pub group: String,
}

impl std::fmt::Display for UserSpec {
    /// Canonical `name:uid:gid` form, which [`UserSpec::parse`] round-trips.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}:{}", self.name, self.uid, self.gid)
    }
}

impl UserSpec {
    /// Accepts `uid`, `uid:gid` and `name:uid:gid`.
    pub fn parse(value: &str) -> Result<UserSpec, Error> {
        let invalid = || Error::Config {
            message: format!("invalid --user value `{value}` (expected uid[:gid] or name:uid:gid)"),
        };
        let parts: Vec<&str> = value.split(':').collect();
        match parts.as_slice() {
            [uid] => {
                let uid: u32 = uid.parse().map_err(|_| invalid())?;
                Ok(UserSpec {
                    uid,
                    gid: uid,
                    name: "app".to_string(),
                    group: "app".to_string(),
                })
            }
            [uid, gid] => {
                let uid: u32 = uid.parse().map_err(|_| invalid())?;
                let gid: u32 = gid.parse().map_err(|_| invalid())?;
                Ok(UserSpec {
                    uid,
                    gid,
                    name: "app".to_string(),
                    group: "app".to_string(),
                })
            }
            [name, uid, gid] => {
                let uid: u32 = uid.parse().map_err(|_| invalid())?;
                let gid: u32 = gid.parse().map_err(|_| invalid())?;
                Ok(UserSpec {
                    uid,
                    gid,
                    name: (*name).to_string(),
                    group: (*name).to_string(),
                })
            }
            _ => Err(invalid()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimePolicy {
    pub ca_certificates: bool,
    pub tmp: bool,
    pub passwd_group: bool,
    pub nsswitch: bool,
    pub tzdata: bool,
    pub user: Option<UserSpec>,
    pub includes: Vec<PathBuf>,
}

impl Default for RuntimePolicy {
    fn default() -> RuntimePolicy {
        RuntimePolicy::from_preset(Preset::Minimal)
    }
}

impl RuntimePolicy {
    pub fn from_preset(preset: Preset) -> RuntimePolicy {
        match preset {
            Preset::Minimal => RuntimePolicy {
                ca_certificates: false,
                tmp: false,
                passwd_group: false,
                nsswitch: false,
                tzdata: false,
                user: None,
                includes: Vec::new(),
            },
            // Pragmatic server defaults. Timezone data stays opt-in.
            Preset::Web => RuntimePolicy {
                ca_certificates: true,
                tmp: true,
                passwd_group: true,
                nsswitch: true,
                tzdata: false,
                user: None,
                includes: Vec::new(),
            },
        }
    }

    /// Locations of a system CA bundle, most common first.
    pub const CA_BUNDLE_CANDIDATES: &'static [&'static str] = &[
        "/etc/ssl/certs/ca-certificates.crt",
        "/etc/pki/tls/certs/ca-bundle.crt",
        "/etc/ssl/ca-bundle.pem",
        "/etc/ssl/cert.pem",
        "/usr/local/share/certs/ca-root-nss.crt",
    ];

    /// NSS modules that older glibc versions `dlopen` at runtime. Since glibc
    /// 2.34 these are built into `libc.so.6`, so they are included only if the
    /// source root actually provides them.
    pub const NSS_MODULES: &'static [&'static str] =
        &["libnss_files.so.2", "libnss_dns.so.2", "libresolv.so.2"];

    pub fn passwd_contents(&self) -> Vec<u8> {
        let mut out = String::from("root:x:0:0:root:/root:/sbin/nologin\n");
        out.push_str("nobody:x:65534:65534:nobody:/nonexistent:/sbin/nologin\n");
        if let Some(user) = &self.user
            && user.uid != 0
            && user.uid != 65534
        {
            out.push_str(&format!(
                "{}:x:{}:{}:{}:/nonexistent:/sbin/nologin\n",
                user.name, user.uid, user.gid, user.name
            ));
        }
        out.into_bytes()
    }

    pub fn group_contents(&self) -> Vec<u8> {
        let mut out = String::from("root:x:0:\n");
        out.push_str("nogroup:x:65534:\n");
        if let Some(user) = &self.user
            && user.gid != 0
            && user.gid != 65534
        {
            out.push_str(&format!("{}:x:{}:\n", user.group, user.gid));
        }
        out.into_bytes()
    }

    pub fn nsswitch_contents(&self) -> Vec<u8> {
        let mut out = String::new();
        out.push_str("# generated by elfpak\n");
        out.push_str("passwd:     files\n");
        out.push_str("group:      files\n");
        out.push_str("shadow:     files\n");
        out.push_str("hosts:      files dns\n");
        out.push_str("networks:   files\n");
        out.push_str("protocols:  files\n");
        out.push_str("services:   files\n");
        out.into_bytes()
    }
}

/// Allow-list of shared libraries the build is permitted to depend on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DependencyPolicy {
    /// `None` disables the check entirely; `Some` enforces the list.
    pub allow: Option<Vec<String>>,
}

impl DependencyPolicy {
    pub fn allow_all() -> DependencyPolicy {
        DependencyPolicy { allow: None }
    }

    pub fn allow_list(list: Vec<String>) -> DependencyPolicy {
        DependencyPolicy { allow: Some(list) }
    }

    /// A library is identified by its `DT_SONAME` when present, otherwise by
    /// file name, which is what a user would write on the command line.
    pub fn is_allowed(&self, soname: &str, path: &Path) -> bool {
        let Some(allow) = &self.allow else {
            return true;
        };
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        allow.iter().any(|a| a == soname || *a == file_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_are_explicit() {
        let minimal = RuntimePolicy::from_preset(Preset::Minimal);
        assert!(!minimal.ca_certificates && !minimal.tmp && !minimal.passwd_group);

        let web = RuntimePolicy::from_preset(Preset::Web);
        assert!(web.ca_certificates && web.tmp && web.passwd_group && web.nsswitch);
        // Timezone data stays opt-in even in the web preset.
        assert!(!web.tzdata);
    }

    #[test]
    fn user_spec_accepts_the_documented_forms() {
        assert_eq!(
            UserSpec::parse("65532").unwrap(),
            UserSpec {
                uid: 65532,
                gid: 65532,
                name: "app".into(),
                group: "app".into()
            }
        );
        assert_eq!(UserSpec::parse("65532:1000").unwrap().gid, 1000);
        assert_eq!(UserSpec::parse("svc:1:2").unwrap().name, "svc");
        assert!(UserSpec::parse("nobody").is_err());
    }

    #[test]
    fn passwd_and_group_include_the_requested_user() {
        let mut policy = RuntimePolicy::from_preset(Preset::Web);
        policy.user = Some(UserSpec::parse("65532:65532").unwrap());
        let passwd = String::from_utf8(policy.passwd_contents()).unwrap();
        assert!(passwd.contains("app:x:65532:65532"));
        assert!(passwd.starts_with("root:x:0:0:"));
        let group = String::from_utf8(policy.group_contents()).unwrap();
        assert!(group.contains("app:x:65532:"));
    }

    #[test]
    fn root_and_nobody_are_not_duplicated() {
        let mut policy = RuntimePolicy::from_preset(Preset::Web);
        policy.user = Some(UserSpec::parse("65534:65534").unwrap());
        let passwd = String::from_utf8(policy.passwd_contents()).unwrap();
        assert_eq!(passwd.lines().count(), 2);
    }

    #[test]
    fn dependency_policy_matches_soname_or_file_name() {
        let policy = DependencyPolicy::allow_list(vec!["libc.so.6".into()]);
        assert!(policy.is_allowed("libc.so.6", Path::new("/lib/libc.so.6")));
        assert!(policy.is_allowed("", Path::new("/lib/libc.so.6")));
        assert!(!policy.is_allowed("libssl.so.3", Path::new("/lib/libssl.so.3")));
        assert!(DependencyPolicy::allow_all().is_allowed("anything.so", Path::new("/x")));
    }
}
