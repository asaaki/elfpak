//! `elfpak bundle`: plan, then write.
//!
//! The command itself is here; the pieces it assembles from arguments and
//! configuration are one module each.

mod outputs;
mod paths;
mod policy;

use crate::{
    bundle::{
        outputs::write_outputs,
        paths::{Paths, manifest_path},
        policy::{dependency_policy, runtime_policy},
    },
    cli::BundleArgs,
    render::{self, Verbosity},
};
use elfpak_core::{Planner, SourceRoot, config::Config};
pub(crate) use outputs::Outputs;
use std::path::PathBuf;

pub(crate) fn run(args: &BundleArgs, verbosity: Verbosity) -> anyhow::Result<()> {
    let config = load_config(args, verbosity)?;
    let paths = Paths::resolve(args, &config)?;
    let preset = args.preset.or(config.runtime.preset);

    let mut planner = Planner::new(SourceRoot::new(&paths.root), &paths.binary);
    if let Some(preset) = preset {
        planner = planner.preset(preset);
    }
    let plan = planner
        .install_as(&paths.install)
        .runtime_policy(runtime_policy(args, &config, preset)?)
        .dependency_policy(dependency_policy(args, &config))
        .library_paths(args.library_paths.clone())
        .plan()?;

    let manifest_path = manifest_path(args, &paths);
    let outputs = if args.dry_run {
        Outputs::default()
    } else {
        write_outputs(args, &paths, &plan, manifest_path.as_deref())?
    };

    verbosity.print(|out| {
        render::bundle_summary(
            out,
            &paths.binary,
            &plan,
            render::Destinations {
                rootfs: paths.output.as_deref(),
                tar: paths.tar.as_deref(),
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
