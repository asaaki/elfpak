//! Compare the resolver against the real glibc loader.
//!
//! `ldd` is a *test* oracle only. `elfpak` itself never runs it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use elfpak_core::{Planner, SourceRoot};

/// Library paths reported by the glibc loader, canonicalized.
fn ldd_closure(binary: &Path) -> Option<BTreeSet<PathBuf>> {
    let output = Command::new("ldd").arg(binary).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut paths = BTreeSet::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("linux-vdso") || line.contains("statically linked") {
            continue;
        }
        let candidate = match line.split_once("=>") {
            Some((_, rest)) => rest.trim(),
            None => line,
        };
        let path = candidate.split(" (").next().unwrap_or("").trim();
        if path.is_empty() || !path.starts_with('/') {
            continue;
        }
        paths.insert(std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path)));
    }
    (!paths.is_empty()).then_some(paths)
}

fn elfpak_closure(binary: &Path) -> BTreeSet<PathBuf> {
    let plan = Planner::new(SourceRoot::new("/"), binary)
        .plan()
        .unwrap_or_else(|e| panic!("planning {} failed: {e}", binary.display()));
    plan.graph
        .nodes
        .iter()
        .filter(|node| node.logical != plan.graph.root_node().logical)
        .map(|node| node.logical.clone())
        .collect()
}

#[test]
fn matches_the_glibc_loader_for_host_binaries() {
    let candidates: Vec<PathBuf> = ["/usr/bin/ls", "/usr/bin/env", "/usr/bin/gcc"]
        .iter()
        .map(PathBuf::from)
        .chain(std::env::current_exe().ok())
        .filter(|p| p.is_file())
        .collect();

    let mut compared = 0;
    for binary in candidates {
        let Some(expected) = ldd_closure(&binary) else {
            continue;
        };
        let actual = elfpak_closure(&binary);
        assert_eq!(
            actual,
            expected,
            "closure mismatch for {}",
            binary.display()
        );
        compared += 1;
    }

    if compared == 0 {
        eprintln!("note: no comparable binaries found, skipping");
    }
}
