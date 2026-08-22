//! Optional `elfpak.toml`. CLI arguments always override the file, and the tool
//! stays fully usable without one.

use elfpak_core::{Error, Preset, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

/// Name of the configuration file discovered beside the working directory.
const CONFIG_NAME_DEFAULT: &str = "elfpak.toml";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    #[serde(default)]
    pub(crate) package: PackageConfig,
    #[serde(default)]
    pub(crate) runtime: RuntimeConfig,
    #[serde(default)]
    pub(crate) include: IncludeConfig,
    #[serde(default)]
    pub(crate) dependencies: DependencyConfig,
    #[serde(default)]
    pub(crate) image: ImageConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PackageConfig {
    pub(crate) binary: Option<PathBuf>,
    #[serde(default)]
    pub(crate) binaries: Vec<PathBuf>,
    pub(crate) install: Option<PathBuf>,
    pub(crate) install_dir: Option<PathBuf>,
    pub(crate) output: Option<PathBuf>,
    pub(crate) tar: Option<PathBuf>,
    pub(crate) oci_layout: Option<PathBuf>,
    pub(crate) oci_archive: Option<PathBuf>,
    pub(crate) root: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ImageConfig {
    pub(crate) tag: Option<String>,
    #[serde(default)]
    pub(crate) entrypoint: Vec<String>,
    #[serde(default)]
    pub(crate) cmd: Vec<String>,
    pub(crate) working_dir: Option<PathBuf>,
    #[serde(default)]
    pub(crate) env: BTreeMap<String, String>,
    #[serde(default)]
    pub(crate) labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuntimeConfig {
    pub(crate) preset: Option<Preset>,
    pub(crate) user: Option<String>,
    pub(crate) ca_certificates: Option<bool>,
    pub(crate) tmp: Option<bool>,
    pub(crate) passwd_group: Option<bool>,
    pub(crate) nsswitch: Option<bool>,
    pub(crate) tzdata: Option<bool>,
    /// `true` always writes a cache, `false` never does; left out, the planner
    /// writes one exactly when the closure needs it.
    pub(crate) ld_so_cache: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct IncludeConfig {
    #[serde(default)]
    pub(crate) paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DependencyConfig {
    pub(crate) allow: Option<Vec<String>>,
}

impl Config {
    pub(crate) fn parse(text: &str) -> Result<Config> {
        toml::from_str(text).map_err(|e| Error::Config {
            message: e.to_string(),
        })
    }

    pub(crate) fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path).map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let mut config = Config::parse(&text).map_err(|e| match e {
            Error::Config { message } => Error::Config {
                message: format!("{}: {message}", path.display()),
            },
            other => other,
        })?;
        // Filesystem paths in a configuration describe the project containing
        // that configuration, not whichever directory happened to invoke the
        // CLI. Logical in-image paths (`install` and `include.paths`) remain
        // rooted in the source filesystem and are intentionally untouched.
        let base = path.parent().unwrap_or_else(|| Path::new("."));
        rebase_path(&mut config.package.binary, base);
        for binary in &mut config.package.binaries {
            rebase_value(binary, base);
        }
        rebase_path(&mut config.package.output, base);
        rebase_path(&mut config.package.tar, base);
        rebase_path(&mut config.package.oci_layout, base);
        rebase_path(&mut config.package.oci_archive, base);
        rebase_path(&mut config.package.root, base);
        Ok(config)
    }

    /// Load `elfpak.toml` from `dir` if it exists. A missing file is not an error.
    pub(crate) fn discover(dir: &Path) -> Result<Option<(PathBuf, Config)>> {
        let candidate = dir.join(CONFIG_NAME_DEFAULT);
        if !candidate.is_file() {
            return Ok(None);
        }
        let config = Config::load(&candidate)?;
        Ok(Some((candidate, config)))
    }
}

fn rebase_path(path: &mut Option<PathBuf>, base: &Path) {
    if let Some(value) = path
        && value.is_relative()
    {
        *value = base.join(&*value);
    }
}

fn rebase_value(path: &mut PathBuf, base: &Path) {
    if path.is_relative() {
        *path = base.join(&*path);
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
    fn parses_multiple_binary_package_settings() {
        let config = Config::parse(
            "[package]\nbinaries = ['target/release/server', 'target/release/migrate']\ninstall_dir = '/app'\n",
        )
        .unwrap();

        assert_eq!(
            config.package.binaries,
            [
                PathBuf::from("target/release/server"),
                PathBuf::from("target/release/migrate")
            ]
        );
        assert_eq!(config.package.install_dir, Some(PathBuf::from("/app")));
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let err = Config::parse("[runtime]\npresset = \"web\"\n").unwrap_err();
        assert!(err.to_string().contains("presset"), "{err}");
    }

    #[test]
    fn load_resolves_project_paths_against_the_config_file() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let config_path = project.join("elfpak.toml");
        std::fs::write(
            &config_path,
            "[package]\nbinary = 'bin/app'\nroot = 'sysroot'\noutput = 'dist/rootfs'\ntar = 'dist/app.tar'\ninstall = '/app/server'\n\n[include]\npaths = ['/app/data']\n",
        )
        .unwrap();

        let config = Config::load(&config_path).unwrap();
        assert_eq!(config.package.binary, Some(project.join("bin/app")));
        assert_eq!(config.package.root, Some(project.join("sysroot")));
        assert_eq!(config.package.output, Some(project.join("dist/rootfs")));
        assert_eq!(config.package.tar, Some(project.join("dist/app.tar")));
        assert_eq!(config.package.install, Some(PathBuf::from("/app/server")));
        assert_eq!(config.include.paths, vec![PathBuf::from("/app/data")]);
    }

    #[test]
    fn parses_and_rebases_oci_outputs_and_image_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let config_path = project.join("elfpak.toml");
        std::fs::write(
            &config_path,
            r#"
[package]
binary = "target/release/server"
oci_layout = "dist/image"
oci_archive = "dist/image.tar"

[image]
tag = "ci-test"
entrypoint = ["/app/server", "--serve"]
cmd = ["--listen", "0.0.0.0:8080"]
working_dir = "/app"
env = { RUST_LOG = "info", EMPTY = "" }
labels = { "org.example.revision" = "abc123" }
"#,
        )
        .unwrap();

        let config = Config::load(&config_path).unwrap();
        assert_eq!(config.package.oci_layout, Some(project.join("dist/image")));
        assert_eq!(
            config.package.oci_archive,
            Some(project.join("dist/image.tar"))
        );
        assert_eq!(config.image.tag.as_deref(), Some("ci-test"));
        assert_eq!(config.image.entrypoint, ["/app/server", "--serve"]);
        assert_eq!(config.image.cmd, ["--listen", "0.0.0.0:8080"]);
        assert_eq!(config.image.working_dir, Some(PathBuf::from("/app")));
        assert_eq!(config.image.env["RUST_LOG"], "info");
        assert_eq!(config.image.env["EMPTY"], "");
        assert_eq!(config.image.labels["org.example.revision"], "abc123");
    }
}
