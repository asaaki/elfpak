//! Compare the resolver against the real glibc loader.
//!
//! `ldd` is a *test* oracle only. `elfpak` itself never runs it.
//!
//! Host binaries cover breadth; the purpose-built fixtures cover the loader
//! rules that a normal system binary never exercises — RPATH inheritance,
//! RUNPATH non-inheritance and `$ORIGIN` expansion.

mod common;

use common::{HostFixtures, cc, have_cc, ldd_closure, ldd_raw};
use elfpak_core::{ElfMetadata, Error, NodeKind, Planner, SourceRoot};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

/// Shared objects `elfpak` resolves, excluding the executable and `PT_INTERP`.
///
/// The interpreter is left out of the comparison: `ldd` only prints it because
/// glibc's `libc.so.6` declares it as `DT_NEEDED`, so a binary that does not
/// link libc has no loader line at all. `elfpak` always includes it, as the
/// kernel requires; that is covered separately.
fn elfpak_closure(binary: &Path) -> BTreeSet<PathBuf> {
    let plan = Planner::new(SourceRoot::new("/"), binary)
        .plan()
        .unwrap_or_else(|e| panic!("planning {} failed: {e}", binary.display()));
    plan.graph()
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

/// glibc guards the whole RPATH phase on the *requesting* object having no
/// DT_RUNPATH, so a library with DT_RUNPATH cannot reach its dependency through
/// the RPATH of the executable that loaded it.
#[test]
fn agrees_with_the_loader_that_runpath_blocks_an_inherited_rpath() {
    if !have_cc() {
        return;
    }
    let fixtures = HostFixtures::build();
    let binary = fixtures.bin("app-rpath-blocked");

    let ldd = ldd_raw(&binary).expect("ldd runs");
    assert!(
        ldd.contains("libtop-runpath.so.1 =>") && !ldd.contains("libtop-runpath.so.1 => not found"),
        "the executable's own RPATH should still resolve its direct dependency:\n{ldd}"
    );
    assert!(
        ldd.contains("libbase.so.1 => not found"),
        "DT_RUNPATH on the intermediate library should block the inherited RPATH:\n{ldd}"
    );

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

/// Whether a private mount namespace can be set up unprivileged, which is what
/// lets a test put a generated cache where the loader looks for it.
fn can_bind_mount() -> bool {
    std::process::Command::new("unshare")
        .args(["-Urm", "true"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Compile `libhidden.so.1` into `directory` and return its path. Nothing about
/// the library matters except that it is real and that it says where it is.
fn build_hidden_library(tmp: &Path, directory: &Path) -> PathBuf {
    let source = tmp.join("hidden.c");
    std::fs::write(&source, "int hidden_value(void) { return 42; }\n").unwrap();

    let library = directory.join("libhidden.so.1");
    cc(&[
        "-shared",
        "-fPIC",
        "-Wl,-soname,libhidden.so.1",
        "-o",
        library.to_str().unwrap(),
        source.to_str().unwrap(),
    ]);
    library
}

/// Compile a program that prints `value=42` if — and only if — it managed to
/// load `library` at runtime.
fn build_hidden_program(tmp: &Path, library: &Path) -> PathBuf {
    let main = tmp.join("main.c");
    std::fs::write(
        &main,
        "#include <stdio.h>\nint hidden_value(void);\n\
         int main(void) { printf(\"value=%d\\n\", hidden_value()); return 0; }\n",
    )
    .unwrap();

    let program = tmp.join("app");
    cc(&[
        "-o",
        program.to_str().unwrap(),
        main.to_str().unwrap(),
        library.to_str().unwrap(),
        &format!("-Wl,-rpath-link,{}", library.parent().unwrap().display()),
    ]);
    program
}

/// The real entry, surrounded by decoys on both sides of it in sort order.
///
/// With a single entry the loader's binary search finds it whatever order the
/// table is in, so a table sorted the wrong way round would still pass.
fn cache_entries_with_decoys(library: &Path) -> Vec<elfpak_core::resolver::cache::CacheEntry> {
    use elfpak_core::resolver::cache::CacheEntry;

    let mut entries: Vec<CacheEntry> = [
        "libaaa.so.1",
        "libccc.so.1",
        "libmmm.so.1",
        "libppp.so.1",
        "libyyy.so.1",
        "libzzz.so.1",
    ]
    .iter()
    .map(|soname| CacheEntry {
        soname: (*soname).to_string(),
        path: PathBuf::from("/nonexistent").join(soname),
    })
    .collect();
    entries.push(CacheEntry {
        soname: "libhidden.so.1".to_string(),
        path: library.to_path_buf(),
    });
    entries
}

///
/// A library is placed somewhere the loader does not search, so the program
/// cannot start; the generated cache is then bind-mounted over
/// `/etc/ld.so.cache` inside a private mount namespace, and the same program
/// must run. Nothing outside the namespace is touched.
#[test]
fn glibc_loads_a_library_through_a_generated_cache() {
    if !have_cc() || !can_bind_mount() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let hidden = tmp.path().join("hidden");
    std::fs::create_dir_all(&hidden).unwrap();

    let library = build_hidden_library(tmp.path(), &hidden);
    let program = build_hidden_program(tmp.path(), &library);

    // Without help, the loader cannot find the library at all.
    let bare = std::process::Command::new(&program).output().unwrap();
    assert!(
        !bare.status.success(),
        "the fixture is only meaningful if the loader fails first"
    );

    let architecture = ElfMetadata::parse_file(&program).unwrap().architecture;
    let entries = cache_entries_with_decoys(&library);

    let Some(image) = elfpak_core::resolver::cache::build(&architecture, &entries) else {
        return; // an architecture the cache format cannot describe
    };
    let cache = tmp.path().join("ld.so.cache");
    std::fs::write(&cache, &image).unwrap();

    let output = std::process::Command::new("unshare")
        .args(["-Urm", "sh", "-c"])
        .arg(format!(
            "mount --bind {} /etc/ld.so.cache && {}",
            cache.display(),
            program.display()
        ))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("value=42"),
        "the glibc loader rejected the generated cache:\nstdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// `ldconfig -p` is glibc's own cache reader, and a second opinion that does
/// not need a mount namespace.
#[test]
fn ldconfig_reads_a_generated_cache() {
    let architecture = match std::env::current_exe()
        .ok()
        .and_then(|exe| ElfMetadata::parse_file(&exe).ok())
    {
        Some(metadata) => metadata.architecture,
        None => return,
    };
    let Some(image) = elfpak_core::resolver::cache::build(
        &architecture,
        &[
            elfpak_core::resolver::cache::CacheEntry {
                soname: "libcached.so.1".to_string(),
                path: PathBuf::from("/opt/cached/libcached.so.1"),
            },
            elfpak_core::resolver::cache::CacheEntry {
                soname: "libc.so.6".to_string(),
                path: PathBuf::from("/usr/lib/libc.so.6"),
            },
        ],
    ) else {
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    let cache = tmp.path().join("ld.so.cache");
    std::fs::write(&cache, &image).unwrap();

    let Ok(output) = std::process::Command::new("ldconfig")
        .args(["-p", "-C"])
        .arg(&cache)
        .output()
    else {
        return;
    };
    if !output.status.success() {
        // ldconfig refuses to read a cache it does not understand, and says so.
        panic!(
            "ldconfig rejected the generated cache: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let listing = String::from_utf8_lossy(&output.stdout);
    assert!(
        listing.contains("libcached.so.1") && listing.contains("/opt/cached/libcached.so.1"),
        "{listing}"
    );
    assert!(
        listing.contains("2 libs found"),
        "both entries are readable: {listing}"
    );
}

/// End to end: a bundle whose library lives outside every directory the loader
/// searches must still start, and must stop starting when the cache is taken
/// away.
///
/// The rootfs is entered with `chroot` inside a private user and mount
/// namespace, which needs no privileges and touches nothing outside it.
#[test]
fn a_bundled_rootfs_starts_from_a_directory_the_loader_never_searches() {
    if !have_cc() || !can_bind_mount() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let hidden = tmp.path().join("elsewhere/lib");
    std::fs::create_dir_all(&hidden).unwrap();

    let library = build_hidden_library(tmp.path(), &hidden);
    let program = build_hidden_program(tmp.path(), &library);

    let run = |rootfs: &Path| {
        std::process::Command::new("unshare")
            .args(["-Urm", "sh", "-c"])
            .arg(format!("chroot {} /app/server", rootfs.display()))
            .output()
            .unwrap()
    };

    let plan = |policy: elfpak_core::CachePolicy| {
        let runtime = elfpak_core::RuntimePolicy {
            ld_so_cache: policy,
            ..elfpak_core::RuntimePolicy::default()
        };
        Planner::new(SourceRoot::new("/"), &program)
            .install_as("/app/server")
            .library_paths(vec![hidden.clone()])
            .runtime_policy(runtime)
            .plan()
            .unwrap()
    };

    let with_cache = tmp.path().join("rootfs");
    elfpak_core::RootFsBuilder::new(&with_cache)
        .apply(&plan(elfpak_core::CachePolicy::Auto))
        .unwrap();
    assert!(
        with_cache.join("etc/ld.so.cache").is_file(),
        "the closure needs a cache, so one is generated"
    );
    let output = run(&with_cache);
    assert!(
        output.status.success() && String::from_utf8_lossy(&output.stdout).contains("value=42"),
        "the packaged application did not start:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Without the cache the same tree cannot resolve the library at all.
    let without = tmp.path().join("rootfs-no-cache");
    elfpak_core::RootFsBuilder::new(&without)
        .apply(&plan(elfpak_core::CachePolicy::Never))
        .unwrap();
    assert!(!without.join("etc/ld.so.cache").exists());
    let output = run(&without);
    assert!(
        !output.status.success(),
        "expected the loader to fail without a cache, got: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}
