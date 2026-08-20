//! `elfpak` command line interface.
//!
//! This crate only parses arguments, loads configuration, calls `elfpak-core`
//! and renders the result. No resolution logic lives here.

mod cli;
mod render;

use std::io::Write;
use std::path::{Path, PathBuf};

use clap::Parser;

use elfpak_core::config::Config;
use elfpak_core::manifest::{MANIFEST_NAME_DEFAULT, Manifest, VerifyOptions};
use elfpak_core::{
    CachePolicy, DependencyPolicy, Error, Planner, Preset, RootFsBuilder, RootFsReport,
    RuntimePolicy, SourceRoot, TarBuilder, TarReport, UserSpec,
};

use cli::{BundleArgs, Cli, Command, InspectArgs, VerifyArgs};

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let verbosity = Verbosity {
        quiet: cli.quiet,
        level: cli.verbose,
    };

    let result = match &cli.command {
        Command::Inspect(args) => inspect(args, verbosity),
        Command::Bundle(args) => bundle(args, verbosity),
        Command::Verify(args) => verify(args, verbosity),
    };

    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            let mut stderr = std::io::stderr();
            let _ = match err.downcast_ref::<Error>() {
                Some(core) => write!(stderr, "{}", render::error(core)),
                None => writeln!(stderr, "error:\n  {err:#}"),
            };
            std::process::ExitCode::FAILURE
        }
    }
}

#[derive(Clone, Copy)]
struct Verbosity {
    quiet: bool,
    level: u8,
}

impl Verbosity {
    fn print(&self, render: impl FnOnce(&mut dyn Write) -> std::io::Result<()>) {
        if self.quiet {
            return;
        }
        let mut stdout = std::io::stdout();
        let _ = render(&mut stdout);
    }

    fn note(&self, message: impl std::fmt::Display) {
        if self.quiet || self.level == 0 {
            return;
        }
        eprintln!("note: {message}");
    }
}

fn inspect(args: &InspectArgs, verbosity: Verbosity) -> anyhow::Result<()> {
    let root = SourceRoot::new(&args.root);
    let plan = Planner::new(root, &args.binary)
        .library_paths(args.library_paths.clone())
        .plan()?;
    assert!(
        !plan.files.is_empty(),
        "a plan always contains the executable"
    );

    if args.json {
        let manifest = Manifest::from_plan(&plan, &args.root, None);
        println!("{}", manifest.to_json());
        return Ok(());
    }

    verbosity.print(|out| render::inspect(out, &args.binary, &plan));
    Ok(())
}

/// `elfpak bundle`: plan, then write. All control flow of the command lives
/// here; the helpers below only compute what it needs.
fn bundle(args: &BundleArgs, verbosity: Verbosity) -> anyhow::Result<()> {
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
            verbosity.level,
        )
    });
    Ok(())
}

/// Where a bundle comes from and where it goes. Command line first, then the
/// configuration file, then the documented default.
#[derive(Debug)]
struct Paths {
    binary: PathBuf,
    install: PathBuf,
    root: PathBuf,
    output: Option<PathBuf>,
    tar: Option<PathBuf>,
}

impl Paths {
    fn resolve(args: &BundleArgs, config: &Config) -> anyhow::Result<Paths> {
        let binary = args
            .binary
            .clone()
            .or_else(|| config.package.binary.clone())
            .ok_or_else(|| Error::Config {
                message: "no binary given (pass one as an argument or set package.binary)"
                    .to_string(),
            })?;
        let output = args
            .output
            .clone()
            .or_else(|| config.package.output.clone());
        let tar = args.tar.clone().or_else(|| config.package.tar.clone());
        if output.is_none() && tar.is_none() {
            return Err(Error::Config {
                message: "no output given (pass --output <dir> and/or --tar <file>)".to_string(),
            }
            .into());
        }
        assert!(!binary.as_os_str().is_empty());
        assert!(output.is_some() || tar.is_some());

        Ok(Paths {
            install: args
                .install
                .clone()
                .or_else(|| config.package.install.clone())
                .unwrap_or_else(|| PathBuf::from("/").join(binary.file_name().unwrap_or_default())),
            binary,
            root: args
                .root
                .clone()
                .or_else(|| config.package.root.clone())
                .unwrap_or_else(|| PathBuf::from("/")),
            output,
            tar,
        })
    }
}

