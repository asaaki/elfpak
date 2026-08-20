//! Materialization, manifest and filesystem-safety tests.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use common::{Sysroot, have_cc};
use elfpak_core::manifest::VerifyOptions;
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
    assert!(loaded.verify(&rootfs, &VerifyOptions::default()).is_ok());

    // Tampering is detected.
    std::fs::write(rootfs.join("app/server"), b"nope").unwrap();
    let report = loaded.verify(&rootfs, &VerifyOptions::default());
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

/// One entry of a tar archive, reduced to the metadata that must be pinned.
struct TarEntry {
    name: String,
    /// `d` directory, `l` symlink, `-` regular file.
    kind: char,
    mode: u32,
    uid: u64,
    gid: u64,
    mtime: u64,
    link_target: Option<String>,
}

fn tar_entries(path: &Path) -> Vec<TarEntry> {
    let file = std::fs::File::open(path).unwrap();
    let mut archive = tar::Archive::new(file);
    archive
        .entries()
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let header = entry.header();
            TarEntry {
                name: entry.path().unwrap().display().to_string(),
                kind: match header.entry_type() {
                    tar::EntryType::Directory => 'd',
                    tar::EntryType::Symlink => 'l',
                    _ => '-',
                },
                mode: header.mode().unwrap(),
                uid: header.uid().unwrap(),
                gid: header.gid().unwrap(),
                mtime: header.mtime().unwrap(),
                link_target: entry.link_name().unwrap().map(|p| p.display().to_string()),
            }
        })
        .collect()
}

#[test]
fn tar_archive_describes_the_same_tree_as_the_directory() {
    let Some(sysroot) = sysroot() else { return };
    let output = tempfile::tempdir().unwrap();
    let rootfs = output.path().join("rootfs");
    let archive = output.path().join("rootfs.tar");

    let mut policy = elfpak_core::RuntimePolicy::from_preset(elfpak_core::Preset::Web);
    policy.ca_certificates = false; // the fixture sysroot has no CA bundle
    let plan = Planner::new(
        SourceRoot::new(&sysroot.root),
        sysroot.path("/bin/app-default"),
    )
    .install_as("/app/server")
    .runtime_policy(policy)
    .plan()
    .unwrap();

    RootFsBuilder::new(&rootfs).apply(&plan).unwrap();
    let report = elfpak_core::TarBuilder::new(&archive).apply(&plan).unwrap();
    assert!(report.files >= 3 && report.symlinks >= 1);

    // Unpacking the archive must reproduce the directory output exactly.
    let unpacked = output.path().join("unpacked");
    std::fs::create_dir_all(&unpacked).unwrap();
    tar::Archive::new(std::fs::File::open(&archive).unwrap())
        .unpack(&unpacked)
        .unwrap();
    assert_eq!(snapshot(&rootfs), snapshot(&unpacked));
}

#[test]
fn tar_metadata_is_pinned_and_paths_are_relative() {
    let Some(sysroot) = sysroot() else { return };
    let output = tempfile::tempdir().unwrap();
    let archive = output.path().join("rootfs.tar");

    let plan = Planner::new(
        SourceRoot::new(&sysroot.root),
        sysroot.path("/bin/app-default"),
    )
    .install_as("/app/server")
    .plan()
    .unwrap();
    elfpak_core::TarBuilder::new(&archive).apply(&plan).unwrap();

    let entries = tar_entries(&archive);
    assert!(!entries.is_empty());
    for entry in &entries {
        let name = &entry.name;
        assert!(!name.starts_with('/'), "tar paths are relative: {name}");
        assert_eq!(entry.uid, 0, "{name} is owned by root");
        assert_eq!(entry.gid, 0, "{name} is owned by root");
        assert_eq!(entry.mtime, 0, "{name} has a pinned timestamp");
    }

    let executable = entries
        .iter()
        .find(|entry| entry.name == "app/server")
        .expect("the executable is in the archive");
    assert_eq!(executable.kind, '-');
    assert_eq!(executable.mode, 0o755);

    let link = entries
        .iter()
        .find(|entry| entry.name == "usr/lib/libbase.so.1")
        .expect("the soname symlink is in the archive");
    assert_eq!(link.kind, 'l');
    assert_eq!(link.link_target.as_deref(), Some("libbase.so.1.4.2"));

    // Directories are emitted with a trailing slash, as extractors expect.
    assert!(
        entries
            .iter()
            .any(|entry| entry.name == "app/" && entry.kind == 'd')
    );
}

