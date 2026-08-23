//! OCI image metadata and materialization tests.

mod common;

use common::{Sysroot, have_cc};
use elfpak_core::{
    BundlePlan, OciArchiveBuilder, OciImageConfig, OciLayoutBuilder, Planner, RootFsBuilder,
    RuntimePolicy, SourceRoot, UserSpec, hash::sha256_file,
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    path::{Path, PathBuf},
};

fn sysroot() -> Option<Sysroot> {
    have_cc().then(Sysroot::build)
}

fn fixture_plan_installed_as(install: &str) -> Option<(Sysroot, BundlePlan)> {
    let sysroot = sysroot()?;
    let plan = Planner::new(
        SourceRoot::new(&sysroot.root),
        sysroot.path("/bin/app-default"),
    )
    .install_as(install)
    .plan()
    .unwrap();
    Some((sysroot, plan))
}

fn fixture_plan_with_user(value: &str) -> Option<(Sysroot, BundlePlan)> {
    let sysroot = sysroot()?;
    let policy = RuntimePolicy {
        user: Some(UserSpec::parse(value).unwrap()),
        ..RuntimePolicy::default()
    };
    let plan = Planner::new(
        SourceRoot::new(&sysroot.root),
        sysroot.path("/bin/app-default"),
    )
    .install_as("/app/server")
    .runtime_policy(policy)
    .plan()
    .unwrap();
    Some((sysroot, plan))
}

fn fixture_multi_plan() -> Option<(Sysroot, BundlePlan)> {
    let sysroot = sysroot()?;
    let plan = Planner::new(
        SourceRoot::new(&sysroot.root),
        sysroot.path("/bin/app-default"),
    )
    .install_as("/app/server")
    .add_binary(sysroot.path("/bin/app-rpath"), "/app/migrate")
    .plan()
    .unwrap();
    Some((sysroot, plan))
}

fn patch_elf_machine(root: &Path, machine: u16) {
    let mut stack = vec![root.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(current).unwrap() {
            let path = entry.unwrap().path();
            let metadata = std::fs::symlink_metadata(&path).unwrap();
            if metadata.is_dir() {
                stack.push(path);
            } else if metadata.is_file() {
                let mut bytes = std::fs::read(&path).unwrap();
                if bytes.starts_with(b"\x7fELF") {
                    bytes[18..20].copy_from_slice(&machine.to_le_bytes());
                    std::fs::write(path, bytes).unwrap();
                }
            }
        }
    }
}

