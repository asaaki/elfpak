//! Compare the resolver against the real glibc loader.
//!
//! `ldd` is a *test* oracle only. `elfpak` itself never runs it.
//!
//! Host binaries cover breadth; the purpose-built fixtures cover the loader
//! rules that a normal system binary never exercises — RPATH inheritance,
//! RUNPATH non-inheritance and `$ORIGIN` expansion.

mod common;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use common::{HostFixtures, have_cc, ldd_closure, ldd_raw};
use elfpak_core::{ElfMetadata, Error, NodeKind, Planner, SourceRoot};

/// Shared objects `elfpak` resolves, excluding the executable and `PT_INTERP`.
///
/// The interpreter is deliberately left out of the comparison: `ldd` only ever
/// prints it because glibc's `libc.so.6` declares it as `DT_NEEDED`, so a
/// binary that does not link libc has no loader line at all. `elfpak` always
/// includes it, which is what the kernel requires; that is covered separately.
fn elfpak_closure(binary: &Path) -> BTreeSet<PathBuf> {
    let plan = Planner::new(SourceRoot::new("/"), binary)
        .plan()
        .unwrap_or_else(|e| panic!("planning {} failed: {e}", binary.display()));
    plan.graph
        .nodes
        .iter()
        .filter(|node| node.kind == NodeKind::SharedObject)
        .map(|node| node.logical.clone())
        .collect()
}

/// The loader's answer, with the interpreter removed for the same reason.
fn loader_closure(binary: &Path) -> Option<BTreeSet<PathBuf>> {
    let mut paths = ldd_closure(binary)?;
    if let Ok(metadata) = ElfMetadata::parse_file(binary)
        && let Some(interpreter) = metadata.interpreter
    {
        let canonical = std::fs::canonicalize(&interpreter).unwrap_or(interpreter);
        paths.remove(&canonical);
    }
    Some(paths)
}

#[test]
fn matches_the_glibc_loader_for_host_binaries() {
    let candidates: Vec<PathBuf> = ["/usr/bin/ls", "/usr/bin/env", "/usr/bin/gcc"]
        .iter()
        .map(PathBuf::from)
        .filter(|p| p.is_file())
        .chain(std::env::current_exe().ok())
        .collect();

    let mut compared = 0;
    for binary in candidates {
        let Some(expected) = loader_closure(&binary) else {
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

#[test]
fn matches_the_glibc_loader_for_rpath_and_origin_fixtures() {
    if !have_cc() {
        return;
    }
    let fixtures = HostFixtures::build();

    for name in ["app-rpath", "app-origin"] {
        let binary = fixtures.bin(name);
        let Some(expected) = loader_closure(&binary) else {
            panic!("the loader could not resolve {name}, so it cannot be an oracle");
        };
        let actual = elfpak_closure(&binary);
        assert_eq!(actual, expected, "closure mismatch for {name}");
    }
}

#[test]
fn agrees_with_the_loader_that_runpath_is_not_inherited() {
    if !have_cc() {
        return;
    }
    let fixtures = HostFixtures::build();
    let binary = fixtures.bin("app-runpath");

    // The executable's DT_RUNPATH covers libtop but must not be consulted for
    // libtop's own dependency, so glibc reports it missing.
    let ldd = ldd_raw(&binary).expect("ldd runs");
    assert!(
        ldd.contains("libbase.so.1 => not found"),
        "expected glibc to fail on the transitive dependency:\n{ldd}"
    );
    assert!(
        ldd.contains("libtop.so.1 =>") && !ldd.contains("libtop.so.1 => not found"),
        "the direct dependency should still resolve:\n{ldd}"
    );

    // elfpak must reach the same conclusion rather than being more permissive.
    let error = Planner::new(SourceRoot::new("/"), &binary)
        .plan()
        .expect_err("elfpak must not resolve what the loader cannot");
    let Error::UnresolvedLibrary { soname, .. } = &error else {
        panic!("expected E2001, got {error:?}");
    };
    assert_eq!(soname, "libbase.so.1");
}

#[test]
fn agrees_with_the_loader_that_rpath_is_inherited() {
    if !have_cc() {
        return;
    }
    let fixtures = HostFixtures::build();
    let binary = fixtures.bin("app-rpath");

    // Same layout as `app-runpath`, only DT_RPATH instead of DT_RUNPATH, which
    // the loader applies to the whole chain.
    let ldd = ldd_raw(&binary).expect("ldd runs");
    assert!(
        !ldd.contains("not found"),
        "glibc should resolve everything through the inherited RPATH:\n{ldd}"
    );

    let closure = elfpak_closure(&binary);
    assert!(
        closure
            .iter()
            .any(|p| p.file_name().unwrap() == "libbase.so.1.4.2"),
        "the transitive dependency must be part of the closure: {closure:?}"
    );
}
