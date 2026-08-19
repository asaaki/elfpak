//! Human-facing rendering of plans, diagnostics and errors.

use std::io::Write;
use std::path::Path;

use elfpak_core::graph::NodeKind;
use elfpak_core::plan::{BundlePlan, InclusionReason, PlannedFileKind};
use elfpak_core::{Error, RootFsReport};

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// `elfpak inspect` output.
pub fn inspect(out: &mut dyn Write, binary: &Path, plan: &BundlePlan) -> std::io::Result<()> {
    let graph = &plan.graph;
    writeln!(out, "{}", binary.display())?;
    writeln!(out, "  {}", plan.architecture)?;
    writeln!(out)?;

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
    writeln!(out)?;

    let direct: Vec<_> = graph.dependencies(graph.root);
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

    let direct_ids: Vec<_> = graph
        .edges
        .iter()
        .filter(|e| e.from == graph.root)
        .map(|e| e.to)
        .collect();
    let transitive: Vec<_> = graph
        .nodes
        .iter()
        .enumerate()
        .filter(|(id, node)| {
            node.kind == NodeKind::SharedObject && !direct_ids.contains(id) && *id != graph.root
        })
        .collect();
    if !transitive.is_empty() {
        writeln!(out, "  transitive:")?;
        for (id, node) in &transitive {
            let via = graph
                .first_dependent(*id)
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
    }

    let shared = graph.shared_objects().count();
    writeln!(out, "  runtime:")?;
    writeln!(out, "    {shared} shared objects")?;
    writeln!(out, "    {}", human_size(graph.total_size()))?;
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

pub fn reason(reason: &InclusionReason) -> String {
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
pub fn bundle_summary(
    out: &mut dyn Write,
    binary: &Path,
    plan: &BundlePlan,
    output: &Path,
    manifest: Option<&Path>,
    report: Option<&RootFsReport>,
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
            )?;
        }
    }

    let files = plan
        .files
        .iter()
        .filter(|f| {
            !matches!(
                f.kind,
                PlannedFileKind::Directory | PlannedFileKind::Symlink
            )
        })
        .count();
    let dirs = plan.files_of_kind(PlannedFileKind::Directory).count();
    let links = plan.files_of_kind(PlannedFileKind::Symlink).count();

    writeln!(out)?;
    writeln!(
        out,
        "  {files} files, {dirs} directories, {links} symlinks, {}",
        human_size(plan.total_size())
    )?;
    match report {
        Some(_) => writeln!(out, "  rootfs:   {}", output.display())?,
        None => writeln!(
            out,
            "  rootfs:   {} (dry run, nothing written)",
            output.display()
        )?,
    }
    if let Some(manifest) = manifest {
        writeln!(out, "  manifest: {}", manifest.display())?;
    }

    for warning in &plan.warnings {
        writeln!(out)?;
        writeln!(out, "warning[{}]:", warning.code)?;
        writeln!(out, "  {}", warning.message)?;
        for detail in &warning.details {
            writeln!(out, "  {detail}")?;
        }
    }
    Ok(())
}

/// Render a core error in the `error[E2001]:` style.
pub fn error(err: &Error) -> String {
    let mut out = format!("error[{}]:\n  {err}\n", err.code());
    for detail in err.details() {
        out.push('\n');
        out.push_str(&detail);
        out.push('\n');
    }
    out
}
