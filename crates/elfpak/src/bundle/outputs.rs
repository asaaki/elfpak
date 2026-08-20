//! Phase two: materialize the plan.

use crate::{bundle::paths::Paths, cli::BundleArgs};
use elfpak_core::{BundlePlan, Manifest, RootFsBuilder, RootFsReport, TarBuilder, TarReport};
use std::path::Path;

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
    assert_eq!(outputs.rootfs.is_some(), paths.output.is_some());
    assert_eq!(outputs.tar.is_some(), paths.tar.is_some());
    Ok(outputs)
}
