//! Argument definitions. All real work happens in `elfpak-core`.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "elfpak",
    version,
    about = "Package a Linux ELF application and its runtime closure into a minimal rootfs",
    long_about = None,
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Suppress all output except errors
    #[arg(short, long, global = true)]
    pub quiet: bool,

    /// Increase verbosity (-v, -vv)
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Analyze an executable and print its runtime closure without copying files
    Inspect(InspectArgs),
    /// Build a minimal rootfs for an executable
    Bundle(BundleArgs),
    /// Check a materialized rootfs against a manifest
    Verify(VerifyArgs),
}

#[derive(Debug, Args)]
pub struct InspectArgs {
    /// Executable to analyze
    pub binary: PathBuf,

    /// Sysroot used as the logical `/` for dependency lookup
    #[arg(long, default_value = "/")]
    pub root: PathBuf,

    /// Extra library search directory (like LD_LIBRARY_PATH)
    #[arg(long = "library-path", value_name = "DIR")]
    pub library_paths: Vec<PathBuf>,

    /// Emit the plan as JSON instead of text
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct BundleArgs {
    /// Executable to package
    pub binary: Option<PathBuf>,

    /// Directory the rootfs is written to
    #[arg(short, long)]
    pub output: Option<PathBuf>,

    /// Also write the rootfs as a deterministic tar archive
    #[arg(long, value_name = "FILE")]
    pub tar: Option<PathBuf>,

    /// Path of the executable inside the rootfs
    #[arg(long)]
    pub install: Option<PathBuf>,

    /// Sysroot used as the logical `/` for dependency lookup
    #[arg(long)]
    pub root: Option<PathBuf>,

    /// Runtime policy preset
    #[arg(long)]
    pub preset: Option<elfpak_core::Preset>,

    /// Additional path to copy into the rootfs, preserving its location
    #[arg(long = "include", value_name = "PATH")]
    pub includes: Vec<PathBuf>,

    /// Restrict the runtime closure to the given sonames (repeatable)
    #[arg(long = "allow-library", value_name = "SONAME")]
    pub allow_library: Vec<String>,

    /// Identity the application runs as, as `uid[:gid]` or `name:uid:gid`
    #[arg(long)]
    pub user: Option<String>,

    /// Include a CA certificate bundle
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    pub ca_certificates: Option<bool>,

    /// Create /tmp
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    pub tmp: Option<bool>,

    /// Generate /etc/passwd and /etc/group
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    pub passwd_group: Option<bool>,

    /// Generate /etc/nsswitch.conf and include NSS modules when present
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    pub nsswitch: Option<bool>,

    /// Include the timezone database
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    pub tzdata: Option<bool>,

    /// Extra library search directory (like LD_LIBRARY_PATH)
    #[arg(long = "library-path", value_name = "DIR")]
    pub library_paths: Vec<PathBuf>,

    /// Where to write the manifest (default: `elfpak-manifest.json` beside the rootfs)
    #[arg(long)]
    pub manifest: Option<PathBuf>,

    /// Do not write a manifest
    #[arg(long, conflicts_with = "manifest")]
    pub no_manifest: bool,

    /// Remove an existing output directory first
    #[arg(long)]
    pub clean: bool,

    /// Plan and validate without writing anything
    #[arg(long)]
    pub dry_run: bool,

    /// Path to an elfpak.toml (default: ./elfpak.toml when present)
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// Ignore any elfpak.toml
    #[arg(long, conflicts_with = "config")]
    pub no_config: bool,
}

#[derive(Debug, Args)]
pub struct VerifyArgs {
    /// Manifest emitted by `elfpak bundle`
    pub manifest: PathBuf,

    /// Rootfs to check (default: the path recorded in the manifest)
    #[arg(long)]
    pub rootfs: Option<PathBuf>,

    /// Also fail on files that are present but not listed in the manifest
    #[arg(long)]
    pub strict: bool,
}
