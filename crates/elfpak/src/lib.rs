//! `elfpak` command line interface.
//!
//! This crate only parses arguments, loads configuration, calls `elfpak-core`
//! and renders the result. No resolution logic lives here.
//!
//! One module per subcommand, plus [`cli`] for the argument definitions and
//! [`render`] for everything written to a terminal.

mod bundle;
mod cli;
mod config;
mod inspect;
mod render;
mod verify;

use crate::{
    cli::{Cli, Command},
    render::Verbosity,
};
use clap::Parser;
use elfpak_core::Error;
use std::{io::Write, path::PathBuf};

pub use cli::BundleArgs;
/// Re-exported so adapters can classify paths the same way this crate does.
pub use elfpak_core::paths;

/// Run the command line: parse, dispatch, and turn the outcome into an exit
/// code.
pub fn run() -> std::process::ExitCode {
    let cli = Cli::parse();
    let verbosity = Verbosity::new(cli.quiet, cli.verbose);

    let result = match cli.command {
        Command::Inspect(args) => inspect::run(&args, verbosity),
        Command::Bundle(args) => bundle::run(&(*args).into_bundle(), verbosity),
        Command::Verify(args) => verify::run(&args, verbosity),
    };

    finish(result)
}

/// Run `elfpak bundle` with a binary selected by an embedding adapter.
///
/// The supplied binary takes precedence over `elfpak.toml`; every other bundle
/// option retains the standalone command's normal precedence rules.
pub fn run_bundle(
    args: BundleArgs,
    binary: PathBuf,
    quiet: bool,
    verbose: u8,
) -> std::process::ExitCode {
    run_bundle_many(args, vec![binary], quiet, verbose)
}

/// Run `elfpak bundle` with binaries selected by an embedding adapter.
///
/// The supplied binaries take precedence over `elfpak.toml`; every other
/// bundle option retains the standalone command's normal precedence rules.
pub fn run_bundle_many(
    mut args: BundleArgs,
    binaries: Vec<PathBuf>,
    quiet: bool,
    verbose: u8,
) -> std::process::ExitCode {
    args.binaries = binaries;
    finish(bundle::run(&args, Verbosity::new(quiet, verbose)))
}

fn finish(result: anyhow::Result<()>) -> std::process::ExitCode {
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            report(&err);
            std::process::ExitCode::FAILURE
        }
    }
}

/// A core error is rendered with its diagnostic code; anything else is printed
/// as the context chain `anyhow` collected.
fn report(err: &anyhow::Error) {
    let mut stderr = std::io::stderr();
    let _ = match err.downcast_ref::<Error>() {
        Some(core) => write!(stderr, "{}", render::error(core)),
        None => writeln!(stderr, "error:\n  {err:#}"),
    };
}