fn assert_config_error(config: OciImageConfig, plan: &BundlePlan, option: &str) {
    let error = config.resolve(plan).unwrap_err();
    assert_eq!(error.code(), "E4001");
    assert!(error.to_string().contains(option), "{error}");
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn blob_path(layout: &Path, digest: &str) -> PathBuf {
    let hex = digest.strip_prefix("sha256:").unwrap();
    assert_eq!(hex.len(), 64);
    assert!(
        hex.bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    );
    layout.join("blobs/sha256").join(hex)
}

fn descriptor_blob(layout: &Path, descriptor: &Value) -> PathBuf {
    let path = blob_path(layout, descriptor["digest"].as_str().unwrap());
    assert_eq!(
        descriptor["size"].as_u64().unwrap(),
        std::fs::metadata(&path).unwrap().len()
    );
    let (digest, _) = sha256_file(&path).unwrap();
    assert_eq!(format!("sha256:{digest}"), descriptor["digest"]);
    path
}

fn assert_descriptor_graph_is_complete_and_hashed(layout: &Path, report: &elfpak_core::OciReport) {
    let index = read_json(&layout.join("index.json"));
    assert_eq!(index["schemaVersion"], 2);
    assert_eq!(
        index["mediaType"],
        "application/vnd.oci.image.index.v1+json"
    );
    assert_eq!(index["manifests"].as_array().unwrap().len(), 1);
    let manifest_descriptor = &index["manifests"][0];
    assert_eq!(
        manifest_descriptor["mediaType"],
        "application/vnd.oci.image.manifest.v1+json"
    );
    assert_eq!(
        manifest_descriptor["annotations"]["org.opencontainers.image.ref.name"],
        "ci-test"
    );
    assert_eq!(manifest_descriptor["platform"]["os"], "linux");
    assert_eq!(manifest_descriptor["platform"]["architecture"], "amd64");
    assert_eq!(
        manifest_descriptor["digest"],
        format!("sha256:{}", report.manifest_digest())
    );
    assert_eq!(manifest_descriptor["size"], report.manifest_size());

    let manifest_path = descriptor_blob(layout, manifest_descriptor);
    let manifest = read_json(&manifest_path);
    assert_eq!(manifest["schemaVersion"], 2);
    assert_eq!(
        manifest["mediaType"],
        "application/vnd.oci.image.manifest.v1+json"
    );
    assert_eq!(manifest["layers"].as_array().unwrap().len(), 1);

    let config_descriptor = &manifest["config"];
    assert_eq!(
        config_descriptor["mediaType"],
        "application/vnd.oci.image.config.v1+json"
    );
    assert_eq!(
        config_descriptor["digest"],
        format!("sha256:{}", report.config_digest())
    );
    assert_eq!(config_descriptor["size"], report.config_size());
    let config_path = descriptor_blob(layout, config_descriptor);

    let layer_descriptor = &manifest["layers"][0];
    assert_eq!(
        layer_descriptor["mediaType"],
        "application/vnd.oci.image.layer.v1.tar"
    );
    assert_eq!(
        layer_descriptor["digest"],
        format!("sha256:{}", report.layer_digest())
    );
    assert_eq!(layer_descriptor["size"], report.layer_size());
    descriptor_blob(layout, layer_descriptor);

    let config = read_json(&config_path);
    assert_eq!(config["architecture"], "amd64");
    assert_eq!(config["os"], "linux");
    assert_eq!(
        config["config"]["Entrypoint"],
        serde_json::json!(["/app/server"])
    );
    assert_eq!(config["config"]["Cmd"], serde_json::json!(["--version"]));
    assert_eq!(config["config"]["WorkingDir"], "/app");
    assert_eq!(
        config["config"]["Env"],
        serde_json::json!(["RUST_LOG=info"])
    );
    assert_eq!(config["config"]["Labels"]["org.example.test"], "true");
    assert_eq!(config["rootfs"]["type"], "layers");
    assert_eq!(
        config["rootfs"]["diff_ids"],
        serde_json::json!([format!("sha256:{}", report.layer_digest())])
    );

    let expected = BTreeSet::from([
        report.layer_digest().to_string(),
        report.config_digest().to_string(),
        report.manifest_digest().to_string(),
    ]);
    let actual: BTreeSet<String> = std::fs::read_dir(layout.join("blobs/sha256"))
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            assert!(entry.file_type().unwrap().is_file());
            entry.file_name().into_string().unwrap()
        })
        .collect();
    assert_eq!(
        actual, expected,
        "all blobs must be reachable from the index"
    );
}

fn layout_snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut snapshot = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    let mut visited = 0usize;
    while let Some(directory) = stack.pop() {
        for entry in std::fs::read_dir(&directory).unwrap() {
            visited += 1;
            assert!(visited <= 10_000, "test layout walk must stay bounded");
            let path = entry.unwrap().path();
            let metadata = std::fs::symlink_metadata(&path).unwrap();
            assert!(
                !metadata.is_symlink(),
                "OCI layouts must not contain symlinks"
            );
            if metadata.is_dir() {
                stack.push(path);
            } else {
                assert!(
                    metadata.is_file(),
                    "OCI layouts contain only files and directories"
                );
                snapshot.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    std::fs::read(path).unwrap(),
                );
            }
        }
    }
    snapshot
}

