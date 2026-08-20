//! Human-facing rendering of plans, diagnostics and errors.
//!
//! Every function here takes a finished plan and writes it out; the two entry
//! points are [`inspect`] and [`bundle_summary`]. [`Verbosity`] decides whether
//! any of it is written at all.

use crate::bundle::Outputs;
use elfpak_core::{
    Error,
    graph::{DependencyGraph, NodeKind},
    plan::{BundlePlan, InclusionReason, PlannedFile, PlannedFileKind, Warning},
};
use std::{io::Write, path::Path};

/// How much of the above reaches the terminal: `-q` silences everything except
/// errors, `-v` adds notes about how the run was configured.
#[derive(Clone, Copy)]
pub(crate) struct Verbosity {
    quiet: bool,
    level: u8,
}

impl Verbosity {
    pub(crate) fn new(quiet: bool, level: u8) -> Verbosity {
        Verbosity { quiet, level }
    }

    pub(crate) fn level(&self) -> u8 {
        self.level
    }

    pub(crate) fn print(&self, render: impl FnOnce(&mut dyn Write) -> std::io::Result<()>) {
        if self.quiet {
            return;
        }
        let mut stdout = std::io::stdout();
        let _ = render(&mut stdout);
    }

    pub(crate) fn note(&self, message: impl std::fmt::Display) {
        if self.quiet || self.level == 0 {
            return;
        }
        eprintln!("note: {message}");
    }
}

/// Where a bundle was (or would be) written.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Destinations<'a> {
    pub(crate) rootfs: Option<&'a Path>,
    pub(crate) tar: Option<&'a Path>,
    pub(crate) manifest: Option<&'a Path>,
}

const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
const UNIT_STEP_BYTES: f64 = 1024.0;

