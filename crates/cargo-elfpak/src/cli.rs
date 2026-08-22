use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "cargo",
    bin_name = "cargo",
    version,
    about = "Package Cargo binaries and their Linux runtime closures"
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: CargoCommand,

    /// Suppress all output except errors
    #[arg(short, long, global = true)]
    pub(crate) quiet: bool,

    /// Increase elfpak verbosity (-v, -vv)
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub(crate) verbose: u8,
}

#[derive(Debug, Subcommand)]
pub(crate) enum CargoCommand {
    /// Package Cargo project binaries with elfpak
    Elfpak(ElfpakArgs),
}

#[derive(Debug, Args)]
pub(crate) struct ElfpakArgs {
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Build Cargo binaries and package their Linux runtime closures
    Bundle(CargoBundleArgs),
}

#[derive(Debug, Args)]
pub(crate) struct CargoBundleArgs {
    /// Select a package from the workspace
    #[arg(
        short = 'p',
        long = "package",
        value_name = "PACKAGE",
        conflicts_with = "all"
    )]
    pub(crate) package: Option<String>,

    /// Select the binary target to package
    #[arg(
        long = "bin",
        value_name = "NAME",
        conflicts_with_all = ["bins", "all_bins", "all"]
    )]
    pub(crate) bin: Option<String>,

    /// Select comma-separated binary targets from one package
    #[arg(
        long = "bins",
        value_name = "NAMES",
        value_delimiter = ',',
        action = clap::ArgAction::Append,
        conflicts_with_all = ["bin", "all_bins", "all"]
    )]
    pub(crate) bins: Vec<String>,

    /// Select every binary target from one package
    #[arg(long, conflicts_with_all = ["bin", "bins", "all"])]
    pub(crate) all_bins: bool,

    /// Select every binary target from every workspace package
    #[arg(
        long,
        conflicts_with_all = ["package", "bin", "bins", "all_bins"]
    )]
    pub(crate) all: bool,

    /// Build artifacts in release mode
    #[arg(long, conflicts_with = "profile")]
    pub(crate) release: bool,

    /// Build artifacts with the named Cargo profile
    #[arg(long, value_name = "NAME")]
    pub(crate) profile: Option<String>,

    /// Build for the target triple
    #[arg(long, value_name = "TRIPLE")]
    pub(crate) target: Option<String>,

    /// Directory for Cargo build artifacts
    #[arg(long, value_name = "DIR")]
    pub(crate) target_dir: Option<PathBuf>,

    /// Path to Cargo.toml
    #[arg(long, value_name = "PATH")]
    pub(crate) manifest_path: Option<PathBuf>,

    /// Space- or comma-separated features to activate
    #[arg(long, value_name = "FEATURES", action = clap::ArgAction::Append)]
    pub(crate) features: Vec<String>,

    /// Activate all available features
    #[arg(long)]
    pub(crate) all_features: bool,

    /// Do not activate the `default` feature
    #[arg(long)]
    pub(crate) no_default_features: bool,

    /// Require Cargo.lock to be up to date
    #[arg(long)]
    pub(crate) locked: bool,

    /// Run Cargo without accessing the network
    #[arg(long)]
    pub(crate) offline: bool,

    /// Require Cargo.lock and cached dependencies
    #[arg(long)]
    pub(crate) frozen: bool,

    #[command(flatten)]
    pub(crate) bundle: elfpak::BundleArgs,
}
