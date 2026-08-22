//! Phase two: materialize the plan.

use crate::{bundle::paths::Paths, cli::BundleArgs};
use elfpak_core::{
    BundlePlan, Error, Manifest, RootFsBuilder, RootFsReport, TarBuilder, TarReport,
};
use std::path::{Path, PathBuf};

/// What was actually materialized, as opposed to only planned.
#[derive(Debug, Default)]
pub(crate) struct Outputs {
    pub(crate) rootfs: Option<RootFsReport>,
    pub(crate) tar: Option<TarReport>,
    pub(crate) written: bool,
}

pub(crate) fn write_outputs(
    args: &BundleArgs,
    paths: &Paths,
    plan: &BundlePlan,
    manifest_path: Option<&Path>,
) -> anyhow::Result<Outputs> {
    validate_output_layout(paths, manifest_path)?;
    let mut outputs = Outputs::default();
    if let Some(output) = &paths.output {
        outputs.rootfs = Some(RootFsBuilder::new(output).clean(args.clean).apply(plan)?);
    }
    if let Some(tar) = &paths.tar {
        outputs.tar = Some(TarBuilder::new(tar).apply(plan)?);
    }
    if let Some(path) = manifest_path {
        let manifest = Manifest::from_plan_with_outputs(
            plan,
            &paths.root,
            paths.output.as_deref(),
            paths.tar.as_deref(),
        );
        manifest.write(path)?;
    }
    outputs.written = true;
    Ok(outputs)
}

/// A rootfs is a directory tree. Publishing a tar or manifest inside it would
/// either add an unplanned file to the bundle or be overwritten during a
/// subsequent rootfs replacement. Keep every requested artifact separate.
fn validate_output_layout(paths: &Paths, manifest: Option<&Path>) -> anyhow::Result<()> {
    let output = paths.output.as_deref().map(absolute_path).transpose()?;
    let tar = paths.tar.as_deref().map(absolute_path).transpose()?;
    let manifest = manifest.map(absolute_path).transpose()?;

    if let (Some(output), Some(tar)) = (&output, &tar)
        && (output == tar || tar.starts_with(output))
    {
        return Err(output_layout_error(
            "tar output must not be inside or equal to --output",
        ));
    }
    if let (Some(output), Some(manifest)) = (&output, &manifest)
        && (output == manifest || manifest.starts_with(output))
    {
        return Err(output_layout_error(
            "manifest output must not be inside or equal to --output",
        ));
    }
    if let (Some(tar), Some(manifest)) = (&tar, &manifest)
        && tar == manifest
    {
        return Err(output_layout_error(
            "tar and manifest outputs must be different paths",
        ));
    }
    Ok(())
}

fn absolute_path(path: &Path) -> anyhow::Result<PathBuf> {
    std::path::absolute(path)
        .map_err(|error| Error::Config {
            message: format!("cannot resolve output path `{}`: {error}", path.display()),
        })
        // `absolute` deliberately preserves `.` and `..`; output layout
        // comparisons must not, or aliases can bypass containment checks.
        .map(|path| elfpak_core::paths::normalize_absolute(&path))
        .map_err(Into::into)
}

fn output_layout_error(message: &str) -> anyhow::Error {
    Error::Config {
        message: message.to_string(),
    }
    .into()
}