pub(crate) fn human_size(bytes: u64) -> String {
    let mut value = bytes as f64;
    let mut unit = 0;
    // Bounded by the number of units, and each step divides by 1024.
    while value >= UNIT_STEP_BYTES && unit < UNITS.len() - 1 {
        value /= UNIT_STEP_BYTES;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// `elfpak inspect` output.
pub(crate) fn inspect(
    out: &mut dyn Write,
    binary: &Path,
    plan: &BundlePlan,
) -> std::io::Result<()> {
    writeln!(out, "{}", binary.display())?;
    writeln!(out, "  {}", plan.architecture)?;
    writeln!(out)?;

    inspect_interpreter(out, plan)?;
    inspect_dependencies(out, &plan.graph)?;
    inspect_transitive(out, &plan.graph)?;

    writeln!(out, "  runtime:")?;
    writeln!(
        out,
        "    {} shared objects",
        plan.graph.shared_objects().count()
    )?;
    writeln!(out, "    {}", human_size(plan.graph.total_size()))?;
    writeln!(out)?;

    writeln!(out, "  warnings:")?;
    if plan.warnings.is_empty() {
        writeln!(out, "    none")?;
    }
    for warning in &plan.warnings {
        writeln!(out, "    {}: {}", warning.code, warning.message)?;
    }
    Ok(())
}

/// `PT_INTERP`, and where it actually lands once symlinks are followed.
fn inspect_interpreter(out: &mut dyn Write, plan: &BundlePlan) -> std::io::Result<()> {
    writeln!(out, "  interpreter:")?;
    match &plan.interpreter {
        Some(interp) => {
            writeln!(out, "    {}", interp.display())?;
            if let Some(resolved) = &plan.interpreter_resolved
                && resolved != interp
            {
                writeln!(out, "      -> {}", resolved.display())?;
            }
        }
        None => writeln!(out, "    none (statically linked)")?,
    }
    writeln!(out)
}

/// `DT_NEEDED` of the executable itself. The interpreter is listed separately.
fn inspect_dependencies(out: &mut dyn Write, graph: &DependencyGraph) -> std::io::Result<()> {
    let direct = graph.dependencies(graph.root);

    writeln!(out, "  dependencies:")?;
    if direct
        .iter()
        .all(|(_, node)| node.kind == NodeKind::Interpreter)
    {
        writeln!(out, "    none")?;
    }
    for (edge, node) in &direct {
        if node.kind == NodeKind::Interpreter {
            continue;
        }
        let soname = match &edge.reason {
            elfpak_core::DependencyReason::Needed { soname } => soname.clone(),
            _ => node.logical.display().to_string(),
        };
        writeln!(out, "    {soname}")?;
        writeln!(out, "      {}", node.logical.display())?;
        writeln!(out)?;
    }
    Ok(())
}

/// Everything else in the closure, with the object that pulled it in.
fn inspect_transitive(out: &mut dyn Write, graph: &DependencyGraph) -> std::io::Result<()> {
    let direct: Vec<_> = graph
        .edges
        .iter()
        .filter(|e| e.from == graph.root)
        .map(|e| e.to)
        .collect();
    let transitive: Vec<_> = graph
        .iter()
        .filter(|(id, node)| {
            node.kind == NodeKind::SharedObject && !direct.contains(id) && *id != graph.root
        })
        .collect();
    if transitive.is_empty() {
        return Ok(());
    }

    writeln!(out, "  transitive:")?;
    for (id, node) in transitive {
        let via = graph
            .first_dependent(id)
            .map(|(_, parent)| parent.logical.display().to_string())
            .unwrap_or_default();
        let soname = node
            .soname
            .clone()
            .unwrap_or_else(|| node.logical.display().to_string());
        writeln!(out, "    {soname}")?;
        writeln!(out, "      {}", node.logical.display())?;
        writeln!(out, "      required by {via}")?;
        writeln!(out)?;
    }
    Ok(())
}

pub(crate) fn reason(reason: &InclusionReason) -> String {
    match reason {
        InclusionReason::Application => "application".to_string(),
        InclusionReason::Interpreter => "interpreter".to_string(),
        InclusionReason::ExplicitInclude => "include".to_string(),
        InclusionReason::NeededBy { binary, soname } => {
            format!("needed by {} ({soname})", binary.display())
        }
        InclusionReason::RuntimePolicy { feature } => {
            format!("runtime policy: {}", feature.as_str())
        }
    }
}

/// `elfpak bundle` summary.
pub(crate) fn bundle_summary(
    out: &mut dyn Write,
    binary: &Path,
    plan: &BundlePlan,
    destinations: Destinations<'_>,
    outputs: &Outputs,
    verbose: u8,
) -> std::io::Result<()> {
    writeln!(
        out,
        "{} -> {}",
        binary.display(),
        plan.executable.destination.display()
    )?;
    writeln!(out, "  {}", plan.architecture)?;
    if let Some(interp) = &plan.interpreter {
        writeln!(out, "  interpreter: {}", interp.display())?;
    }

    if verbose > 0 {
        writeln!(out)?;
        writeln!(out, "  plan:")?;
        for file in &plan.files {
            summary_entry(out, file)?;
        }
    }

    writeln!(out)?;
    summary_counts(out, plan)?;
    summary_destinations(out, destinations, outputs.written)?;

    for warning in &plan.warnings {
        writeln!(out)?;
        summary_warning(out, warning)?;
    }
    Ok(())
}

/// One line per planned entry: what it is, where it goes, why it is there.
fn summary_entry(out: &mut dyn Write, file: &PlannedFile) -> std::io::Result<()> {
    let marker = match file.kind {
        PlannedFileKind::Directory => "d",
        PlannedFileKind::Symlink => "l",
        _ => "-",
    };
    writeln!(
        out,
        "    {marker} {:<48} {}",
        file.destination.display(),
        reason(&file.reason)
    )
}

fn summary_counts(out: &mut dyn Write, plan: &BundlePlan) -> std::io::Result<()> {
    let dirs = plan.files_of_kind(PlannedFileKind::Directory).count();
    let links = plan.files_of_kind(PlannedFileKind::Symlink).count();
    let files = plan
        .files
        .iter()
        .filter(|file| {
            !matches!(
                file.kind,
                PlannedFileKind::Directory | PlannedFileKind::Symlink
            )
        })
        .count();

    writeln!(
        out,
        "  {files} files, {dirs} directories, {links} symlinks, {}",
        human_size(plan.total_size())
    )
}

fn summary_destinations(
    out: &mut dyn Write,
    destinations: Destinations<'_>,
    written: bool,
) -> std::io::Result<()> {
    let suffix = if written {
        ""
    } else {
        " (dry run, nothing written)"
    };
    if let Some(rootfs) = destinations.rootfs {
        writeln!(out, "  rootfs:   {}{suffix}", rootfs.display())?;
    }
    if let Some(tar) = destinations.tar {
        writeln!(out, "  tar:      {}{suffix}", tar.display())?;
    }
    if let Some(manifest) = destinations.manifest {
        writeln!(out, "  manifest: {}{suffix}", manifest.display())?;
    }
    Ok(())
}

fn summary_warning(out: &mut dyn Write, warning: &Warning) -> std::io::Result<()> {
    writeln!(out, "warning[{}]:", warning.code)?;
    writeln!(out, "  {}", warning.message)?;
    for detail in &warning.details {
        writeln!(out, "  {detail}")?;
    }
    Ok(())
}

/// Render a core error in the `error[E2001]:` style.
pub(crate) fn error(err: &Error) -> String {
    let mut out = format!("error[{}]:\n  {err}\n", err.code());
    for detail in err.details() {
        out.push('\n');
        out.push_str(&detail);
        out.push('\n');
    }
    out
}
