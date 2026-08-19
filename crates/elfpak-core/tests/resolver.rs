//! Loader-semantics tests against a synthetic sysroot.

mod common;

use std::path::{Path, PathBuf};

use common::{Sysroot, have_cc};
use elfpak_core::{
    DependencyPolicy, Error, NodeKind, PlannedFileKind, Planner, Preset, RuntimePolicy, SourceRoot,
};

fn sysroot() -> Option<Sysroot> {
    have_cc().then(Sysroot::build)
}

fn plan_for(sysroot: &Sysroot, exe: &str) -> elfpak_core::BundlePlan {
    Planner::new(SourceRoot::new(&sysroot.root), sysroot.path(exe))
        .install_as("/app/server")
        .plan()
        .unwrap_or_else(|e| panic!("planning {exe} failed: {e}"))
}

fn logical_paths(plan: &elfpak_core::BundlePlan) -> Vec<String> {
    plan.graph
        .nodes
        .iter()
        .map(|n| n.logical.display().to_string())
        .collect()
}

#[test]
fn resolves_transitive_needed_and_preserves_soname_symlinks() {
    let Some(sysroot) = sysroot() else { return };
    let plan = plan_for(&sysroot, "/bin/app-default");

    let paths = logical_paths(&plan);
    assert!(
        paths.iter().any(|p| p.ends_with("/usr/lib/libtop.so.1")),
        "{paths:?}"
    );
    assert!(
        paths
            .iter()
            .any(|p| p.ends_with("/usr/lib/libbase.so.1.4.2")),
        "the versioned file, not the symlink, is the graph node: {paths:?}"
    );

    // The `libbase.so.1 -> libbase.so.1.4.2` relationship must survive.
    let links: Vec<_> = plan
        .files_of_kind(PlannedFileKind::Symlink)
        .map(|f| {
            (
                f.destination.display().to_string(),
                f.link_target.as_ref().unwrap().display().to_string(),
            )
        })
        .collect();
    assert!(
        links.contains(&(
            "/usr/lib/libbase.so.1".to_string(),
            "libbase.so.1.4.2".to_string()
        )),
        "{links:?}"
    );
    assert!(
        links.contains(&("/lib".to_string(), "usr/lib".to_string())),
        "the merged-/usr symlink is preserved too: {links:?}"
    );

    // The executable is installed where asked; libraries keep their paths.
    assert_eq!(plan.executable.destination, PathBuf::from("/app/server"));
    for node in plan.graph.shared_objects() {
        assert_eq!(node.destination, node.logical);
    }
}

#[test]
fn interpreter_is_part_of_the_closure() {
    let Some(sysroot) = sysroot() else { return };
    let plan = plan_for(&sysroot, "/bin/app-default");

    let interpreter = plan.interpreter.expect("PT_INTERP");
    assert!(interpreter.is_absolute());
    assert_eq!(
        plan.graph
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Interpreter)
            .count(),
        1
    );
}

#[test]
fn rpath_is_inherited_by_transitive_lookups() {
    let Some(sysroot) = sysroot() else { return };
    let plan = plan_for(&sysroot, "/bin/app-rpath");
    assert!(
        logical_paths(&plan)
            .iter()
            .any(|p| p.ends_with("/opt/hidden/libdeep.so.1")),
        "DT_RPATH of the executable applies to its dependencies' dependencies"
    );
}

#[test]
fn runpath_is_not_inherited_by_transitive_lookups() {
    let Some(sysroot) = sysroot() else { return };
    let result = Planner::new(
        SourceRoot::new(&sysroot.root),
        sysroot.path("/bin/app-runpath"),
    )
    .install_as("/app/server")
    .plan();

    match result {
        Err(Error::UnresolvedLibrary {
            soname, searched, ..
        }) => {
            assert_eq!(soname, "libdeep.so.1");
            assert!(!searched.is_empty());
            assert!(
                !searched.contains(&PathBuf::from("/opt/hidden")),
                "DT_RUNPATH must not leak into a dependency's search: {searched:?}"
            );
        }
        other => panic!("expected an unresolved library, got {other:?}"),
    }
}

#[test]
fn origin_token_is_expanded_against_the_object_directory() {
    let Some(sysroot) = sysroot() else { return };
    let plan = plan_for(&sysroot, "/opt/origin/bin/app-origin");
    assert!(
        logical_paths(&plan)
            .iter()
            .any(|p| p.ends_with("/opt/origin/lib/libor.so.1")),
        "$ORIGIN/../lib should resolve"
    );
}

#[test]
fn ld_so_conf_include_globs_are_honoured() {
    let Some(sysroot) = sysroot() else { return };
    let plan = plan_for(&sysroot, "/bin/app-conf");
    assert!(
        logical_paths(&plan)
            .iter()
            .any(|p| p.ends_with("/opt/conf/lib/libconf.so.1")),
        "path from /etc/ld.so.conf.d/*.conf should be searched"
    );
}

