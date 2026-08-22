//! `elfpak bundle`: plan, then write.
//!
//! The command itself is here; the pieces it assembles from arguments and
//! configuration are one module each.

mod image;
mod outputs;
pub(crate) mod paths;
mod policy;

use crate::{
    bundle::{
        outputs::write_outputs,
        paths::{Paths, manifest_path},
        policy::{dependency_policy, runtime_policy},
    },
    cli::BundleArgs,
    config::Config,
    render::{self, Verbosity},
};
use elfpak_core::{Planner, SourceRoot};
pub(crate) use outputs::Outputs;
use std::path::PathBuf;

pub(crate) fn run(args: &BundleArgs, verbosity: Verbosity) -> anyhow::Result<()> {
    let config = load_config(args, verbosity)?;
    let paths = Paths::resolve(args, &config)?;
    let preset = args.preset.or(config.runtime.preset);

    let first = paths
        .inputs
        .first()
        .expect("path resolution always produces at least one input");
    let mut planner =
        Planner::new(SourceRoot::new(&paths.root), &first.binary).install_as(&first.install);
    for input in &paths.inputs[1..] {
        planner = planner.add_binary(&input.binary, &input.install);
    }
    if let Some(preset) = preset {
        planner = planner.preset(preset);
    }
    let plan = planner
        .runtime_policy(runtime_policy(args, &config, preset)?)
        .dependency_policy(dependency_policy(args, &config))
        .library_paths(args.library_paths.clone())
        .plan()?;

    let image = if paths.oci_layout.is_some() || paths.oci_archive.is_some() {
        let image = image::resolve(args, &config)?;
        // Validate now, so a bad tag or entrypoint fails before anything is
        // written. The builders resolve it again from the same inputs.
        image.resolve(&plan)?;
        Some(image)
    } else {
        // Nothing would consume it, and silently dropping an entrypoint or tag
        // leaves a pipeline believing it configured an image it never built.
        if image::was_requested_on_the_command_line(args) {
            anyhow::bail!(elfpak_core::Error::Config {
                message: "image options need --oci-layout or --oci-archive; \
                          no OCI destination was given"
                    .to_string(),
            });
        }
        None
    };

    let manifest_path = manifest_path(args, &paths);
    outputs::validate_output_layout(&paths, manifest_path.as_deref())?;
    let outputs = if args.dry_run {
        Outputs::default()
    } else {
        write_outputs(
            args,
            &paths,
            &plan,
            image.as_ref(),
            manifest_path.as_deref(),
        )?
    };

    verbosity.print(|out| {
        render::bundle_summary(
            out,
            &paths.inputs,
            &plan,
            render::Destinations {
                rootfs: paths.output.as_deref(),
                tar: paths.tar.as_deref(),
                oci_layout: paths.oci_layout.as_deref(),
                oci_archive: paths.oci_archive.as_deref(),
                manifest: manifest_path.as_deref(),
            },
            &outputs,
            verbosity.level(),
        )
    });
    Ok(())
}

fn load_config(args: &BundleArgs, verbosity: Verbosity) -> anyhow::Result<Config> {
    if args.no_config {
        return Ok(Config::default());
    }
    if let Some(path) = &args.config {
        verbosity.note(format!("using config {}", path.display()));
        return Ok(Config::load(path)?);
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    match Config::discover(&cwd)? {
        Some((path, config)) => {
            verbosity.note(format!("using config {}", path.display()));
            Ok(config)
        }
        None => Ok(Config::default()),
    }
}
