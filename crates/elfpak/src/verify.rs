//! `elfpak verify`: check a materialized rootfs against its manifest.

use crate::{cli::VerifyArgs, render::Verbosity};
use elfpak_core::{
    Error,
    manifest::{Manifest, VerifyOptions},
};
use std::path::PathBuf;

pub(crate) fn run(args: &VerifyArgs, verbosity: Verbosity) -> anyhow::Result<()> {
    let manifest = Manifest::load(&args.manifest)?;
    let rootfs = args
        .rootfs
        .clone()
        .or_else(|| manifest.rootfs.clone().map(PathBuf::from))
        .ok_or_else(|| Error::Config {
            message: "manifest does not record a rootfs; pass --rootfs".to_string(),
        })?;

    let options = VerifyOptions {
        strict: args.strict,
    };
    let report = manifest.verify(&rootfs, &options);
    assert_eq!(report.checked as usize, manifest.files.len());

    if report.is_ok() {
        verbosity.print(|out| {
            writeln!(
                out,
                "ok: {} entries verified in {}{}",
                report.checked,
                rootfs.display(),
                if options.strict { " (strict)" } else { "" }
            )
        });
        return Ok(());
    }

    for problem in &report.problems {
        eprintln!("  {}: {}", problem.path, problem.detail);
    }
    Err(Error::VerifyFailed {
        checked: report.checked,
        failures: report.failure_count(),
    }
    .into())
}