#[test]
fn ld_so_cache_entries_are_used() {
    let Some(sysroot) = sysroot() else { return };
    let plan = plan_for(&sysroot, "/bin/app-cached");
    assert!(
        logical_paths(&plan)
            .iter()
            .any(|p| p.ends_with("/opt/cached/libcached.so.1")),
        "library reachable only through /etc/ld.so.cache"
    );
}

#[test]
fn a_matching_file_name_is_not_enough() {
    let Some(sysroot) = sysroot() else { return };
    // /opt/decoy/libbase.so.1 is plain text, and is searched first.
    let plan = Planner::new(
        SourceRoot::new(&sysroot.root),
        sysroot.path("/bin/app-default"),
    )
    .library_paths(vec![PathBuf::from("/opt/decoy")])
    .plan()
    .expect("the decoy is skipped, not accepted");
    assert!(
        logical_paths(&plan)
            .iter()
            .any(|p| p.ends_with("/usr/lib/libbase.so.1.4.2"))
    );
}

#[test]
fn unresolved_libraries_report_where_we_looked() {
    let Some(sysroot) = sysroot() else { return };
    let err = Planner::new(
        SourceRoot::new(&sysroot.root),
        sysroot.path("/bin/app-missing"),
    )
    .plan()
    .unwrap_err();

    let Error::UnresolvedLibrary {
        soname, searched, ..
    } = &err
    else {
        panic!("expected E2001, got {err:?}");
    };
    assert_eq!(soname, "libgone.so.9");
    assert!(
        searched.contains(&PathBuf::from("/usr/lib")),
        "{searched:?}"
    );
    assert_eq!(err.code(), "E2001");
}

#[test]
fn dependency_policy_rejects_unexpected_libraries() {
    let Some(sysroot) = sysroot() else { return };
    let err = Planner::new(
        SourceRoot::new(&sysroot.root),
        sysroot.path("/bin/app-default"),
    )
    .dependency_policy(DependencyPolicy::allow_list(vec!["libtop.so.1".into()]))
    .plan()
    .unwrap_err();

    let Error::DisallowedLibrary { soname, .. } = &err else {
        panic!("expected E2002, got {err:?}");
    };
    assert_eq!(soname, "libbase.so.1");
    assert_eq!(err.code(), "E2002");

    // Allowing everything the closure needs makes it pass.
    Planner::new(
        SourceRoot::new(&sysroot.root),
        sysroot.path("/bin/app-default"),
    )
    .dependency_policy(DependencyPolicy::allow_list(vec![
        "libtop.so.1".into(),
        "libbase.so.1".into(),
    ]))
    .plan()
    .expect("allow-list satisfied");
}

#[test]
fn plans_are_deterministic() {
    let Some(sysroot) = sysroot() else { return };
    let first = plan_for(&sysroot, "/bin/app-default");
    let second = plan_for(&sysroot, "/bin/app-default");

    let render = |plan: &elfpak_core::BundlePlan| {
        plan.files
            .iter()
            .map(|f| {
                format!(
                    "{} {} {:?}",
                    f.destination.display(),
                    f.kind.as_str(),
                    f.sha256.as_ref().map(|d| d.0.clone())
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(render(&first), render(&second));
}

#[test]
fn runtime_policy_entries_carry_a_reason() {
    let Some(sysroot) = sysroot() else { return };
    let mut policy = RuntimePolicy::from_preset(Preset::Web);
    policy.ca_certificates = false; // the fixture sysroot has no CA bundle
    let plan = Planner::new(
        SourceRoot::new(&sysroot.root),
        sysroot.path("/bin/app-default"),
    )
    .install_as("/app/server")
    .runtime_policy(policy)
    .plan()
    .unwrap();

    let destinations: Vec<String> = plan
        .files
        .iter()
        .map(|f| f.destination.display().to_string())
        .collect();
    for expected in ["/tmp", "/etc/passwd", "/etc/group", "/etc/nsswitch.conf"] {
        assert!(
            destinations.contains(&expected.to_string()),
            "{destinations:?}"
        );
    }
    let tmp = plan
        .files
        .iter()
        .find(|f| f.destination == Path::new("/tmp"))
        .unwrap();
    assert_eq!(tmp.mode, 0o1777);
}

#[test]
fn missing_ca_bundle_is_an_actionable_error() {
    let Some(sysroot) = sysroot() else { return };
    let err = Planner::new(
        SourceRoot::new(&sysroot.root),
        sysroot.path("/bin/app-default"),
    )
    .runtime_policy(RuntimePolicy::from_preset(Preset::Web))
    .plan()
    .unwrap_err();
    assert_eq!(err.code(), "E2004");
    assert!(!err.details().is_empty());
}
