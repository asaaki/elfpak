//! Argument definitions. All real work happens in `elfpak-core`.

use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "elfpak",
    version,
    about = "Package a Linux ELF application and its runtime closure into a minimal rootfs",
    long_about = None,
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Command,

    /// Suppress all output except errors
    #[arg(short, long, global = true)]
    pub(crate) quiet: bool,

    /// Increase verbosity (-v, -vv)
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub(crate) verbose: u8,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Analyze an executable and print its runtime closure without copying files
    Inspect(InspectArgs),
    /// Build a minimal rootfs for an executable
    Bundle(BundleArgs),
    /// Check a materialized rootfs against a manifest
    Verify(VerifyArgs),
}

#[derive(Debug, Args)]
pub(crate) struct InspectArgs {
    /// Executable to analyze
    pub(crate) binary: PathBuf,

    /// Sysroot used as the logical `/` for dependency lookup
    #[arg(long, default_value = "/")]
    pub(crate) root: PathBuf,

    /// Extra library search directory (like LD_LIBRARY_PATH)
    #[arg(long = "library-path", value_name = "DIR")]
    pub(crate) library_paths: Vec<PathBuf>,

    /// Emit the plan as JSON instead of text
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(Debug, Args)]
pub(crate) struct BundleArgs {
    /// Executable to package
    pub(crate) binary: Option<PathBuf>,

    /// Directory the rootfs is written to
    #[arg(short, long)]
    pub(crate) output: Option<PathBuf>,

    /// Also write the rootfs as a deterministic tar archive
    #[arg(long, value_name = "FILE")]
    pub(crate) tar: Option<PathBuf>,

    /// Path of the executable inside the rootfs
    #[arg(long)]
    pub(crate) install: Option<PathBuf>,

    /// Sysroot used as the logical `/` for dependency lookup
    #[arg(long)]
    pub(crate) root: Option<PathBuf>,

    /// Runtime policy preset
    #[arg(long)]
    pub(crate) preset: Option<elfpak_core::Preset>,

    /// Additional path to copy into the rootfs, preserving its location
    #[arg(long = "include", value_name = "PATH")]
    pub(crate) includes: Vec<PathBuf>,

    /// Restrict the runtime closure to the given sonames (repeatable)
    #[arg(long = "allow-library", value_name = "SONAME")]
    pub(crate) allow_library: Vec<String>,

    /// Identity the application runs as, as `uid[:gid]` or `name:uid:gid`
    #[arg(long)]
    pub(crate) user: Option<String>,

    /// Include a CA certificate bundle
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    pub(crate) ca_certificates: Option<bool>,

    /// Create /tmp
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    pub(crate) tmp: Option<bool>,

    /// Generate /etc/passwd and /etc/group
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    pub(crate) passwd_group: Option<bool>,

    /// Generate /etc/nsswitch.conf and include NSS modules when present
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    pub(crate) nsswitch: Option<bool>,

    /// Include the timezone database
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    pub(crate) tzdata: Option<bool>,

    /// Generate /etc/ld.so.cache (default: only when the closure needs one)
    #[arg(
        long = "ld-so-cache",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
    )]
    pub(crate) ld_so_cache: Option<bool>,

    /// Extra library search directory (like LD_LIBRARY_PATH)
    #[arg(long = "library-path", value_name = "DIR")]
    pub(crate) library_paths: Vec<PathBuf>,

    /// Where to write the manifest (default: `elfpak-manifest.json` beside the rootfs)
    #[arg(long)]
    pub(crate) manifest: Option<PathBuf>,

    /// Do not write a manifest
    #[arg(long, conflicts_with = "manifest")]
    pub(crate) no_manifest: bool,

    /// Remove an existing output directory first
    #[arg(long)]
    pub(crate) clean: bool,

    /// Plan and validate without writing anything
    #[arg(long)]
    pub(crate) dry_run: bool,

    /// Path to an elfpak.toml (default: ./elfpak.toml when present)
    #[arg(long)]
    pub(crate) config: Option<PathBuf>,

    /// Ignore any elfpak.toml
    #[arg(long, conflicts_with = "config")]
    pub(crate) no_config: bool,
}

#[derive(Debug, Args)]
pub(crate) struct VerifyArgs {
    /// Manifest emitted by `elfpak bundle`
    pub(crate) manifest: PathBuf,

    /// Rootfs to check (default: the path recorded in the manifest)
    #[arg(long)]
    pub(crate) rootfs: Option<PathBuf>,

    /// Also fail on files that are present but not listed in the manifest
    #[arg(long)]
    pub(crate) strict: bool,
}