fn tree_snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut snapshot = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        for entry in std::fs::read_dir(&directory).unwrap() {
            let path = entry.unwrap().path();
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            let metadata = std::fs::symlink_metadata(&path).unwrap();
            if metadata.is_symlink() {
                let mut value = b"symlink:".to_vec();
                value.extend_from_slice(
                    std::fs::read_link(&path)
                        .unwrap()
                        .to_string_lossy()
                        .as_bytes(),
                );
                snapshot.insert(relative, value);
            } else if metadata.is_dir() {
                snapshot.insert(relative, b"directory".to_vec());
                stack.push(path);
            } else {
                let mut value = b"file:".to_vec();
                value.extend_from_slice(&std::fs::read(&path).unwrap());
                snapshot.insert(relative, value);
            }
        }
    }
    snapshot
}

fn isolated_source_date_epoch(test_name: &str, value: &str) -> bool {
    const MARKER: &str = "ELFPAK_OCI_SOURCE_DATE_EPOCH_TEST";
    if std::env::var_os(MARKER).as_deref() == Some(OsStr::new(test_name)) {
        return true;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([test_name, "--exact", "--nocapture"])
        .env(MARKER, test_name)
        .env("SOURCE_DATE_EPOCH", value)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "child test failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    false
}

#[test]
fn singular_image_defaults_are_runnable_and_platform_correct() {
    let Some((_sysroot, plan)) = fixture_plan_installed_as("/app/server") else {
        return;
    };

    let resolved = OciImageConfig::default().resolve(&plan).unwrap();

    assert_eq!(resolved.tag(), "latest");
    assert_eq!(resolved.os(), "linux");
    assert_eq!(resolved.architecture(), "amd64");
    assert_eq!(resolved.entrypoint(), ["/app/server"]);
    assert!(resolved.cmd().is_empty());
    assert_eq!(resolved.working_dir(), "/");
    assert!(resolved.env().is_empty());
    assert!(resolved.labels().is_empty());
    assert_eq!(resolved.user(), None);
}

#[test]
fn aarch64_plan_maps_to_linux_arm64() {
    let Some(sysroot) = sysroot() else { return };
    patch_elf_machine(&sysroot.root, 183);
    let plan = Planner::new(
        SourceRoot::new(&sysroot.root),
        sysroot.path("/bin/app-default"),
    )
    .install_as("/app/server")
    .plan()
    .unwrap();

    let resolved = OciImageConfig::default().resolve(&plan).unwrap();

    assert_eq!(resolved.os(), "linux");
    assert_eq!(resolved.architecture(), "arm64");
}

#[test]
fn planned_user_becomes_numeric_oci_user() {
    let Some((_sysroot, plan)) = fixture_plan_with_user("service:1234:5678") else {
        return;
    };

    let resolved = OciImageConfig::default().resolve(&plan).unwrap();

    assert_eq!(resolved.user(), Some("1234:5678"));
}

#[test]
fn explicit_process_metadata_preserves_argument_boundaries() {
    let Some((_sysroot, plan)) = fixture_plan_installed_as("/app/server") else {
        return;
    };
    let config = OciImageConfig {
        tag: "ci-test_1.2".to_string(),
        entrypoint: vec![
            "/app/server".to_string(),
            "--mode".to_string(),
            "two words".to_string(),
        ],
        cmd: vec!["--listen".to_string(), "0.0.0.0:8080".to_string()],
        working_dir: Some("/app".to_string()),
        env: vec!["RUST_LOG=info".to_string(), "EMPTY=".to_string()],
        labels: BTreeMap::from([
            ("org.example.revision".to_string(), "abc123".to_string()),
            ("org.example.source".to_string(), "repository".to_string()),
        ]),
    };

    let resolved = config.resolve(&plan).unwrap();

    assert_eq!(resolved.tag(), "ci-test_1.2");
    assert_eq!(
        resolved.entrypoint(),
        ["/app/server", "--mode", "two words"]
    );
    assert_eq!(resolved.cmd(), ["--listen", "0.0.0.0:8080"]);
    assert_eq!(resolved.working_dir(), "/app");
    assert_eq!(resolved.env(), ["RUST_LOG=info", "EMPTY="]);
    assert_eq!(resolved.labels().len(), 2);
}

#[test]
fn multi_binary_image_requires_an_explicit_entrypoint() {
    let Some((_sysroot, plan)) = fixture_multi_plan() else {
        return;
    };

    assert_config_error(OciImageConfig::default(), &plan, "entrypoint");

    let config = OciImageConfig {
        entrypoint: vec!["/app/migrate".to_string()],
        ..OciImageConfig::default()
    };
    assert_eq!(
        config.resolve(&plan).unwrap().entrypoint(),
        ["/app/migrate"]
    );
}

#[test]
fn image_tag_uses_portable_tag_grammar() {
    let Some((_sysroot, plan)) = fixture_plan_installed_as("/app/server") else {
        return;
    };
    for tag in [
        "",
        ".leading",
        "contains/slash",
        "contains:colon",
        "space tag",
    ] {
        let config = OciImageConfig {
            tag: tag.to_string(),
            ..OciImageConfig::default()
        };
        assert_config_error(config, &plan, "image tag");
    }
    let config = OciImageConfig {
        tag: format!("a{}", "b".repeat(128)),
        ..OciImageConfig::default()
    };
    assert_config_error(config, &plan, "image tag");
}

#[test]
fn entrypoint_must_name_an_absolute_planned_file() {
    let Some((_sysroot, plan)) = fixture_plan_installed_as("/app/server") else {
        return;
    };
    for path in ["app/server", "/app/missing", "/app/../app/server"] {
        let config = OciImageConfig {
            entrypoint: vec![path.to_string()],
            ..OciImageConfig::default()
        };
        assert_config_error(config, &plan, "entrypoint");
    }
}

#[test]
fn working_directory_must_name_an_absolute_planned_directory() {
    let Some((_sysroot, plan)) = fixture_plan_installed_as("/app/server") else {
        return;
    };
    for path in ["app", "/missing", "/app/server", "/app/../app"] {
        let config = OciImageConfig {
            working_dir: Some(path.to_string()),
            ..OciImageConfig::default()
        };
        assert_config_error(config, &plan, "working directory");
    }
}

#[test]
fn environment_requires_unique_nonempty_keys() {
    let Some((_sysroot, plan)) = fixture_plan_installed_as("/app/server") else {
        return;
    };
    for env in [
        vec!["=value".to_string()],
        vec!["NO_SEPARATOR".to_string()],
        vec!["A=one".to_string(), "A=two".to_string()],
    ] {
        let config = OciImageConfig {
            env,
            ..OciImageConfig::default()
        };
        assert_config_error(config, &plan, "environment");
    }
}

#[test]
fn labels_require_nonempty_keys() {
    let Some((_sysroot, plan)) = fixture_plan_installed_as("/app/server") else {
        return;
    };
    let config = OciImageConfig {
        labels: BTreeMap::from([(String::new(), "value".to_string())]),
        ..OciImageConfig::default()
    };

    assert_config_error(config, &plan, "label");
}

#[test]
fn image_metadata_rejects_nul_bytes() {
    let Some((_sysroot, plan)) = fixture_plan_installed_as("/app/server") else {
        return;
    };
    let configs = [
        OciImageConfig {
            entrypoint: vec!["/app/server".to_string(), "bad\0argument".to_string()],
            ..OciImageConfig::default()
        },
        OciImageConfig {
            cmd: vec!["bad\0argument".to_string()],
            ..OciImageConfig::default()
        },
        OciImageConfig {
            env: vec!["KEY=bad\0value".to_string()],
            ..OciImageConfig::default()
        },
        OciImageConfig {
            labels: BTreeMap::from([("bad\0key".to_string(), "value".to_string())]),
            ..OciImageConfig::default()
        },
        OciImageConfig {
            labels: BTreeMap::from([("key".to_string(), "bad\0value".to_string())]),
            ..OciImageConfig::default()
        },
    ];

    for config in configs {
        assert_config_error(config, &plan, "NUL");
    }
}

#[test]
fn environment_and_label_entry_counts_are_bounded() {
    let Some((_sysroot, plan)) = fixture_plan_installed_as("/app/server") else {
        return;
    };
    let config = OciImageConfig {
        env: (0..=4096)
            .map(|index| format!("KEY{index}=value"))
            .collect(),
        ..OciImageConfig::default()
    };
    assert_config_error(config, &plan, "4,096");

    let config = OciImageConfig {
        labels: (0..=4096)
            .map(|index| (format!("key{index}"), "value".to_string()))
            .collect(),
        ..OciImageConfig::default()
    };
    assert_config_error(config, &plan, "4,096");
}

#[test]
fn individual_metadata_values_are_bounded() {
    let Some((_sysroot, plan)) = fixture_plan_installed_as("/app/server") else {
        return;
    };
    let config = OciImageConfig {
        labels: BTreeMap::from([("key".to_string(), "x".repeat((1 << 20) + 1))]),
        ..OciImageConfig::default()
    };

    assert_config_error(config, &plan, "1,048,576");
}

#[test]
fn layout_descriptor_graph_is_complete_and_hashed() {
    let Some((_sysroot, plan)) = fixture_plan_installed_as("/app/server") else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let layout = temp.path().join("image");
    let report = OciLayoutBuilder::new(&layout)
        .image(OciImageConfig {
            tag: "ci-test".to_string(),
            entrypoint: vec!["/app/server".to_string()],
            cmd: vec!["--version".to_string()],
            working_dir: Some("/app".to_string()),
            env: vec!["RUST_LOG=info".to_string()],
            labels: BTreeMap::from([("org.example.test".to_string(), "true".to_string())]),
        })
        .apply(&plan)
        .unwrap();

    assert_eq!(
        read_json(&layout.join("oci-layout"))["imageLayoutVersion"],
        "1.0.0"
    );
    assert_eq!(report.platform(), "linux/amd64");
    assert_eq!(report.image().tag(), "ci-test");
    assert_descriptor_graph_is_complete_and_hashed(&layout, &report);
}

#[test]
fn layout_layer_matches_rootfs_materialization() {
    let Some((_sysroot, plan)) = fixture_plan_installed_as("/app/server") else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let layout = temp.path().join("image");
    let rootfs = temp.path().join("rootfs");
    let extracted = temp.path().join("extracted");
    let report = OciLayoutBuilder::new(&layout).apply(&plan).unwrap();
    RootFsBuilder::new(&rootfs).apply(&plan).unwrap();
    std::fs::create_dir(&extracted).unwrap();
    let layer = layout.join("blobs/sha256").join(&report.layer_digest().0);
    tar::Archive::new(std::fs::File::open(layer).unwrap())
        .unpack(&extracted)
        .unwrap();

    assert_eq!(tree_snapshot(&extracted), tree_snapshot(&rootfs));
}

#[test]
fn image_layouts_are_byte_for_byte_deterministic() {
    let Some((_sysroot, plan)) = fixture_plan_installed_as("/app/server") else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let first = temp.path().join("first");
    let second = temp.path().join("second");
    OciLayoutBuilder::new(&first).apply(&plan).unwrap();
    OciLayoutBuilder::new(&second).apply(&plan).unwrap();

    assert_eq!(layout_snapshot(&first), layout_snapshot(&second));
}

#[test]
fn image_layer_honors_source_date_epoch() {
    if !isolated_source_date_epoch("image_layer_honors_source_date_epoch", "123456789") {
        return;
    }
    let Some((_sysroot, plan)) = fixture_plan_installed_as("/app/server") else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let layout = temp.path().join("image");
    let report = OciLayoutBuilder::new(&layout).apply(&plan).unwrap();
    let layer = layout.join("blobs/sha256").join(&report.layer_digest().0);
    let mut archive = tar::Archive::new(std::fs::File::open(layer).unwrap());
    for entry in archive.entries().unwrap() {
        assert_eq!(entry.unwrap().header().mtime().unwrap(), 123456789);
    }
}

#[test]
fn failed_layout_builds_do_not_publish_partial_output() {
    let Some((sysroot, plan)) = fixture_plan_installed_as("/app/server") else {
        return;
    };
    std::fs::write(sysroot.path("/bin/app-default"), b"changed after planning").unwrap();
    let temp = tempfile::tempdir().unwrap();
    let absent = temp.path().join("absent");
    let error = OciLayoutBuilder::new(&absent).apply(&plan).unwrap_err();
    assert_eq!(error.code(), "E1006");
    assert!(!absent.exists());

    let existing = temp.path().join("existing");
    std::fs::create_dir(&existing).unwrap();
    std::fs::write(existing.join("sentinel"), b"keep").unwrap();
    let before = layout_snapshot(&existing);
    // `--clean` gets past the occupancy check, so the failure under test is the
    // build itself rather than the refusal to replace a foreign directory.
    let error = OciLayoutBuilder::new(&existing)
        .clean(true)
        .apply(&plan)
        .unwrap_err();
    assert_eq!(error.code(), "E1006");
    assert_eq!(layout_snapshot(&existing), before);

    assert!(std::fs::read_dir(temp.path()).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".elfpak-oci-")
    }));
}