#[test]
fn tar_output_is_reproducible() {
    let Some(sysroot) = sysroot() else { return };
    let output = tempfile::tempdir().unwrap();
    let plan = Planner::new(
        SourceRoot::new(&sysroot.root),
        sysroot.path("/bin/app-default"),
    )
    .install_as("/app/server")
    .plan()
    .unwrap();

    let first = output.path().join("first.tar");
    let second = output.path().join("second.tar");
    elfpak_core::TarBuilder::new(&first).apply(&plan).unwrap();
    elfpak_core::TarBuilder::new(&second).apply(&plan).unwrap();

    assert_eq!(
        std::fs::read(&first).unwrap(),
        std::fs::read(&second).unwrap(),
        "the same plan must produce a byte-identical archive"
    );
}

#[test]
fn manifest_records_the_resolved_policy() {
    let Some(sysroot) = sysroot() else { return };
    let output = tempfile::tempdir().unwrap();

    let mut policy = elfpak_core::RuntimePolicy::from_preset(elfpak_core::Preset::Web);
    policy.ca_certificates = false;
    policy.user = Some(elfpak_core::UserSpec::parse("65532:65532").unwrap());
    let plan = Planner::new(
        SourceRoot::new(&sysroot.root),
        sysroot.path("/bin/app-default"),
    )
    .preset(elfpak_core::Preset::Web)
    .install_as("/app/server")
    .runtime_policy(policy)
    .dependency_policy(elfpak_core::DependencyPolicy::allow_list(vec![
        "libtop.so.1".into(),
        "libbase.so.1".into(),
    ]))
    .plan()
    .unwrap();

    let path = output.path().join("elfpak-manifest.json");
    Manifest::from_plan(&plan, &sysroot.root, None)
        .write(&path)
        .unwrap();
    let loaded = Manifest::load(&path).unwrap();

    assert_eq!(loaded.policy.preset.as_deref(), Some("web"));
    assert_eq!(loaded.policy.user.as_deref(), Some("app:65532:65532"));
    assert!(loaded.policy.tmp && loaded.policy.nsswitch);
    assert!(!loaded.policy.tzdata && !loaded.policy.ca_certificates);
    assert_eq!(
        loaded.policy.allow_libraries.as_deref(),
        Some(["libtop.so.1".to_string(), "libbase.so.1".to_string()].as_slice())
    );
}

#[test]
fn strict_verification_detects_files_the_manifest_does_not_list() {
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

    let strict = VerifyOptions { strict: true };
    assert!(manifest.verify(&rootfs, &strict).is_ok());

    std::fs::create_dir_all(rootfs.join("opt/payload")).unwrap();
    std::fs::write(rootfs.join("opt/payload/extra.so"), b"smuggled").unwrap();

    // The default mode only proves nothing was removed or altered.
    assert!(manifest.verify(&rootfs, &VerifyOptions::default()).is_ok());

    let report = manifest.verify(&rootfs, &strict);
    assert!(!report.is_ok());
    assert_eq!(report.unexpected, 3, "{:?}", report.problems);
    assert!(
        report
            .problems
            .iter()
            .any(|p| p.path == "/opt/payload/extra.so")
    );
}

