//! Loader-semantics tests against a synthetic sysroot.

mod common;

use common::{Sysroot, have_cc};
use elfpak_core::{
    CachePolicy, DependencyPolicy, Error, NodeKind, PlannedFileKind, Planner, Preset,
    RuntimePolicy, SourceRoot,
};
use std::path::{Path, PathBuf};

/// The web preset without the CA bundle, which the fixture sysroot has no
/// equivalent of.
fn web_policy() -> RuntimePolicy {
    let mut policy = RuntimePolicy::from_preset(Preset::Web);
    policy.ca_certificates = false;
    policy
}

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
    plan.graph()
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
                f.destination().display().to_string(),
                f.link_target().as_ref().unwrap().display().to_string(),
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
    assert_eq!(
        plan.executable().destination(),
        PathBuf::from("/app/server")
    );
    for node in plan.graph().shared_objects() {
        assert_eq!(node.destination, node.logical);
    }
}

#[test]
fn interpreter_is_part_of_the_closure() {
    let Some(sysroot) = sysroot() else { return };
    let plan = plan_for(&sysroot, "/bin/app-default");

    let interpreter = plan.interpreter().expect("PT_INTERP");
    assert!(interpreter.is_absolute());
    assert_eq!(
        plan.graph()
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
        plan.files()
            .iter()
            .map(|f| {
                format!(
                    "{} {} {:?}",
                    f.destination().display(),
                    f.kind().as_str(),
                    f.sha256().as_ref().map(|d| d.0.clone())
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
        .files()
        .iter()
        .map(|f| f.destination().display().to_string())
        .collect();
    for expected in ["/tmp", "/etc/passwd", "/etc/group", "/etc/nsswitch.conf"] {
        assert!(
            destinations.contains(&expected.to_string()),
            "{destinations:?}"
        );
    }
    let tmp = plan
        .files()
        .iter()
        .find(|f| f.destination() == Path::new("/tmp"))
        .unwrap();
    assert_eq!(tmp.mode(), 0o1777);
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

#[test]
fn the_allow_list_governs_the_application_not_the_runtime_policy() {
    let Some(sysroot) = sysroot() else { return };
    // The NSS module is dlopen()ed by glibc, so no caller can list it up front.
    let plan = Planner::new(
        SourceRoot::new(&sysroot.root),
        sysroot.path("/bin/app-default"),
    )
    .install_as("/app/server")
    .runtime_policy(web_policy())
    .dependency_policy(DependencyPolicy::allow_list(vec![
        "libtop.so.1".into(),
        "libbase.so.1".into(),
    ]))
    .plan()
    .expect("policy-provided modules are outside the dependency contract");

    assert!(
        logical_paths(&plan)
            .iter()
            .any(|p| p.ends_with("/usr/lib/libnss_files.so.2")),
        "the NSS module is still bundled: {:?}",
        logical_paths(&plan)
    );

    // A library the application itself pulls in is still policed.
    let err = Planner::new(
        SourceRoot::new(&sysroot.root),
        sysroot.path("/bin/app-default"),
    )
    .runtime_policy(web_policy())
    .dependency_policy(DependencyPolicy::allow_list(vec!["libtop.so.1".into()]))
    .plan()
    .unwrap_err();
    assert_eq!(err.code(), "E2002");
}

/// The generated cache, parsed back the way the loader would read it.
fn bundled_cache(plan: &elfpak_core::BundlePlan) -> Option<elfpak_core::LdCache> {
    let file = plan
        .files()
        .iter()
        .find(|f| f.destination() == Path::new("/etc/ld.so.cache"))?;
    assert_eq!(
        f_reason(file),
        "runtime policy: ld-so-cache",
        "the cache carries its own reason"
    );
    assert_eq!(
        file.sha256().cloned(),
        Some(elfpak_core::hash::sha256_bytes(
            file.content().expect("generated content")
        ))
    );
    Some(elfpak_core::LdCache::parse(file.content().unwrap()))
}

fn f_reason(file: &elfpak_core::PlannedFile) -> String {
    match file.reason() {
        elfpak_core::InclusionReason::RuntimePolicy { feature } => {
            format!("runtime policy: {}", feature.as_str())
        }
        other => format!("{other:?}"),
    }
}

#[test]
fn a_library_outside_the_loader_path_gets_a_cache_that_finds_it() {
    let Some(sysroot) = sysroot() else { return };

    // /opt/cached is reachable only through the build host's ld.so.cache, which
    // the bundle does not inherit — so the bundle needs one of its own.
    let plan = plan_for(&sysroot, "/bin/app-cached");
    let cache = bundled_cache(&plan).expect("a cache is generated");
    assert_eq!(
        cache.lookup("libcached.so.1"),
        [PathBuf::from("/opt/cached/libcached.so.1")],
        "the loader can find the library at its bundled path"
    );
    assert!(
        !plan.warnings().iter().any(|w| w.code == "E2005"),
        "nothing to warn about once the cache exists: {:?}",
        plan.warnings()
    );

    // Every other shared object is in there too, so the cache is a complete
    // description of the bundle rather than a patch for one library.
    for node in plan.graph().shared_objects() {
        let soname = node.soname.clone().unwrap();
        assert_eq!(
            cache.lookup(&soname),
            std::slice::from_ref(&node.destination),
            "{soname} is missing from the cache"
        );
    }

    // A directory that only /etc/ld.so.conf named is the same situation.
    let conf = plan_for(&sysroot, "/bin/app-conf");
    assert_eq!(
        bundled_cache(&conf).unwrap().lookup("libconf.so.1"),
        [PathBuf::from("/opt/conf/lib/libconf.so.1")]
    );
}

#[test]
fn a_closure_the_loader_can_already_find_gets_no_cache() {
    let Some(sysroot) = sysroot() else { return };
    // Everything is in /usr/lib, which the loader searches by default: adding a
    // cache would put a file in the image for no reason.
    let plan = plan_for(&sysroot, "/bin/app-default");
    assert!(bundled_cache(&plan).is_none(), "no cache was needed");
    assert!(!plan.warnings().iter().any(|w| w.code == "E2005"));
}

#[test]
fn the_cache_can_be_forced_on_or_off() {
    let Some(sysroot) = sysroot() else { return };

    let policy = RuntimePolicy {
        ld_so_cache: CachePolicy::Always,
        ..RuntimePolicy::default()
    };
    let forced = Planner::new(
        SourceRoot::new(&sysroot.root),
        sysroot.path("/bin/app-default"),
    )
    .install_as("/app/server")
    .runtime_policy(policy)
    .plan()
    .unwrap();
    assert_eq!(
        bundled_cache(&forced).unwrap().lookup("libtop.so.1"),
        [PathBuf::from("/usr/lib/libtop.so.1")]
    );

    // Suppressing it brings the diagnostic back, because the problem is real
    // again: nothing in the bundle points the loader at /opt/cached.
    let policy = RuntimePolicy {
        ld_so_cache: CachePolicy::Never,
        ..RuntimePolicy::default()
    };
    let suppressed = Planner::new(
        SourceRoot::new(&sysroot.root),
        sysroot.path("/bin/app-cached"),
    )
    .install_as("/app/server")
    .runtime_policy(policy)
    .plan()
    .unwrap();
    assert!(bundled_cache(&suppressed).is_none());
    let warning = suppressed
        .warnings()
        .iter()
        .find(|w| w.code == "E2005")
        .unwrap_or_else(|| panic!("expected E2005, got {:?}", suppressed.warnings()));
    assert!(
        warning
            .details
            .iter()
            .any(|d| d.contains("libcached.so.1") && d.contains("/opt/cached")),
        "{:?}",
        warning.details
    );
}

#[test]
fn a_relocated_origin_relative_executable_gets_a_cache() {
    let Some(sysroot) = sysroot() else { return };

    // $ORIGIN moves with the binary, so /opt/origin/lib is no longer where the
    // executable's search path points once it is installed at /app/server.
    let moved = plan_for(&sysroot, "/opt/origin/bin/app-origin");
    assert_eq!(
        bundled_cache(&moved).unwrap().lookup("libor.so.1"),
        [PathBuf::from("/opt/origin/lib/libor.so.1")],
        "the cache keeps the relocated executable working"
    );
    assert!(!moved.warnings().iter().any(|w| w.code == "E2006"));

    // Installed where it already lives, the relative paths still hold and
    // nothing has to be generated.
    let in_place = Planner::new(
        SourceRoot::new(&sysroot.root),
        sysroot.path("/opt/origin/bin/app-origin"),
    )
    .install_as("/opt/origin/bin/app-origin")
    .plan()
    .unwrap();
    assert!(bundled_cache(&in_place).is_none(), "nothing was needed");

    // With the cache suppressed, the relocation is reported instead.
    let policy = RuntimePolicy {
        ld_so_cache: CachePolicy::Never,
        ..RuntimePolicy::default()
    };
    let suppressed = Planner::new(
        SourceRoot::new(&sysroot.root),
        sysroot.path("/opt/origin/bin/app-origin"),
    )
    .install_as("/app/server")
    .runtime_policy(policy)
    .plan()
    .unwrap();
    let warning = suppressed
        .warnings()
        .iter()
        .find(|w| w.code == "E2006")
        .unwrap_or_else(|| panic!("expected E2006, got {:?}", suppressed.warnings()));
    assert!(warning.message.contains("/opt/origin/bin"), "{warning:?}");
    assert!(warning.details.iter().any(|d| d.contains("$ORIGIN/../lib")));
}

#[test]
fn an_unsupported_architecture_names_what_it_found() {
    let Some(sysroot) = sysroot() else { return };
    let mut bytes = std::fs::read(sysroot.path("/bin/app-default")).unwrap();
    // e_machine sits at the same offset in every little-endian ELF.
    bytes[18] = 243; // EM_RISCV
    bytes[19] = 0;
    let tmp = tempfile::tempdir().unwrap();
    let binary = tmp.path().join("riscv-app");
    std::fs::write(&binary, &bytes).unwrap();

    let err = Planner::new(SourceRoot::new(&sysroot.root), &binary)
        .plan()
        .unwrap_err();
    assert_eq!(err.code(), "E1003");
    let message = err.to_string();
    assert!(message.contains("riscv64"), "{message}");
    assert!(message.contains("0xf3"), "{message}");
    assert!(err.details().iter().any(|d| d.contains("x86_64")));
}

#[test]
fn the_install_path_may_not_displace_a_bundled_library() {
    let Some(sysroot) = sysroot() else { return };
    let err = Planner::new(
        SourceRoot::new(&sysroot.root),
        sysroot.path("/bin/app-default"),
    )
    .install_as("/usr/lib/libtop.so.1")
    .plan()
    .unwrap_err();
    assert_eq!(err.code(), "E4001");
    assert!(err.to_string().contains("libtop.so.1"), "{err}");

    // An install path with no file name is rejected before anything is read.
    let err = Planner::new(
        SourceRoot::new(&sysroot.root),
        sysroot.path("/bin/app-default"),
    )
    .install_as("/")
    .plan()
    .unwrap_err();
    assert_eq!(err.code(), "E4001");
}