#[test]
fn layout_publication_replaces_directories_and_rejects_symlinks() {
    let Some((_sysroot, plan)) = fixture_plan_installed_as("/app/server") else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let layout = temp.path().join("image");
    std::fs::create_dir(&layout).unwrap();
    std::fs::write(layout.join("sentinel"), b"old").unwrap();

    // Publication replaces the whole directory, so a destination holding
    // something that is not a layout is refused rather than deleted.
    let error = OciLayoutBuilder::new(&layout).apply(&plan).unwrap_err();
    assert_eq!(error.code(), "E4001");
    assert_eq!(std::fs::read(layout.join("sentinel")).unwrap(), b"old");

    // A foreign directory must not become replaceable merely by naming one
    // of its files like the OCI marker.
    let fake_layout = temp.path().join("fake-image");
    std::fs::create_dir(&fake_layout).unwrap();
    std::fs::write(fake_layout.join("oci-layout"), b"not an OCI marker").unwrap();
    std::fs::write(fake_layout.join("sentinel"), b"keep").unwrap();
    let error = OciLayoutBuilder::new(&fake_layout)
        .apply(&plan)
        .unwrap_err();
    assert_eq!(error.code(), "E4001");
    assert_eq!(
        std::fs::read(fake_layout.join("oci-layout")).unwrap(),
        b"not an OCI marker"
    );
    assert_eq!(
        std::fs::read(fake_layout.join("sentinel")).unwrap(),
        b"keep"
    );

    std::fs::write(
        fake_layout.join("oci-layout"),
        br#"{"imageLayoutVersion":"1.0.0"}"#,
    )
    .unwrap();
    let error = OciLayoutBuilder::new(&fake_layout)
        .apply(&plan)
        .unwrap_err();
    assert_eq!(error.code(), "E4001");
    assert_eq!(
        std::fs::read(fake_layout.join("sentinel")).unwrap(),
        b"keep"
    );

    OciLayoutBuilder::new(&layout)
        .clean(true)
        .apply(&plan)
        .unwrap();
    assert!(!layout.join("sentinel").exists());
    assert!(layout.join("index.json").is_file());

    // Rebuilding over a layout this tool wrote needs no permission.
    OciLayoutBuilder::new(&layout).apply(&plan).unwrap();
    assert!(layout.join("index.json").is_file());

    let target = temp.path().join("target");
    let symlink = temp.path().join("symlink");
    std::fs::create_dir(&target).unwrap();
    std::fs::write(target.join("sentinel"), b"keep").unwrap();
    std::os::unix::fs::symlink(&target, &symlink).unwrap();
    let error = OciLayoutBuilder::new(&symlink).apply(&plan).unwrap_err();
    assert_eq!(error.code(), "E4001");
    assert_eq!(std::fs::read(target.join("sentinel")).unwrap(), b"keep");
}

