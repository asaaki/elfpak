//! Where a bundle comes from and where it goes. Command line first, then the
//! configuration file, then the documented default.

use crate::{cli::BundleArgs, config::Config};
use elfpak_core::{Error, manifest::MANIFEST_NAME_DEFAULT};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub(crate) struct Paths {
    pub(crate) binary: PathBuf,
    pub(crate) install: PathBuf,
    pub(crate) root: PathBuf,
    pub(crate) output: Option<PathBuf>,
    pub(crate) tar: Option<PathBuf>,
}

impl Paths {
    pub(crate) fn resolve(args: &BundleArgs, config: &Config) -> anyhow::Result<Paths> {
        let binary = args
            .binary
            .clone()
            .or_else(|| config.package.binary.clone())
            .ok_or_else(|| Error::Config {
                message: "no binary given (pass one as an argument or set package.binary)"
                    .to_string(),
            })?;
        let output = args
            .output
            .clone()
            .or_else(|| config.package.output.clone());
        let tar = args.tar.clone().or_else(|| config.package.tar.clone());
        if output.is_none() && tar.is_none() {
            return Err(Error::Config {
                message: "no output given (pass --output <dir> and/or --tar <file>)".to_string(),
            }
            .into());
        }
        if binary.as_os_str().is_empty() {
            return Err(Error::Config {
                message: "binary path cannot be empty".to_string(),
            }
            .into());
        }

        Ok(Paths {
            install: args
                .install
                .clone()
                .or_else(|| config.package.install.clone())
                .unwrap_or_else(|| PathBuf::from("/").join(binary.file_name().unwrap_or_default())),
            binary,
            root: args
                .root
                .clone()
                .or_else(|| config.package.root.clone())
                .unwrap_or_else(|| PathBuf::from("/")),
            output,
            tar,
        })
    }
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
