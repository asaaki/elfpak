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
use elfpak_core::manifest::{DEFAULT_MANIFEST_NAME, Manifest};
use elfpak_core::{
    DependencyPolicy, Error, Planner, Preset, RootFsBuilder, RuntimePolicy, SourceRoot, UserSpec,
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

    if args.json {
        let manifest = Manifest::from_plan(&plan, &args.root, None);
        println!("{}", manifest.to_json());
        return Ok(());
    }

    verbosity.print(|out| render::inspect(out, &args.binary, &plan));
    Ok(())
}

fn bundle(args: &BundleArgs, verbosity: Verbosity) -> anyhow::Result<()> {
    let config = load_config(args, verbosity)?;

    let binary = args
        .binary
        .clone()
        .or_else(|| config.package.binary.clone())
        .ok_or_else(|| Error::Config {
            message: "no binary given (pass one as an argument or set package.binary)".to_string(),
        })?;
    let output = args
        .output
        .clone()
        .or_else(|| config.package.output.clone())
        .ok_or_else(|| Error::Config {
            message: "no output directory given (pass --output or set package.output)".to_string(),
        })?;
    let root = args
        .root
        .clone()
        .or_else(|| config.package.root.clone())
        .unwrap_or_else(|| PathBuf::from("/"));
    let install = args
        .install
        .clone()
        .or_else(|| config.package.install.clone())
        .unwrap_or_else(|| PathBuf::from("/").join(binary.file_name().unwrap_or_default()));

    let preset = args
        .preset
        .or(config.runtime.preset)
        .unwrap_or(Preset::Minimal);
    let mut policy = RuntimePolicy::from_preset(preset);
    for (value, field) in [
        (
            args.ca_certificates.or(config.runtime.ca_certificates),
            Field::Ca,
        ),
        (args.tmp.or(config.runtime.tmp), Field::Tmp),
        (
            args.passwd_group.or(config.runtime.passwd_group),
            Field::Passwd,
        ),
        (args.nsswitch.or(config.runtime.nsswitch), Field::Nsswitch),
        (args.tzdata.or(config.runtime.tzdata), Field::Tzdata),
    ] {
        if let Some(value) = value {
            match field {
                Field::Ca => policy.ca_certificates = value,
                Field::Tmp => policy.tmp = value,
                Field::Passwd => policy.passwd_group = value,
                Field::Nsswitch => policy.nsswitch = value,
                Field::Tzdata => policy.tzdata = value,
            }
        }
    }
    if let Some(user) = args.user.clone().or_else(|| config.runtime.user.clone()) {
        policy.user = Some(UserSpec::parse(&user)?);
    }
    policy.includes = if args.includes.is_empty() {
        config.include.paths.clone()
    } else {
        args.includes.clone()
    };

    let allow = if args.allow_library.is_empty() {
        config.dependencies.allow
    } else {
        Some(args.allow_library.clone())
    };
    let dependency_policy = match allow {
        Some(list) => DependencyPolicy::allow_list(list),
        None => DependencyPolicy::allow_all(),
    };

    let plan = Planner::new(SourceRoot::new(&root), &binary)
        .install_as(&install)
        .runtime_policy(policy)
        .dependency_policy(dependency_policy)
        .library_paths(args.library_paths.clone())
        .plan()?;

    let manifest_path = if args.no_manifest {
        None
    } else {
        Some(
            args.manifest
                .clone()
                .unwrap_or_else(|| default_manifest_path(&output)),
        )
    };

    let report = if args.dry_run {
        None
    } else {
        let report = RootFsBuilder::new(&output).clean(args.clean).apply(&plan)?;
        if let Some(path) = &manifest_path {
            let manifest = Manifest::from_plan(&plan, &root, Some(&output));
            manifest.write(path)?;
        }
        Some(report)
    };

    verbosity.print(|out| {
        render::bundle_summary(
            out,
            &binary,
            &plan,
            &output,
            manifest_path.as_deref(),
            report.as_ref(),
            verbosity.level,
        )
    });
    Ok(())
}

enum Field {
    Ca,
    Tmp,
    Passwd,
    Nsswitch,
    Tzdata,
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

fn default_manifest_path(output: &Path) -> PathBuf {
    match output.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(DEFAULT_MANIFEST_NAME),
        _ => PathBuf::from(DEFAULT_MANIFEST_NAME),
    }
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

    let report = manifest.verify(&rootfs);
    if report.is_ok() {
        verbosity.print(|out| {
            writeln!(
                out,
                "ok: {} entries verified in {}",
                report.checked,
                rootfs.display()
            )
        });
        return Ok(());
    }

    for problem in &report.problems {
        eprintln!("  {}: {}", problem.path, problem.detail);
    }
    Err(Error::VerifyFailed {
        checked: report.checked,
        failures: report.problems.len(),
    }
    .into())
}