/// The preset, then every feature the caller switched on its own.
fn runtime_policy(
    args: &BundleArgs,
    config: &Config,
    preset: Option<Preset>,
) -> anyhow::Result<RuntimePolicy> {
    let mut policy = RuntimePolicy::from_preset(preset.unwrap_or(Preset::Minimal));

    override_flag(
        &mut policy.ca_certificates,
        args.ca_certificates.or(config.runtime.ca_certificates),
    );
    override_flag(&mut policy.tmp, args.tmp.or(config.runtime.tmp));
    override_flag(
        &mut policy.passwd_group,
        args.passwd_group.or(config.runtime.passwd_group),
    );
    override_flag(
        &mut policy.nsswitch,
        args.nsswitch.or(config.runtime.nsswitch),
    );
    override_flag(&mut policy.tzdata, args.tzdata.or(config.runtime.tzdata));

    policy.ld_so_cache = CachePolicy::from_flag(args.ld_so_cache.or(config.runtime.ld_so_cache));
    if let Some(user) = args.user.clone().or_else(|| config.runtime.user.clone()) {
        policy.user = Some(UserSpec::parse(&user)?);
    }
    // A repeated `--include` replaces the configured list rather than adding to
    // it, so that a command line can always be read on its own.
    policy.includes = if args.includes.is_empty() {
        config.include.paths.clone()
    } else {
        args.includes.clone()
    };

    // Nothing appears in a policy that no one asked for: every field above is
    // either the preset's answer or an explicit override of it.
    assert!(policy.user.is_none() || args.user.is_some() || config.runtime.user.is_some());
    assert!(policy.includes.len() <= args.includes.len() + config.include.paths.len());
    Ok(policy)
}

/// `None` leaves the preset's answer in place; a flag or config value wins.
fn override_flag(target: &mut bool, value: Option<bool>) {
    if let Some(value) = value {
        *target = value;
    }
}

fn dependency_policy(args: &BundleArgs, config: &Config) -> DependencyPolicy {
    let allow = if args.allow_library.is_empty() {
        config.dependencies.allow.clone()
    } else {
        Some(args.allow_library.clone())
    };
    match allow {
        Some(list) => DependencyPolicy::allow_list(list),
        None => DependencyPolicy::allow_all(),
    }
}

/// Beside the rootfs, or beside the archive when only a tar was asked for.
fn manifest_path(args: &BundleArgs, paths: &Paths) -> Option<PathBuf> {
    if args.no_manifest {
        return None;
    }
    if let Some(explicit) = &args.manifest {
        return Some(explicit.clone());
    }
    let beside = paths
        .output
        .clone()
        .or_else(|| paths.tar.clone())
        .unwrap_or_else(|| PathBuf::from("."));
    Some(manifest_path_default(&beside))
}

/// Phase two: materialize the plan. Nothing here decides anything.
fn write_outputs(
    args: &BundleArgs,
    paths: &Paths,
    plan: &elfpak_core::BundlePlan,
    manifest_path: Option<&Path>,
) -> anyhow::Result<Outputs> {
    let mut outputs = Outputs::default();
    if let Some(output) = &paths.output {
        outputs.rootfs = Some(RootFsBuilder::new(output).clean(args.clean).apply(plan)?);
    }
    if let Some(tar) = &paths.tar {
        outputs.tar = Some(TarBuilder::new(tar).apply(plan)?);
    }
    if let Some(path) = manifest_path {
        let manifest = Manifest::from_plan_with_outputs(
            plan,
            &paths.root,
            paths.output.as_deref(),
            paths.tar.as_deref(),
        );
        manifest.write(path)?;
    }
    outputs.written = true;
    assert_eq!(outputs.rootfs.is_some(), paths.output.is_some());
    assert_eq!(outputs.tar.is_some(), paths.tar.is_some());
    Ok(outputs)
}

/// What was actually materialized, as opposed to only planned.
#[derive(Debug, Default)]
pub(crate) struct Outputs {
    pub(crate) rootfs: Option<RootFsReport>,
    pub(crate) tar: Option<TarReport>,
    pub(crate) written: bool,
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

/// The manifest sits beside the bundle, not inside it: a rootfs contains only
/// what the plan put there.
fn manifest_path_default(output: &Path) -> PathBuf {
    let path = match output.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(MANIFEST_NAME_DEFAULT),
        _ => PathBuf::from(MANIFEST_NAME_DEFAULT),
    };
    assert!(path.ends_with(MANIFEST_NAME_DEFAULT));
    path
}

fn verify(args: &VerifyArgs, verbosity: Verbosity) -> anyhow::Result<()> {
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