#[test]
fn oci_archive_extracts_to_the_same_layout() {
    let Some((_sysroot, plan)) = fixture_plan_installed_as("/app/server") else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let layout = temp.path().join("image");
    let archive = temp.path().join("image.tar");
    let unpacked = temp.path().join("unpacked");
    let image = OciImageConfig {
        tag: "ci-test".to_string(),
        env: vec!["RUST_LOG=info".to_string()],
        ..OciImageConfig::default()
    };

    let directory_report = OciLayoutBuilder::new(&layout)
        .image(image.clone())
        .apply(&plan)
        .unwrap();
    let archive_report = OciArchiveBuilder::new(&archive)
        .image(image)
        .apply(&plan)
        .unwrap();
    std::fs::create_dir(&unpacked).unwrap();
    tar::Archive::new(std::fs::File::open(&archive).unwrap())
        .unpack(&unpacked)
        .unwrap();

    assert_eq!(layout_snapshot(&layout), layout_snapshot(&unpacked));
    assert_eq!(
        directory_report.layer_digest(),
        archive_report.layer_digest()
    );
    assert_eq!(
        directory_report.config_digest(),
        archive_report.config_digest()
    );
    assert_eq!(
        directory_report.manifest_digest(),
        archive_report.manifest_digest()
    );
}