#[test]
fn manifests_without_a_policy_section_still_load() {
    // Version 1 manifests predate the recorded policy. Reading one must keep
    // working, otherwise `verify` breaks for bundles built by an older release.
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("elfpak-manifest.json");
    std::fs::write(
        &path,
        r#"{
  "manifest_version": 1,
  "elfpak_version": "0.1.0",
  "binary": "/app/server",
  "architecture": "x86_64",
  "source_root": "/",
  "files": [
    { "path": "/app/server", "kind": "executable", "reason": "application",
      "sha256": "00", "size": 1, "mode": "0755" }
  ]
}"#,
    )
    .unwrap();

    let manifest = Manifest::load(&path).unwrap();
    assert_eq!(manifest.manifest_version, 1);
    assert_eq!(manifest.files.len(), 1);
    assert!(manifest.policy.preset.is_none());
    assert!(!manifest.policy.tmp);
}

#[test]
fn a_leftover_symlink_never_redirects_a_write() {
    let Some(sysroot) = sysroot() else { return };
    let output = tempfile::tempdir().unwrap();
    let rootfs = output.path().join("rootfs");
    let outside = output.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("victim"), b"untouched").unwrap();

    // A previous run — or anything else — may have left a symlink where an
    // entry is about to be written. Writing onto it would follow it out of the
    // output root, which the guarantee "writes only beneath --output" forbids.
    std::fs::create_dir_all(rootfs.join("app")).unwrap();
    std::os::unix::fs::symlink(outside.join("victim"), rootfs.join("app/server")).unwrap();
    std::fs::create_dir_all(rootfs.join("usr")).unwrap();
    std::os::unix::fs::symlink(&outside, rootfs.join("usr/lib")).unwrap();

    let plan = Planner::new(
        SourceRoot::new(&sysroot.root),
        sysroot.path("/bin/app-default"),
    )
    .install_as("/app/server")
    .plan()
    .unwrap();
    RootFsBuilder::new(&rootfs).apply(&plan).unwrap();

    assert_eq!(
        std::fs::read(outside.join("victim")).unwrap(),
        b"untouched",
        "the write escaped the output root"
    );
    assert!(
        !std::fs::symlink_metadata(rootfs.join("app/server"))
            .unwrap()
            .is_symlink(),
        "the stale link was replaced by the real file"
    );
    assert!(rootfs.join("usr/lib/libtop.so.1").is_file());
    assert!(
        !outside.join("libtop.so.1").exists(),
        "a symlinked directory must not become a write channel"
    );

    // Everything written is exactly what a run into a fresh directory produces.
    let fresh = output.path().join("fresh");
    RootFsBuilder::new(&fresh).apply(&plan).unwrap();
    assert_eq!(snapshot(&rootfs), snapshot(&fresh));
}

#[test]
fn timestamps_are_pinned_for_files_and_directories() {
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

    for entry in [rootfs.join("app/server"), rootfs.join("app"), rootfs.join("usr/lib")] {
        let modified = std::fs::metadata(&entry).unwrap().modified().unwrap();
        assert_eq!(
            modified,
            std::time::UNIX_EPOCH,
            "{} keeps a build-time timestamp",
            entry.display()
        );
    }
}

#[test]
fn strict_verification_detects_a_changed_mode() {
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
    let strict = VerifyOptions { strict: true };
    assert!(manifest.verify(&rootfs, &strict).is_ok());

    // setuid is invisible to a content digest.
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        rootfs.join("app/server"),
        std::fs::Permissions::from_mode(0o4755),
    )
    .unwrap();

    assert!(
        manifest.verify(&rootfs, &VerifyOptions::default()).is_ok(),
        "the default mode only covers content"
    );
    let report = manifest.verify(&rootfs, &strict);
    assert!(!report.is_ok());
    assert!(
        report
            .problems
            .iter()
            .any(|p| p.path == "/app/server" && p.detail.contains("4755")),
        "{:?}",
        report.problems
    );
}
