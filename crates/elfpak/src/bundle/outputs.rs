//! Phase two: materialize the plan.

use crate::{bundle::paths::Paths, cli::BundleArgs};
use elfpak_core::{
    BundlePlan, Error, Manifest, ManifestImage, ManifestOutputs, OciArchiveBuilder, OciImageConfig,
    OciLayoutBuilder, OciReport, RootFsBuilder, RootFsReport, TarBuilder, TarReport,
};
use std::path::{Path, PathBuf};

/// What was actually materialized, as opposed to only planned.
#[derive(Debug, Default)]
pub(crate) struct Outputs {
    pub(crate) rootfs: Option<RootFsReport>,
    pub(crate) tar: Option<TarReport>,
    pub(crate) oci_layout: Option<OciReport>,
    pub(crate) oci_archive: Option<OciReport>,
    pub(crate) written: bool,
}

pub(crate) fn write_outputs(
    args: &BundleArgs,
    paths: &Paths,
    plan: &BundlePlan,
    image: Option<&OciImageConfig>,
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
    if let Some(layout) = &paths.oci_layout {
        let image = image.expect("OCI destinations have resolved image metadata");
        outputs.oci_layout = Some(
            OciLayoutBuilder::new(layout)
                .image(image.clone())
                .apply(plan)?,
        );
    }
    if let Some(archive) = &paths.oci_archive {
        let image = image.expect("OCI destinations have resolved image metadata");
        outputs.oci_archive = Some(
            OciArchiveBuilder::new(archive)
                .image(image.clone())
                .apply(plan)?,
        );
    }
    if let (Some(layout), Some(archive)) = (&outputs.oci_layout, &outputs.oci_archive) {
        assert_eq!(layout.image(), archive.image());
        assert_eq!(layout.layer_digest(), archive.layer_digest());
        assert_eq!(layout.config_digest(), archive.config_digest());
        assert_eq!(layout.manifest_digest(), archive.manifest_digest());
    }
    if let Some(path) = manifest_path {
        let oci_report = outputs.oci_layout.as_ref().or(outputs.oci_archive.as_ref());
        let image = oci_report
            .map(|report| ManifestImage::from_oci(report.image(), report.manifest_digest()));
        let manifest = Manifest::from_plan_with_artifacts(
            plan,
            &paths.root,
            ManifestOutputs {
                rootfs: paths.output.as_deref(),
                tar: paths.tar.as_deref(),
                oci_layout: paths.oci_layout.as_deref(),
                oci_archive: paths.oci_archive.as_deref(),
            },
            image,
        );
        manifest.write(path)?;
    }
    outputs.written = true;
    Ok(outputs)
}

/// A rootfs is a directory tree. Publishing a tar or manifest inside it would
/// either add an unplanned file to the bundle or be overwritten during a
/// subsequent rootfs replacement. Keep every requested artifact separate.
pub(crate) fn validate_output_layout(paths: &Paths, manifest: Option<&Path>) -> anyhow::Result<()> {
    let directories = [
        ("--output", paths.output.as_deref()),
        ("--oci-layout", paths.oci_layout.as_deref()),
    ];
    let files = [
        ("--tar", paths.tar.as_deref()),
        ("--oci-archive", paths.oci_archive.as_deref()),
        ("--manifest", manifest),
    ];
    let directories = normalize_artifacts(&directories)?;
    let files = normalize_artifacts(&files)?;

    for (index, (left_kind, left)) in directories.iter().enumerate() {
        for (right_kind, right) in &directories[index + 1..] {
            if left.starts_with(right) || right.starts_with(left) {
                return Err(output_layout_error(&format!(
                    "{left_kind} and {right_kind} directory outputs must not overlap"
                )));
            }
        }
    }
    for (directory_kind, directory) in &directories {
        for (file_kind, file) in &files {
            if file == directory || file.starts_with(directory) {
                return Err(output_layout_error(&format!(
                    "{file_kind} must not be inside or equal to {directory_kind}"
                )));
            }
        }
    }
    for (index, (left_kind, left)) in files.iter().enumerate() {
        for (right_kind, right) in &files[index + 1..] {
            if left == right {
                return Err(output_layout_error(&format!(
                    "{left_kind} and {right_kind} outputs must be different paths"
                )));
            }
        }
    }
    Ok(())
}

fn normalize_artifacts(
    artifacts: &[(&'static str, Option<&Path>)],
) -> anyhow::Result<Vec<(&'static str, PathBuf)>> {
    artifacts
        .iter()
        .filter_map(|(kind, path)| path.map(|path| (*kind, path)))
        .map(|(kind, path)| absolute_path(path).map(|path| (kind, path)))
        .collect()
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
