//! Where a bundle comes from and where it goes. Command line first, then the
//! configuration file, then the documented default.

use crate::{cli::BundleArgs, config::Config};
use elfpak_core::{Error, manifest::MANIFEST_NAME_DEFAULT};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Debug)]
pub(crate) struct BundleInput {
    pub(crate) binary: PathBuf,
    pub(crate) install: PathBuf,
}

#[derive(Debug)]
pub(crate) struct Paths {
    pub(crate) inputs: Vec<BundleInput>,
    pub(crate) root: PathBuf,
    pub(crate) output: Option<PathBuf>,
    pub(crate) tar: Option<PathBuf>,
    pub(crate) oci_layout: Option<PathBuf>,
    pub(crate) oci_archive: Option<PathBuf>,
}

impl Paths {
    pub(crate) fn resolve(args: &BundleArgs, config: &Config) -> anyhow::Result<Paths> {
        let binaries = resolve_binaries(args, config)?;
        let output = args
            .output
            .clone()
            .or_else(|| config.package.output.clone());
        let tar = args.tar.clone().or_else(|| config.package.tar.clone());
        let oci_layout = args
            .oci_layout
            .clone()
            .or_else(|| config.package.oci_layout.clone());
        let oci_archive = args
            .oci_archive
            .clone()
            .or_else(|| config.package.oci_archive.clone());
        if output.is_none() && tar.is_none() && oci_layout.is_none() && oci_archive.is_none() {
            return Err(Error::Config {
                message: "no output given (pass --output <dir>, --tar <file>, --oci-layout <dir>, and/or --oci-archive <file>)".to_string(),
            }
            .into());
        }
        let inputs = resolve_inputs(args, config, binaries)?;

        Ok(Paths {
            inputs,
            root: args
                .root
                .clone()
                .or_else(|| config.package.root.clone())
                .unwrap_or_else(|| PathBuf::from("/")),
            output,
            tar,
            oci_layout,
            oci_archive,
        })
    }
}

fn resolve_binaries(args: &BundleArgs, config: &Config) -> anyhow::Result<Vec<PathBuf>> {
    if config.package.binary.is_some() && !config.package.binaries.is_empty() {
        return Err(Error::Config {
            message: "package.binary and package.binaries cannot both be set".to_string(),
        }
        .into());
    }
    if !args.binaries.is_empty() {
        return Ok(args.binaries.clone());
    }
    let binaries = if !config.package.binaries.is_empty() {
        config.package.binaries.clone()
    } else {
        config.package.binary.iter().cloned().collect()
    };
    if binaries.is_empty() {
        return Err(Error::Config {
            message: "no binary given (pass one or more arguments or set package.binary/package.binaries)"
                .to_string(),
        }
        .into());
    }
    Ok(binaries)
}

fn resolve_inputs(
    args: &BundleArgs,
    config: &Config,
    binaries: Vec<PathBuf>,
) -> anyhow::Result<Vec<BundleInput>> {
    if config.package.install.is_some() && config.package.install_dir.is_some() {
        return Err(Error::Config {
            message: "package.install and package.install_dir cannot both be set".to_string(),
        }
        .into());
    }
    let (install, install_dir) = if args.install.is_some() || args.install_dir.is_some() {
        (args.install.clone(), args.install_dir.clone())
    } else {
        (
            config.package.install.clone(),
            config.package.install_dir.clone(),
        )
    };
    if binaries.len() > 1 && install.is_some() {
        return Err(Error::Config {
            message: "--install names one executable; use --install-dir for multiple binaries"
                .to_string(),
        }
        .into());
    }

    let install_dir = install_dir
        .as_deref()
        .map(elfpak_core::paths::normalize_absolute)
        .unwrap_or_else(|| PathBuf::from("/"));
    let mut destinations = BTreeMap::<PathBuf, PathBuf>::new();
    let mut inputs = Vec::with_capacity(binaries.len());
    for binary in binaries {
        if binary.as_os_str().is_empty() {
            return Err(Error::Config {
                message: "binary path cannot be empty".to_string(),
            }
            .into());
        }
        let name = binary
            .file_name()
            .filter(|name| !name.is_empty())
            .ok_or_else(|| Error::Config {
                message: format!("binary path `{}` has no file name", binary.display()),
            })?;
        let destination = install
            .as_deref()
            .map(elfpak_core::paths::normalize_absolute)
            .unwrap_or_else(|| elfpak_core::paths::normalize_absolute(&install_dir.join(name)));
        if let Some(existing) = destinations.insert(destination.clone(), binary.clone()) {
            return Err(Error::Config {
                message: format!(
                    "binaries `{}` and `{}` both install as `{}`; select distinct names",
                    existing.display(),
                    binary.display(),
                    destination.display()
                ),
            }
            .into());
        }
        inputs.push(BundleInput {
            binary,
            install: destination,
        });
    }
    Ok(inputs)
}

/// Beside the rootfs, or beside the archive when only a tar was asked for.
pub(crate) fn manifest_path(args: &BundleArgs, paths: &Paths) -> Option<PathBuf> {
    if args.no_manifest {
        return None;
    }
    if let Some(explicit) = &args.manifest {
        return Some(explicit.clone());
    }
    let beside = paths
        .output
        .clone()
        .or_else(|| paths.tar.clone())
        .or_else(|| paths.oci_layout.clone())
        .or_else(|| paths.oci_archive.clone())
        .unwrap_or_else(|| PathBuf::from("."));
    Some(manifest_path_default(&beside))
}

/// The manifest sits beside the bundle: a rootfs contains only what the plan
/// put there.
fn manifest_path_default(output: &Path) -> PathBuf {
    match output.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(MANIFEST_NAME_DEFAULT),
        _ => PathBuf::from(MANIFEST_NAME_DEFAULT),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Command};
    use clap::Parser;

    fn args(values: &[&str]) -> BundleArgs {
        let cli = Cli::try_parse_from(
            ["elfpak", "bundle", "/bin/app"]
                .into_iter()
                .chain(values.iter().copied()),
        )
        .unwrap();
        match cli.command {
            Command::Bundle(bundle) => bundle.into_bundle(),
            _ => unreachable!(),
        }
    }

    #[test]
    fn oci_only_outputs_are_accepted_and_cli_paths_take_precedence() {
        let config = Config::parse(
            "[package]\noci_layout = 'configured-layout'\noci_archive = 'configured.tar'\n",
        )
        .unwrap();
        let args = args(&["--oci-layout", "cli-layout", "--oci-archive", "cli.tar"]);
        let paths = Paths::resolve(&args, &config).unwrap();
        assert_eq!(paths.oci_layout, Some(PathBuf::from("cli-layout")));
        assert_eq!(paths.oci_archive, Some(PathBuf::from("cli.tar")));
        assert!(paths.output.is_none());
        assert!(paths.tar.is_none());
    }

    #[test]
    fn default_manifest_location_uses_the_first_output_kind() {
        let args = args(&[]);
        let paths = Paths {
            inputs: Vec::new(),
            root: PathBuf::from("/"),
            output: None,
            tar: None,
            oci_layout: Some(PathBuf::from("dist/layout")),
            oci_archive: Some(PathBuf::from("elsewhere/archive.tar")),
        };
        assert_eq!(
            manifest_path(&args, &paths),
            Some(PathBuf::from("dist").join(MANIFEST_NAME_DEFAULT))
        );
    }
}
