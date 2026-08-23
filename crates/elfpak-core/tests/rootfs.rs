//! Materialization, manifest and filesystem-safety tests.

mod common;

use common::{Sysroot, have_cc};
use elfpak_core::{
    LdCache, Manifest, ManifestImage, ManifestOutputs, OciLayoutBuilder, PlannedFileKind, Planner,
    RootFsBuilder, SourceRoot, manifest::VerifyOptions,
};
use std::{
    collections::BTreeMap,
    ffi::OsStr,
    path::{Path, PathBuf},
};

fn sysroot() -> Option<Sysroot> {
    have_cc().then(Sysroot::build)
}

/// Run an environment-sensitive test body in a child test process so parallel
/// tests never mutate or inherit `SOURCE_DATE_EPOCH` accidentally.
fn isolated_source_date_epoch(test_name: &str, value: Option<&str>) -> bool {
    const MARKER: &str = "ELFPAK_SOURCE_DATE_EPOCH_TEST";
    if std::env::var_os(MARKER).as_deref() == Some(OsStr::new(test_name)) {
        return true;
    }

    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([test_name, "--exact", "--nocapture"])
        .env(MARKER, test_name);
    if let Some(value) = value {
        command.env("SOURCE_DATE_EPOCH", value);
    } else {
        command.env_remove("SOURCE_DATE_EPOCH");
    }

    let output = command.output().expect("child test process runs");
    assert!(
        output.status.success(),
        "child test failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    false
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
fn multi_executable_plan_contains_every_application_and_shared_file_once() {
    let Some(sysroot) = sysroot() else { return };
    let output = tempfile::tempdir().unwrap();
    let rootfs = output.path().join("rootfs");

    let plan = Planner::new(
        SourceRoot::new(&sysroot.root),
        sysroot.path("/bin/app-default"),
    )
    .install_as("/app/default")
    .add_binary(sysroot.path("/bin/app-rpath"), "/app/rpath")
    .plan()
    .unwrap();

    let destinations: Vec<_> = plan
        .executables()
        .map(|file| file.destination().to_path_buf())
        .collect();
    assert_eq!(
        destinations,
        [PathBuf::from("/app/default"), PathBuf::from("/app/rpath")]
    );
    assert_eq!(plan.applications().len(), 2);
    assert_eq!(
        plan.files()
            .iter()
            .filter(|file| {
                file.kind() == PlannedFileKind::SharedObject
                    && file.destination() == Path::new("/usr/lib/libbase.so.1.4.2")
            })
            .count(),
        1,
        "a shared dependency has one materialized entry"
    );

    RootFsBuilder::new(&rootfs).apply(&plan).unwrap();
    assert!(rootfs.join("app/default").is_file());
    assert!(rootfs.join("app/rpath").is_file());
}

#[test]
fn multi_executable_plan_rejects_duplicate_install_destinations() {
    let Some(sysroot) = sysroot() else { return };

    let error = Planner::new(
        SourceRoot::new(&sysroot.root),
        sysroot.path("/bin/app-default"),
    )
    .install_as("/app/server")
    .add_binary(sysroot.path("/bin/app-rpath"), "/app/server")
    .plan()
    .unwrap_err();

    assert!(error.to_string().contains("/app/server"), "{error}");
}

#[test]
fn multi_executable_loader_cache_names_libraries_from_every_closure() {
    let Some(sysroot) = sysroot() else { return };
    let output = tempfile::tempdir().unwrap();
    let rootfs = output.path().join("rootfs");

    let plan = Planner::new(
        SourceRoot::new(&sysroot.root),
        sysroot.path("/bin/app-default"),
    )
    .install_as("/app/default")
    .add_binary(sysroot.path("/bin/app-cached"), "/app/cached")
    .plan()
    .unwrap();
    RootFsBuilder::new(&rootfs).apply(&plan).unwrap();

    let cache = LdCache::parse(&std::fs::read(rootfs.join("etc/ld.so.cache")).unwrap());
    assert!(
        cache
            .lookup("libbase.so.1")
            .contains(&PathBuf::from("/usr/lib/libbase.so.1.4.2"))
    );
    assert!(
        cache
            .lookup("libcached.so.1")
            .contains(&PathBuf::from("/opt/cached/libcached.so.1"))
    );
}

#[test]
fn multi_executable_plan_rejects_a_cross_application_library_collision() {
    let Some(sysroot) = sysroot() else { return };

    let error = Planner::new(
        SourceRoot::new(&sysroot.root),
        sysroot.path("/bin/app-default"),
    )
    .install_as("/app/default")
    .add_binary(sysroot.path("/bin/app-rpath"), "/usr/lib/libtop.so.1")
    .plan()
    .unwrap_err();

    let message = error.to_string();
    assert!(message.contains("/usr/lib/libtop.so.1"), "{message}");
    assert!(message.contains("collides"), "{message}");
}

/// One file legitimately reaches the plan under two kinds: `--tzdata` copies
/// the zone database, and `/etc/localtime` is a symlink into it. That is the
/// same file planned twice, not two entries contesting a destination.
#[test]
fn tzdata_survives_a_localtime_symlink_into_the_zone_database() {
    let Some(sysroot) = sysroot() else { return };
    std::fs::create_dir_all(sysroot.path("/usr/share/zoneinfo/Etc")).unwrap();
    std::fs::write(sysroot.path("/usr/share/zoneinfo/Etc/UTC"), b"TZif2fixture").unwrap();
    std::os::unix::fs::symlink(
        "/usr/share/zoneinfo/Etc/UTC",
        sysroot.path("/etc/localtime"),
    )
    .unwrap();

    let policy = elfpak_core::RuntimePolicy {
        tzdata: true,
        ..Default::default()
    };
    let plan = Planner::new(
        SourceRoot::new(&sysroot.root),
        sysroot.path("/bin/app-default"),
    )
    .runtime_policy(policy)
    .plan()
    .expect("a localtime symlink into the zone database is not a conflict");

    let zone = plan
        .files()
        .iter()
        .find(|file| file.destination() == Path::new("/usr/share/zoneinfo/Etc/UTC"))
        .expect("the zone file is planned once");
    assert_eq!(zone.size(), b"TZif2fixture".len() as u64);
    assert!(
        plan.files()
            .iter()
            .any(|file| file.destination() == Path::new("/etc/localtime")
                && file.kind() == PlannedFileKind::Symlink),
        "the link itself is preserved"
    );
}

/// `--include` of a library directory is the documented escape hatch for
/// `dlopen`, so it routinely covers objects the closure already planned.
#[test]
fn an_include_may_overlap_the_closure() {
    let Some(sysroot) = sysroot() else { return };
    let policy = elfpak_core::RuntimePolicy {
        includes: vec![PathBuf::from("/usr/lib")],
        ..Default::default()
    };
    let plan = Planner::new(
        SourceRoot::new(&sysroot.root),
        sysroot.path("/bin/app-default"),
    )
    .runtime_policy(policy)
    .plan()
    .expect("an include overlapping the closure is not a conflict");

    let library = Path::new("/usr/lib/libbase.so.1.4.2");
    assert_eq!(
        plan.files()
            .iter()
            .filter(|file| file.destination() == library)
            .count(),
        1,
        "the shared object is planned exactly once"
    );
}

#[test]
fn a_runtime_directory_cannot_silently_lose_to_an_executable() {
    let Some(sysroot) = sysroot() else { return };
    let policy = elfpak_core::RuntimePolicy {
        tmp: true,
        ..Default::default()
    };

    let error = Planner::new(
        SourceRoot::new(&sysroot.root),
        sysroot.path("/bin/app-default"),
    )
    .install_as("/tmp")
    .runtime_policy(policy)
    .plan()
    .expect_err("one path cannot be both the application and the requested /tmp directory");

    assert_eq!(error.code(), "E4001");
    assert!(error.to_string().contains("/tmp"), "{error}");
}

#[test]
fn a_runtime_directory_keeps_precedence_over_an_included_file() {
    let Some(sysroot) = sysroot() else { return };
    std::fs::write(sysroot.path("/tmp"), b"not a directory").unwrap();
    let policy = elfpak_core::RuntimePolicy {
        tmp: true,
        includes: vec![PathBuf::from("/tmp")],
        ..Default::default()
    };

    let plan = Planner::new(
        SourceRoot::new(&sysroot.root),
        sysroot.path("/bin/app-default"),
    )
    .runtime_policy(policy)
    .plan()
    .expect("runtime policy has documented precedence over an include tree");

    let tmp = plan
        .files()
        .iter()
        .find(|file| file.destination() == Path::new("/tmp"))
        .expect("/tmp is planned");
    assert_eq!(tmp.kind(), PlannedFileKind::Directory);
    assert_eq!(tmp.mode(), 0o1777);
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
    assert_eq!(loaded.binaries, ["/app/server"]);
    assert_eq!(loaded.files.len(), plan.files().len());
    assert!(loaded.verify(&rootfs, &VerifyOptions::default()).is_ok());

    // Tampering is detected.
    std::fs::write(rootfs.join("app/server"), b"nope").unwrap();
    let report = loaded.verify(&rootfs, &VerifyOptions::default());
    assert!(!report.is_ok());
    assert!(report.problems.iter().any(|p| p.path == "/app/server"));
}

#[test]
fn version_three_manifest_requires_its_complete_binary_list() {
    let Some(sysroot) = sysroot() else { return };
    let output = tempfile::tempdir().unwrap();
    let path = output.path().join("elfpak-manifest.json");
    let plan = Planner::new(
        SourceRoot::new(&sysroot.root),
        sysroot.path("/bin/app-default"),
    )
    .install_as("/app/server")
    .plan()
    .unwrap();
    let mut manifest = Manifest::from_plan(&plan, &sysroot.root, None);
    manifest.binaries.clear();
    manifest.write(&path).unwrap();

    let error = Manifest::load(&path).unwrap_err();
    assert!(error.to_string().contains("binaries"), "{error}");
}

#[test]
fn verification_checks_a_recorded_file_size() {
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

    let mut manifest = Manifest::from_plan(&plan, &sysroot.root, Some(&rootfs));
    let application = manifest
        .files
        .iter_mut()
        .find(|file| file.path == "/app/server")
        .unwrap();
    application.size += 1;

    let report = manifest.verify(&rootfs, &VerifyOptions::default());
    assert!(
        report
            .problems
            .iter()
            .any(|problem| problem.path == "/app/server" && problem.detail.contains("size is"))
    );
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
    assert_eq!(plan.executable().destination(), PathBuf::from("/etc/evil"));

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
fn non_clean_build_preserves_unplanned_files() {
    let Some(sysroot) = sysroot() else { return };
    let output = tempfile::tempdir().unwrap();
    let rootfs = output.path().join("rootfs");
    std::fs::create_dir_all(rootfs.join("stale")).unwrap();
    std::fs::write(rootfs.join("stale/file"), b"keep").unwrap();

    let plan = Planner::new(
        SourceRoot::new(&sysroot.root),
        sysroot.path("/bin/app-default"),
    )
    .install_as("/app/server")
    .plan()
    .unwrap();
    RootFsBuilder::new(&rootfs).apply(&plan).unwrap();

    assert_eq!(std::fs::read(rootfs.join("stale/file")).unwrap(), b"keep");
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
    assert!(plan.interpreter().is_none());
    assert_eq!(plan.graph().nodes.len(), 1);

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
fn tar_stream_matches_the_file_builder() {
    if !isolated_source_date_epoch("tar_stream_matches_the_file_builder", None) {
        return;
    }
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
    let builder = elfpak_core::TarBuilder::new(&archive);

    let file_report = builder.apply(&plan).unwrap();
    let (stream, stream_report) = builder.write_to(Vec::new(), &plan).unwrap();

    assert_eq!(stream, std::fs::read(&archive).unwrap());
    assert_eq!(stream_report.files, file_report.files);
    assert_eq!(stream_report.directories, file_report.directories);
    assert_eq!(stream_report.symlinks, file_report.symlinks);
    assert_eq!(stream_report.bytes, file_report.bytes);
}

#[test]
fn tar_metadata_is_pinned_and_paths_are_relative() {
    if !isolated_source_date_epoch("tar_metadata_is_pinned_and_paths_are_relative", None) {
        return;
    }
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
    assert!(manifest.binaries.is_empty());
    assert_eq!(manifest.files.len(), 1);
    assert!(manifest.policy.preset.is_none());
    assert!(!manifest.policy.tmp);
}

#[test]
fn manifest_versions_one_through_three_load_without_oci_fields() {
    let Some(sysroot) = sysroot() else { return };
    let temp = tempfile::tempdir().unwrap();
    let plan = Planner::new(
        SourceRoot::new(&sysroot.root),
        sysroot.path("/bin/app-default"),
    )
    .install_as("/app/server")
    .plan()
    .unwrap();
    let manifest = Manifest::from_plan(&plan, &sysroot.root, None);

    for version in 1..=3 {
        let path = temp.path().join(format!("v{version}.json"));
        let mut value = serde_json::to_value(&manifest).unwrap();
        value["manifest_version"] = serde_json::json!(version);
        let object = value.as_object_mut().unwrap();
        object.remove("oci_layout");
        object.remove("oci_archive");
        object.remove("image");
        if version < 3 {
            object.remove("binaries");
        }
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();

        let loaded = Manifest::load(&path).unwrap();
        assert_eq!(loaded.manifest_version, version);
        assert!(loaded.oci_layout.is_none());
        assert!(loaded.oci_archive.is_none());
        assert!(loaded.image.is_none());
    }
}

#[test]
fn version_four_requires_consistent_oci_image_metadata() {
    let Some(sysroot) = sysroot() else { return };
    let temp = tempfile::tempdir().unwrap();
    let layout = temp.path().join("image");
    let plan = Planner::new(
        SourceRoot::new(&sysroot.root),
        sysroot.path("/bin/app-default"),
    )
    .install_as("/app/server")
    .plan()
    .unwrap();
    let report = OciLayoutBuilder::new(&layout).apply(&plan).unwrap();
    let manifest = Manifest::from_plan_with_artifacts(
        &plan,
        &sysroot.root,
        ManifestOutputs {
            oci_layout: Some(&layout),
            ..ManifestOutputs::default()
        },
        Some(ManifestImage::from_oci(
            report.image(),
            report.manifest_digest(),
        )),
    );
    let valid = temp.path().join("valid.json");
    manifest.write(&valid).unwrap();
    Manifest::load(&valid).unwrap();

    let mut cases = Vec::new();
    let mut missing_destination = serde_json::to_value(&manifest).unwrap();
    missing_destination
        .as_object_mut()
        .unwrap()
        .remove("oci_layout");
    cases.push((missing_destination, "destinations"));
    let mut missing_image = serde_json::to_value(&manifest).unwrap();
    missing_image.as_object_mut().unwrap().remove("image");
    cases.push((missing_image, "metadata"));
    let mut invalid_digest = serde_json::to_value(&manifest).unwrap();
    invalid_digest["image"]["manifest_digest"] = serde_json::json!("sha256:ABC");
    cases.push((invalid_digest, "manifest_digest"));

    for (index, (value, expected)) in cases.into_iter().enumerate() {
        let path = temp.path().join(format!("invalid-{index}.json"));
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        let error = Manifest::load(&path).unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
    }
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
fn materialization_rejects_a_source_file_changed_after_planning() {
    let Some(sysroot) = sysroot() else { return };
    let output = tempfile::tempdir().unwrap();
    let rootfs = output.path().join("rootfs");
    std::fs::create_dir_all(&rootfs).unwrap();
    std::fs::write(rootfs.join("previous"), b"previous rootfs").unwrap();
    let rootfs_before = snapshot(&rootfs);
    let binary = sysroot.path("/bin/app-default");
    let plan = Planner::new(SourceRoot::new(&sysroot.root), &binary)
        .install_as("/app/server")
        .plan()
        .unwrap();

    std::fs::write(&binary, b"changed after planning").unwrap();
    let error = RootFsBuilder::new(&rootfs)
        .clean(true)
        .apply(&plan)
        .unwrap_err();
    assert!(matches!(error, elfpak_core::Error::SourceChanged { .. }));
    assert_eq!(
        snapshot(&rootfs),
        rootfs_before,
        "a failed clean build must leave the previous rootfs untouched"
    );
    assert_eq!(
        std::fs::read(rootfs.join("previous")).unwrap(),
        b"previous rootfs"
    );

    let absent_rootfs = output.path().join("absent-rootfs");
    let error = RootFsBuilder::new(&absent_rootfs).apply(&plan).unwrap_err();
    assert!(matches!(error, elfpak_core::Error::SourceChanged { .. }));
    assert!(
        !absent_rootfs.exists(),
        "a failed build must not publish a partial new rootfs"
    );

    let archive = output.path().join("bundle.tar");
    std::fs::write(&archive, b"previous archive").unwrap();
    let error = elfpak_core::TarBuilder::new(&archive)
        .apply(&plan)
        .unwrap_err();
    assert!(matches!(error, elfpak_core::Error::SourceChanged { .. }));
    assert_eq!(
        std::fs::read(&archive).unwrap(),
        b"previous archive",
        "a failed build must not truncate the previous archive"
    );

    let absent_archive = output.path().join("absent.tar");
    let error = elfpak_core::TarBuilder::new(&absent_archive)
        .apply(&plan)
        .unwrap_err();
    assert!(matches!(error, elfpak_core::Error::SourceChanged { .. }));
    assert!(
        !absent_archive.exists(),
        "a failed build must not publish a partial new archive"
    );

    let temporary_outputs: Vec<_> = std::fs::read_dir(output.path())
        .unwrap()
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(".elfpak-"))
        .collect();
    assert!(
        temporary_outputs.is_empty(),
        "failed builds clean up their staging paths"
    );
}

#[test]
fn directory_timestamps_default_to_materialization_time() {
    if !isolated_source_date_epoch("directory_timestamps_default_to_materialization_time", None) {
        return;
    }
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
    let earliest = std::time::SystemTime::now() - std::time::Duration::from_secs(2);
    RootFsBuilder::new(&rootfs).apply(&plan).unwrap();
    let latest = std::time::SystemTime::now() + std::time::Duration::from_secs(2);

    let entries = [
        rootfs.join("app/server"),
        rootfs.join("app"),
        rootfs.join("usr/lib"),
    ];
    let timestamp = std::fs::metadata(&entries[0]).unwrap().modified().unwrap();
    for entry in entries {
        let modified = std::fs::metadata(&entry).unwrap().modified().unwrap();
        assert_eq!(
            modified,
            timestamp,
            "{} has a different time",
            entry.display()
        );
        assert!(
            (earliest..=latest).contains(&modified),
            "{} has {modified:?}, outside the materialization window {earliest:?}..={latest:?}",
            entry.display()
        );
    }
}

#[test]
fn directory_timestamps_honor_source_date_epoch() {
    const EPOCH: u64 = 1_234_567_890;
    if !isolated_source_date_epoch(
        "directory_timestamps_honor_source_date_epoch",
        Some("1234567890"),
    ) {
        return;
    }
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

    let expected = std::time::UNIX_EPOCH + std::time::Duration::from_secs(EPOCH);
    for entry in [
        rootfs.join("app/server"),
        rootfs.join("app"),
        rootfs.join("usr/lib"),
    ] {
        assert_eq!(
            std::fs::metadata(&entry).unwrap().modified().unwrap(),
            expected,
            "{} does not honor SOURCE_DATE_EPOCH",
            entry.display()
        );
    }
}

#[test]
fn invalid_source_date_epoch_fails_before_directory_publication() {
    if !isolated_source_date_epoch(
        "invalid_source_date_epoch_fails_before_directory_publication",
        Some("not-a-timestamp"),
    ) {
        return;
    }
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
    let error = RootFsBuilder::new(&rootfs).apply(&plan).unwrap_err();

    assert!(error.to_string().contains("SOURCE_DATE_EPOCH"), "{error}");
    assert!(!rootfs.exists(), "invalid input must publish no directory");
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
