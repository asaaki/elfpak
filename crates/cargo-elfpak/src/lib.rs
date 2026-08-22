//! Cargo project adapter for `elfpak`.

mod build;
mod cli;
mod metadata;

use crate::{
    build::BuildRequest,
    cli::{CargoCommand, Cli, Command},
    metadata::SelectionContext,
};
use anyhow::Result;
use clap::Parser;
use std::{ffi::OsString, path::PathBuf};

/// Run the Cargo subcommand and return a process exit code.
pub fn run() -> std::process::ExitCode {
    let mut args: Vec<OsString> = std::env::args_os().collect();
    if args.get(1).is_none_or(|argument| argument != "elfpak") {
        args.insert(1, OsString::from("elfpak"));
    }
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(error) => error.exit(),
    };

    match try_run(cli) {
        Ok(status) => status,
        Err(error) => {
            eprintln!("error:\n  {error:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn try_run(cli: Cli) -> Result<std::process::ExitCode> {
    let CargoCommand::Elfpak(elfpak) = cli.command;
    let Command::Bundle(args) = elfpak.command;
    let current_dir = std::env::current_dir()?;
    let metadata = metadata::load(
        &current_dir,
        args.manifest_path.as_deref(),
        args.locked,
        args.offline,
        args.frozen,
    )?;
    let selection = metadata::select(
        &metadata,
        &SelectionContext {
            package: args.package,
            binary: args.bin,
            manifest_path: args.manifest_path.clone(),
            current_dir: PathBuf::from(&current_dir),
        },
    )?;
    let artifact = build::run(&BuildRequest {
        selection: &selection,
        current_dir: &current_dir,
        manifest_path: args.manifest_path.as_deref(),
        release: args.release,
        profile: args.profile.as_deref(),
        target: args.target.as_deref(),
        target_dir: args.target_dir.as_deref(),
        features: &args.features,
        all_features: args.all_features,
        no_default_features: args.no_default_features,
        locked: args.locked,
        offline: args.offline,
        frozen: args.frozen,
        quiet: cli.quiet,
    })?;

    if !cli.quiet {
        let state = if artifact.fresh { "fresh" } else { "built" };
        println!("{state} Cargo binary: {}", artifact.executable.display());
    }
    Ok(elfpak::run_bundle(
        args.bundle,
        artifact.executable,
        cli.quiet,
        cli.verbose,
    ))
}