#[test]
fn oci_archives_are_byte_for_byte_deterministic() {
    let Some((_sysroot, plan)) = fixture_plan_installed_as("/app/server") else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let first = temp.path().join("first.tar");
    let second = temp.path().join("second.tar");
    OciArchiveBuilder::new(&first).apply(&plan).unwrap();
    OciArchiveBuilder::new(&second).apply(&plan).unwrap();

    assert_eq!(
        std::fs::read(first).unwrap(),
        std::fs::read(second).unwrap()
    );
}

#[test]
fn oci_archive_metadata_and_order_are_pinned() {
    if !isolated_source_date_epoch("oci_archive_metadata_and_order_are_pinned", "123456789") {
        return;
    }
    let Some((_sysroot, plan)) = fixture_plan_installed_as("/app/server") else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let archive = temp.path().join("image.tar");
    OciArchiveBuilder::new(&archive).apply(&plan).unwrap();

    let mut tar = tar::Archive::new(std::fs::File::open(archive).unwrap());
    let mut names = Vec::new();
    for entry in tar.entries().unwrap() {
        let entry = entry.unwrap();
        let header = entry.header();
        let name = String::from_utf8(header.path_bytes().into_owned()).unwrap();
        assert!(!name.starts_with('/'));
        assert_eq!(header.uid().unwrap(), 0);
        assert_eq!(header.gid().unwrap(), 0);
        assert_eq!(header.mtime().unwrap(), 123456789);
        let directory = header.entry_type().is_dir();
        assert_eq!(
            header.mode().unwrap(),
            if directory { 0o755 } else { 0o644 }
        );
        names.push(name);
    }

    assert_eq!(
        &names[..4],
        ["oci-layout", "index.json", "blobs/", "blobs/sha256/"]
    );
    assert_eq!(names[4..], {
        let mut blobs = names[4..].to_vec();
        blobs.sort();
        blobs
    });
}

#[test]
fn invalid_source_date_epoch_leaves_an_oci_archive_untouched() {
    if !isolated_source_date_epoch(
        "invalid_source_date_epoch_leaves_an_oci_archive_untouched",
        "invalid",
    ) {
        return;
    }
    let Some((_sysroot, plan)) = fixture_plan_installed_as("/app/server") else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let archive = temp.path().join("image.tar");
    std::fs::write(&archive, b"previous archive").unwrap();

    let error = OciArchiveBuilder::new(&archive).apply(&plan).unwrap_err();
    assert_eq!(error.code(), "E4001");
    assert_eq!(std::fs::read(archive).unwrap(), b"previous archive");
}
