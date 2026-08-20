//! Optional `elfpak.toml`. CLI arguments always override the file, and the tool
//! stays fully usable without one.

use crate::{
    error::{Error, Result, io},
    rootfs::policy::Preset,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Name of the configuration file discovered beside the working directory.
pub const CONFIG_NAME_DEFAULT: &str = "elfpak.toml";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub package: PackageConfig,
    #[serde(default)]
    pub runtime: RuntimeConfig,
    #[serde(default)]
    pub include: IncludeConfig,
    #[serde(default)]
    pub dependencies: DependencyConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageConfig {
    pub binary: Option<PathBuf>,
    pub install: Option<PathBuf>,
    pub output: Option<PathBuf>,
    pub tar: Option<PathBuf>,
    pub root: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConfig {
    pub preset: Option<Preset>,
    pub user: Option<String>,
    pub ca_certificates: Option<bool>,
    pub tmp: Option<bool>,
    pub passwd_group: Option<bool>,
    pub nsswitch: Option<bool>,
    pub tzdata: Option<bool>,
    /// `true` always writes a cache, `false` never does; left out, the planner
    /// writes one exactly when the closure needs it.
    pub ld_so_cache: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IncludeConfig {
    #[serde(default)]
    pub paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DependencyConfig {
    pub allow: Option<Vec<String>>,
}

impl Config {
    pub fn parse(text: &str) -> Result<Config> {
        toml::from_str(text).map_err(|e| Error::Config {
            message: e.to_string(),
        })
    }

    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path).map_err(|e| io(path, e))?;
        Config::parse(&text).map_err(|e| match e {
            Error::Config { message } => Error::Config {
                message: format!("{}: {message}", path.display()),
            },
            other => other,
        })
    }

    /// Load `elfpak.toml` from `dir` if it exists. A missing file is not an error.
    pub fn discover(dir: &Path) -> Result<Option<(PathBuf, Config)>> {
        let candidate = dir.join(CONFIG_NAME_DEFAULT);
        if !candidate.is_file() {
            return Ok(None);
        }
        let config = Config::load(&candidate)?;
        Ok(Some((candidate, config)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_documented_example() {
        let config = Config::parse(
            r#"
[package]
binary = "/my-server"
install = "/app/server"
output = "/rootfs"

[runtime]
preset = "web"
user = "65532:65532"
tzdata = false

[include]
paths = ["/app/templates"]

[dependencies]
allow = ["libc.so.6", "libgcc_s.so.1"]
"#,
        )
        .unwrap();

        assert_eq!(config.package.install, Some(PathBuf::from("/app/server")));
        assert_eq!(config.runtime.preset, Some(Preset::Web));
        assert_eq!(config.runtime.tzdata, Some(false));
        assert_eq!(config.runtime.ld_so_cache, None);
        assert_eq!(config.include.paths, vec![PathBuf::from("/app/templates")]);
        assert_eq!(config.dependencies.allow.unwrap().len(), 2);
    }

    #[test]
    fn an_empty_config_is_valid() {
        let config = Config::parse("").unwrap();
        assert!(config.package.binary.is_none());
        assert!(config.include.paths.is_empty());
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let err = Config::parse("[runtime]\npresset = \"web\"\n").unwrap_err();
        assert!(err.to_string().contains("presset"), "{err}");
    }
}
