//! Materialization, manifest and filesystem-safety tests.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use common::{Sysroot, have_cc};
use elfpak_core::{Manifest, Planner, RootFsBuilder, SourceRoot};

fn sysroot() -> Option<Sysroot> {
    have_cc().then(Sysroot::build)
}

/// Snapshot every path under `dir` with its type and size.
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, (String, u64)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path).unwrap();
            let kind = if metadata.is_symlink() {
                "symlink"
            } else if metadata.is_dir() {
                stack.push(path.clone());
                "dir"
            } else {
                "file"
            };
            let relative = path.strip_prefix(dir).unwrap().to_path_buf();
            out.insert(relative, (kind.to_string(), metadata.len()));
        }
    }
    out
}

#[test]
fn materializes_files_symlinks_and_directories() {
    let Some(sysroot) = sysroot() else { return };
    let output = tempfile::tempdir().unwrap();
    let rootfs = output.path().join("rootfs");

    let plan = Planner::new(
        SourceRoot::new(&sysroot.root),
        sysroot.path("/bin/app-default"),
    )
    .install_as("/app/server")
    .plan()
    .unwrap();
    let before = snapshot(&sysroot.root);
    let report = RootFsBuilder::new(&rootfs).apply(&plan).unwrap();

    assert!(report.files >= 3, "{report:?}");
    assert!(rootfs.join("app/server").is_file());

    let link = rootfs.join("usr/lib/libbase.so.1");
    let metadata = std::fs::symlink_metadata(&link).unwrap();
    assert!(metadata.is_symlink(), "soname link must stay a link");
    assert_eq!(
        std::fs::read_link(&link).unwrap(),
        PathBuf::from("libbase.so.1.4.2")
    );
    assert!(rootfs.join("usr/lib/libbase.so.1.4.2").is_file());

    // The executable's bytes are copied verbatim.
    assert_eq!(
        std::fs::read(rootfs.join("app/server")).unwrap(),
        std::fs::read(sysroot.path("/bin/app-default")).unwrap()
    );

    // The source root is never modified.
    assert_eq!(before, snapshot(&sysroot.root));
}

#[test]
fn output_is_reproducible() {
    let Some(sysroot) = sysroot() else { return };
    let output = tempfile::tempdir().unwrap();
    let plan = Planner::new(
        SourceRoot::new(&sysroot.root),
        sysroot.path("/bin/app-default"),
    )
    .install_as("/app/server")
    .plan()
    .unwrap();

    let first = output.path().join("a");
    let second = output.path().join("b");
    RootFsBuilder::new(&first).apply(&plan).unwrap();
    RootFsBuilder::new(&second).apply(&plan).unwrap();
    assert_eq!(snapshot(&first), snapshot(&second));
}

#[test]
fn manifest_records_every_file_and_verifies() {
    let Some(sysroot) = sysroot() else { return };
    let output = tempfile::tempdir().unwrap();
    let rootfs = output.path().join("rootfs");

    let plan = Planner::new(
        SourceRoot::new(&sysroot.root),
        sysroot.path("/bin/app-default"),
    )
    .install_as("/app/server")
    .plan()
    .unwrap();
    RootFsBuilder::new(&rootfs).apply(&plan).unwrap();

    let manifest = Manifest::from_plan(&plan, &sysroot.root, Some(&rootfs));
    let path = output.path().join("elfpak-manifest.json");
    manifest.write(&path).unwrap();

    // The manifest lives beside the rootfs, never inside it.
    assert!(!rootfs.join("elfpak-manifest.json").exists());

    let loaded = Manifest::load(&path).unwrap();
    assert_eq!(loaded.binary, "/app/server");
    assert_eq!(loaded.files.len(), plan.files.len());
    assert!(loaded.verify(&rootfs).is_ok());

    // Tampering is detected.
    std::fs::write(rootfs.join("app/server"), b"nope").unwrap();
    let report = loaded.verify(&rootfs);
    assert!(!report.is_ok());
    assert!(report.problems.iter().any(|p| p.path == "/app/server"));
}

#[test]
fn install_paths_cannot_escape_the_output_directory() {
    let Some(sysroot) = sysroot() else { return };
    let output = tempfile::tempdir().unwrap();
    let rootfs = output.path().join("rootfs");

    let plan = Planner::new(
        SourceRoot::new(&sysroot.root),
        sysroot.path("/bin/app-default"),
    )
    .install_as("/../../etc/evil")
    .plan()
    .unwrap();
    assert_eq!(plan.executable.destination, PathBuf::from("/etc/evil"));

    RootFsBuilder::new(&rootfs).apply(&plan).unwrap();
    assert!(rootfs.join("etc/evil").is_file());
    assert!(!output.path().join("etc/evil").exists());
}

#[test]
fn clean_replaces_a_previous_rootfs() {
    let Some(sysroot) = sysroot() else { return };
    let output = tempfile::tempdir().unwrap();
    let rootfs = output.path().join("rootfs");
    std::fs::create_dir_all(rootfs.join("stale")).unwrap();
    std::fs::write(rootfs.join("stale/file"), b"old").unwrap();

    let plan = Planner::new(
        SourceRoot::new(&sysroot.root),
        sysroot.path("/bin/app-default"),
    )
    .install_as("/app/server")
    .plan()
    .unwrap();
    RootFsBuilder::new(&rootfs)
        .clean(true)
        .apply(&plan)
        .unwrap();

    assert!(!rootfs.join("stale").exists());
    assert!(rootfs.join("app/server").is_file());
}

#[test]
fn statically_linked_binaries_bundle_to_just_themselves() {
    if !have_cc() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("static.c");
    std::fs::write(&source, "int main(void) { return 0; }\n").unwrap();
    let binary = tmp.path().join("static-app");

    let built = std::process::Command::new("cc")
        .args(["-static", "-o"])
        .arg(&binary)
        .arg(&source)
        .output()
        .expect("cc runs");
    if !built.status.success() {
        // No static libc available in this environment.
        return;
    }

    let rootfs = tmp.path().join("rootfs");
    let plan = Planner::new(SourceRoot::new("/"), &binary)
        .install_as("/app/server")
        .plan()
        .expect("a static binary needs no closure");
    assert!(plan.interpreter.is_none());
    assert_eq!(plan.graph.nodes.len(), 1);

    RootFsBuilder::new(&rootfs).apply(&plan).unwrap();
    assert!(rootfs.join("app/server").is_file());
}
