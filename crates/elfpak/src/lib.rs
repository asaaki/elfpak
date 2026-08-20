//! `elfpak` command line interface.
//!
//! This crate only parses arguments, loads configuration, calls `elfpak-core`
//! and renders the result. No resolution logic lives here.
//!
//! One module per subcommand, plus [`cli`] for the argument definitions and
//! [`render`] for everything written to a terminal.

mod bundle;
mod cli;
mod inspect;
mod render;
mod verify;

use crate::{
    cli::{Cli, Command},
    render::Verbosity,
};
use clap::Parser;
use elfpak_core::Error;
use std::io::Write;

/// Run the command line: parse, dispatch, and turn the outcome into an exit
/// code.
pub fn run() -> std::process::ExitCode {
    let cli = Cli::parse();
    let verbosity = Verbosity::new(cli.quiet, cli.verbose);

    let result = match &cli.command {
        Command::Inspect(args) => inspect::run(args, verbosity),
        Command::Bundle(args) => bundle::run(args, verbosity),
        Command::Verify(args) => verify::run(args, verbosity),
    };

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
