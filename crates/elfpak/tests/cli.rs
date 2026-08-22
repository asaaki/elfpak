//! End-to-end CLI tests against the host filesystem.

use clap::Parser;
use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn elfpak(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_elfpak"))
        .args(args)
        .output()
        .expect("elfpak runs")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A dynamically linked host binary to package.
fn subject() -> Option<PathBuf> {
    ["/usr/bin/ls", "/bin/ls", "/usr/bin/env"]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
}

fn multiple_subjects() -> Option<[PathBuf; 2]> {
    let mut binaries = ["/usr/bin/ls", "/usr/bin/env", "/bin/cat"]
        .into_iter()
        .map(PathBuf::from)
        .filter(|path| path.is_file());
    let first = binaries.next()?;
    let second = binaries.find(|path| path.file_name() != first.file_name())?;
    Some([first, second])
}

#[test]
fn multiple_binaries_are_installed_under_one_directory_and_recorded() {
    let Some([first, second]) = multiple_subjects() else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let rootfs = tmp.path().join("rootfs");
    let archive = tmp.path().join("rootfs.tar");

    let output = elfpak(&[
        "bundle",
        first.to_str().unwrap(),
        second.to_str().unwrap(),
        "--output",
        rootfs.to_str().unwrap(),
        "--tar",
        archive.to_str().unwrap(),
        "--install-dir",
        "/app",
        "--no-config",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));

    let first_install = format!("/app/{}", first.file_name().unwrap().to_string_lossy());
    let second_install = format!("/app/{}", second.file_name().unwrap().to_string_lossy());
    assert!(rootfs.join(first_install.trim_start_matches('/')).is_file());
    assert!(
        rootfs
            .join(second_install.trim_start_matches('/'))
            .is_file()
    );
    assert!(archive.is_file());
    let archive_listing = Command::new("tar")
        .args(["-tf", archive.to_str().unwrap()])
        .output();
    if let Ok(listing) = archive_listing {
        assert!(listing.status.success(), "{}", stderr(&listing));
        let listing = stdout(&listing);
        assert!(
            listing.contains(first_install.trim_start_matches('/')),
            "{listing}"
        );
        assert!(
            listing.contains(second_install.trim_start_matches('/')),
            "{listing}"
        );
    }

    let manifest_path = tmp.path().join("elfpak-manifest.json");
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    assert_eq!(manifest["binary"], first_install);
    assert_eq!(
        manifest["binaries"],
        serde_json::json!([first_install, second_install])
    );

    let verify = elfpak(&["verify", manifest_path.to_str().unwrap()]);
    assert!(verify.status.success(), "{}", stderr(&verify));
}

#[test]
fn multiple_binaries_reject_a_singular_install_path() {
    let Some([first, second]) = multiple_subjects() else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let rootfs = tmp.path().join("rootfs");

    let output = elfpak(&[
        "bundle",
        first.to_str().unwrap(),
        second.to_str().unwrap(),
        "--output",
        rootfs.to_str().unwrap(),
        "--install",
        "/app/server",
        "--no-config",
    ]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("--install-dir"),
        "{}",
        stderr(&output)
    );
    assert!(!rootfs.exists());
}

#[test]
fn multiple_binaries_reject_duplicate_preserved_names_before_writing() {
    let Some([first, second]) = multiple_subjects() else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let first_copy = tmp.path().join("one/tool");
    let second_copy = tmp.path().join("two/tool");
    std::fs::create_dir_all(first_copy.parent().unwrap()).unwrap();
    std::fs::create_dir_all(second_copy.parent().unwrap()).unwrap();
    std::fs::copy(first, &first_copy).unwrap();
    std::fs::copy(second, &second_copy).unwrap();
    let rootfs = tmp.path().join("rootfs");

    let output = elfpak(&[
        "bundle",
        first_copy.to_str().unwrap(),
        second_copy.to_str().unwrap(),
        "--output",
        rootfs.to_str().unwrap(),
        "--install-dir",
        "/app",
        "--no-config",
    ]);

    assert!(!output.status.success());
    let error = stderr(&output);
    assert!(error.contains("/app/tool"), "{error}");
    assert!(!rootfs.exists());
}

#[test]
fn inspect_reports_architecture_interpreter_and_dependencies() {
    let Some(binary) = subject() else { return };
    let output = elfpak(&["inspect", binary.to_str().unwrap()]);
    assert!(output.status.success(), "{}", stderr(&output));

    let text = stdout(&output);
    assert!(text.contains("ELF64"), "{text}");
    assert!(text.contains("interpreter:"), "{text}");
    assert!(text.contains("ld-linux"), "{text}");
    assert!(text.contains("libc.so.6"), "{text}");
    assert!(text.contains("shared objects"), "{text}");
}

#[test]
fn inspect_json_is_machine_readable() {
    let Some(binary) = subject() else { return };
    let output = elfpak(&["inspect", "--json", binary.to_str().unwrap()]);
    assert!(output.status.success(), "{}", stderr(&output));

    let value: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("valid json");
    assert!(value["files"].as_array().unwrap().len() > 1);
    assert_eq!(value["architecture"], "x86_64");
}

#[test]
fn bundle_dry_run_writes_nothing() {
    let Some(binary) = subject() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let rootfs = tmp.path().join("rootfs");

    let output = elfpak(&[
        "bundle",
        binary.to_str().unwrap(),
        "-o",
        rootfs.to_str().unwrap(),
        "--install",
        "/app/server",
        "--dry-run",
        "--no-config",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!rootfs.exists());
    assert!(stdout(&output).contains("dry run"));

    // The same holds for the archive backend.
    let archive = tmp.path().join("rootfs.tar");
    let output = elfpak(&[
        "bundle",
        binary.to_str().unwrap(),
        "--tar",
        archive.to_str().unwrap(),
        "--install",
        "/app/server",
        "--dry-run",
        "--no-config",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!archive.exists(), "a dry run writes no archive");
    assert!(!tmp.path().join("elfpak-manifest.json").exists());
}

#[test]
fn oci_outputs_and_image_options_are_accepted_in_dry_runs() {
    let Some(binary) = subject() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let layout = tmp.path().join("image");
    let archive = tmp.path().join("image.tar");

    let layout_output = elfpak(&[
        "bundle",
        binary.to_str().unwrap(),
        "--oci-layout",
        layout.to_str().unwrap(),
        "--install",
        "/app/server",
        "--image-tag",
        "ci-test",
        "--entrypoint",
        "/app/server",
        "--entrypoint",
        "--verbose",
        "--cmd",
        "--version",
        "--working-dir",
        "/app",
        "--env",
        "RUST_LOG=info",
        "--label",
        "org.example.test=true",
        "--dry-run",
        "--no-config",
    ]);
    assert!(layout_output.status.success(), "{}", stderr(&layout_output));
    assert!(stdout(&layout_output).contains("oci layout:"));
    assert!(!layout.exists());

    let archive_output = elfpak(&[
        "bundle",
        binary.to_str().unwrap(),
        "--oci-archive",
        archive.to_str().unwrap(),
        "--install",
        "/app/server",
        "--dry-run",
        "--no-config",
    ]);
    assert!(
        archive_output.status.success(),
        "{}",
        stderr(&archive_output)
    );
    assert!(stdout(&archive_output).contains("oci archive:"));
    assert!(!archive.exists());
}

#[test]
fn invalid_oci_metadata_fails_before_writing() {
    let Some(binary) = subject() else { return };
    let Some([first, second]) = multiple_subjects() else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();

    let cases: &[(&str, &[&str])] = &[
        ("image tag", &["--image-tag", "bad/tag"]),
        ("environment", &["--env", "MISSING_SEPARATOR"]),
        ("label", &["--label", "MISSING_SEPARATOR"]),
        ("working directory", &["--working-dir", "/missing"]),
    ];
    for (expected, options) in cases {
        let layout = tmp.path().join(expected.replace(' ', "-"));
        let mut arguments = vec![
            "bundle",
            binary.to_str().unwrap(),
            "--oci-layout",
            layout.to_str().unwrap(),
            "--install",
            "/app/server",
            "--dry-run",
            "--no-config",
        ];
        arguments.extend_from_slice(options);
        let output = elfpak(&arguments);
        assert!(
            !output.status.success(),
            "{expected} unexpectedly succeeded"
        );
        assert!(stderr(&output).contains(expected), "{}", stderr(&output));
        assert!(!layout.exists());
    }

    let multi = tmp.path().join("multi");
    let output = elfpak(&[
        "bundle",
        first.to_str().unwrap(),
        second.to_str().unwrap(),
        "--oci-layout",
        multi.to_str().unwrap(),
        "--install-dir",
        "/app",
        "--dry-run",
        "--no-config",
    ]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("entrypoint"),
        "{}",
        stderr(&output)
    );
    assert!(!multi.exists());
}

#[test]
fn rootfs_only_does_not_validate_unused_image_defaults() {
    let Some(binary) = subject() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("elfpak.toml");
    let rootfs = tmp.path().join("rootfs");
    std::fs::write(
        &config,
        format!(
            "[package]\nbinary = '{}'\noutput = '{}'\n\n[image]\ntag = 'bad/tag'\nworking_dir = '/missing'\n",
            binary.display(),
            rootfs.display()
        ),
    )
    .unwrap();

    let output = elfpak(&["bundle", "--config", config.to_str().unwrap(), "--dry-run"]);
    assert!(output.status.success(), "{}", stderr(&output));
}

#[test]
fn bundle_can_write_every_output_from_one_plan() {
    let Some(binary) = subject() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let rootfs = tmp.path().join("rootfs");
    let rootfs_tar = tmp.path().join("rootfs.tar");
    let layout = tmp.path().join("image");
    let archive = tmp.path().join("image.tar");

    let output = elfpak(&[
        "bundle",
        binary.to_str().unwrap(),
        "--output",
        rootfs.to_str().unwrap(),
        "--tar",
        rootfs_tar.to_str().unwrap(),
        "--oci-layout",
        layout.to_str().unwrap(),
        "--oci-archive",
        archive.to_str().unwrap(),
        "--install",
        "/app/server",
        "--image-tag",
        "ci-test",
        "--entrypoint",
        "/app/server",
        "--cmd",
        "--version",
        "--working-dir",
        "/app",
        "--env",
        "RUST_LOG=info",
        "--label",
        "org.example.test=true",
        "--no-config",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(rootfs.is_dir());
    assert!(rootfs_tar.is_file());
    assert!(layout.is_dir());
    assert!(archive.is_file());
    let text = stdout(&output);
    for destination in ["rootfs:", "tar:", "oci layout:", "oci archive:"] {
        assert!(text.contains(destination), "{text}");
    }

    let layout_index: serde_json::Value =
        serde_json::from_slice(&std::fs::read(layout.join("index.json")).unwrap()).unwrap();
    let unpacked = tmp.path().join("unpacked-image");
    std::fs::create_dir(&unpacked).unwrap();
    let extraction = Command::new("tar")
        .args([
            "-xf",
            archive.to_str().unwrap(),
            "-C",
            unpacked.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(extraction.status.success(), "{}", stderr(&extraction));
    let archive_index: serde_json::Value =
        serde_json::from_slice(&std::fs::read(unpacked.join("index.json")).unwrap()).unwrap();
    assert_eq!(
        layout_index["manifests"][0]["digest"],
        archive_index["manifests"][0]["digest"]
    );

    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(tmp.path().join("elfpak-manifest.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["manifest_version"], 4);
    assert_eq!(manifest["oci_layout"], layout.display().to_string());
    assert_eq!(manifest["oci_archive"], archive.display().to_string());
    assert_eq!(manifest["image"]["tag"], "ci-test");
    assert_eq!(manifest["image"]["os"], "linux");
    assert_eq!(manifest["image"]["architecture"], "amd64");
    assert_eq!(
        manifest["image"]["entrypoint"],
        serde_json::json!(["/app/server"])
    );
    assert_eq!(manifest["image"]["cmd"], serde_json::json!(["--version"]));
    assert_eq!(manifest["image"]["working_dir"], "/app");
    assert_eq!(
        manifest["image"]["env"],
        serde_json::json!(["RUST_LOG=info"])
    );
    assert_eq!(manifest["image"]["labels"]["org.example.test"], "true");
    assert_eq!(
        manifest["image"]["manifest_digest"],
        layout_index["manifests"][0]["digest"]
    );
}

#[test]
fn output_artifacts_must_not_overlap() {
    let Some(binary) = subject() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let cases = [
        (
            vec!["--output", "tree", "--oci-layout", "tree/child/.."],
            ["--output", "--oci-layout"],
        ),
        (
            vec!["--output", "tree", "--tar", "tree/rootfs.tar"],
            ["--output", "--tar"],
        ),
        (
            vec!["--output", "tree", "--oci-archive", "tree/image.tar"],
            ["--output", "--oci-archive"],
        ),
        (
            vec!["--output", "tree", "--manifest", "tree/manifest.json"],
            ["--output", "--manifest"],
        ),
        (
            vec!["--oci-layout", "tree", "--tar", "tree/rootfs.tar"],
            ["--oci-layout", "--tar"],
        ),
        (
            vec!["--oci-layout", "tree", "--oci-archive", "tree/image.tar"],
            ["--oci-layout", "--oci-archive"],
        ),
        (
            vec!["--oci-layout", "tree", "--manifest", "tree/manifest.json"],
            ["--oci-layout", "--manifest"],
        ),
        (
            vec!["--tar", "same", "--oci-archive", "same"],
            ["--tar", "--oci-archive"],
        ),
        (
            vec!["--tar", "same", "--manifest", "same"],
            ["--tar", "--manifest"],
        ),
        (
            vec!["--oci-archive", "same", "--manifest", "same"],
            ["--oci-archive", "--manifest"],
        ),
    ];

    for (case_index, (options, expected)) in cases.into_iter().enumerate() {
        let case = tmp.path().join(format!("case-{case_index}"));
        std::fs::create_dir(&case).unwrap();
        let mut arguments = vec![
            "bundle".to_string(),
            binary.display().to_string(),
            "--install".to_string(),
            "/app/server".to_string(),
            "--dry-run".to_string(),
            "--no-config".to_string(),
        ];
        let has_manifest = options.contains(&"--manifest");
        for [flag, path] in options.as_chunks::<2>().0 {
            arguments.push((*flag).to_string());
            arguments.push(case.join(path).display().to_string());
        }
        if !has_manifest {
            arguments.push("--no-manifest".to_string());
        }
        let refs: Vec<&str> = arguments.iter().map(String::as_str).collect();
        let output = elfpak(&refs);
        assert!(
            !output.status.success(),
            "case {case_index} unexpectedly succeeded"
        );
        let error = stderr(&output);
        assert!(error.contains(expected[0]), "case {case_index}: {error}");
        assert!(error.contains(expected[1]), "case {case_index}: {error}");
    }
}

#[test]
fn bundle_then_verify_round_trips() {
    let Some(binary) = subject() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let rootfs = tmp.path().join("rootfs");

    let output = elfpak(&[
        "bundle",
        binary.to_str().unwrap(),
        "-o",
        rootfs.to_str().unwrap(),
        "--install",
        "/app/server",
        "--preset",
        "web",
        "--user",
        "65532:65532",
        "--no-config",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));

    assert!(rootfs.join("app/server").is_file());
    assert!(rootfs.join("etc/passwd").is_file());
    assert!(rootfs.join("etc/group").is_file());
    assert!(rootfs.join("etc/nsswitch.conf").is_file());
    assert!(rootfs.join("etc/ssl/certs/ca-certificates.crt").is_file());
    assert!(rootfs.join("tmp").is_dir());

    let passwd = std::fs::read_to_string(rootfs.join("etc/passwd")).unwrap();
    assert!(passwd.contains("65532"), "{passwd}");

    let manifest = tmp.path().join("elfpak-manifest.json");
    assert!(manifest.is_file(), "manifest is written beside the rootfs");

    let verify = elfpak(&["verify", manifest.to_str().unwrap()]);
    assert!(verify.status.success(), "{}", stderr(&verify));
    assert!(stdout(&verify).contains("ok:"));
}

#[test]
fn dependency_policy_failures_are_reported_with_a_code() {
    let Some(binary) = subject() else { return };
    let tmp = tempfile::tempdir().unwrap();

    let output = elfpak(&[
        "bundle",
        binary.to_str().unwrap(),
        "-o",
        tmp.path().join("rootfs").to_str().unwrap(),
        "--allow-library",
        "libnothing.so.0",
        "--no-config",
    ]);
    assert!(!output.status.success());
    let text = stderr(&output);
    assert!(text.contains("error[E2002]"), "{text}");
    assert!(text.contains("--allow-library"), "{text}");
}

/// A warning and an error must never share a code, or matching on one is
/// meaningless. `E1006` was once both `SourceChanged` and this warning.
#[test]
fn warnings_are_reported_with_a_code_no_error_uses() {
    let Some(binary) = subject() else { return };
    let tmp = tempfile::tempdir().unwrap();

    let output = elfpak(&[
        "bundle",
        binary.to_str().unwrap(),
        "-o",
        tmp.path().join("rootfs").to_str().unwrap(),
        "--user",
        "65532:65532",
        "--passwd-group=false",
        "--no-config",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));

    let text = stdout(&output);
    assert!(text.contains("warning[E4003]"), "{text}");
    assert!(
        text.contains("--user was given without passwd/group"),
        "{text}"
    );
    assert!(
        !text.contains("E1006"),
        "E1006 belongs to the source-changed error: {text}"
    );
}

#[test]
fn config_file_supplies_defaults_and_cli_overrides_them() {
    let Some(binary) = subject() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("elfpak.toml");
    let configured = tmp.path().join("configured");
    std::fs::write(
        &config,
        format!(
            "[package]\n\
             binary = \"{}\"\n\
             install = \"/srv/app\"\n\
             output = \"{}\"\n\
             \n\
             [runtime]\n\
             preset = \"minimal\"\n",
            binary.display(),
            configured.display()
        ),
    )
    .unwrap();

    let output = elfpak(&["bundle", "--config", config.to_str().unwrap()]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(configured.join("srv/app").is_file());

    // An explicit --install wins over the configured one.
    let overridden = tmp.path().join("overridden");
    let output = elfpak(&[
        "bundle",
        "--config",
        config.to_str().unwrap(),
        "-o",
        overridden.to_str().unwrap(),
        "--install",
        "/opt/app",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(overridden.join("opt/app").is_file());
}

#[test]
fn config_file_can_supply_multiple_binaries_and_an_install_directory() {
    let Some([first, second]) = multiple_subjects() else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("elfpak.toml");
    let rootfs = tmp.path().join("rootfs");
    std::fs::write(
        &config,
        format!(
            "[package]\nbinaries = ['{}', '{}']\ninstall_dir = '/app'\noutput = '{}'\n",
            first.display(),
            second.display(),
            rootfs.display()
        ),
    )
    .unwrap();

    let output = elfpak(&["bundle", "--config", config.to_str().unwrap()]);

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        rootfs
            .join("app")
            .join(first.file_name().unwrap())
            .is_file()
    );
    assert!(
        rootfs
            .join("app")
            .join(second.file_name().unwrap())
            .is_file()
    );
}

#[test]
fn missing_binary_is_a_clear_error() {
    let output = elfpak(&["inspect", "/nonexistent/binary"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("error["), "{}", stderr(&output));
}

#[test]
fn a_non_elf_input_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let script = tmp.path().join("script.sh");
    std::fs::write(&script, "#!/bin/sh\necho hi\n").unwrap();

    let output = elfpak(&["inspect", script.to_str().unwrap()]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("E1002"), "{}", stderr(&output));
}

#[test]
fn verify_detects_a_modified_rootfs() {
    let Some(binary) = subject() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let rootfs = tmp.path().join("rootfs");
    let output = elfpak(&[
        "bundle",
        binary.to_str().unwrap(),
        "-o",
        rootfs.to_str().unwrap(),
        "--install",
        "/app/server",
        "--no-config",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));

    std::fs::write(rootfs.join("app/server"), b"tampered").unwrap();
    let verify = elfpak(&[
        "verify",
        tmp.path().join("elfpak-manifest.json").to_str().unwrap(),
    ]);
    assert!(!verify.status.success());
    assert!(stderr(&verify).contains("E5001"), "{}", stderr(&verify));
}

#[test]
fn include_copies_extra_paths_preserving_their_location() {
    let Some(binary) = subject() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let rootfs = tmp.path().join("rootfs");

    // /etc/os-release exists on every mainstream distribution.
    let extra = Path::new("/etc/os-release");
    if !extra.is_file() {
        return;
    }
    let output = elfpak(&[
        "bundle",
        binary.to_str().unwrap(),
        "-o",
        rootfs.to_str().unwrap(),
        "--install",
        "/app/server",
        "--include",
        extra.to_str().unwrap(),
        "--no-config",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        rootfs.join("etc/os-release").exists() || rootfs.join("usr/lib/os-release").exists(),
        "the include keeps its original path (or the symlink target's)"
    );
}

#[test]
fn tar_output_can_replace_the_directory() {
    let Some(binary) = subject() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let archive = tmp.path().join("rootfs.tar");

    let output = elfpak(&[
        "bundle",
        binary.to_str().unwrap(),
        "--tar",
        archive.to_str().unwrap(),
        "--install",
        "/app/server",
        "--no-config",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(archive.is_file());
    assert!(stdout(&output).contains("tar:"), "{}", stdout(&output));

    // The manifest lands beside the archive when no directory was requested.
    let manifest = tmp.path().join("elfpak-manifest.json");
    assert!(manifest.is_file());
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
    assert_eq!(value["tar"], archive.display().to_string());
    assert!(value["rootfs"].is_null(), "no directory was written");

    // Unpacking the archive yields the executable at its install path.
    let unpacked = tmp.path().join("unpacked");
    std::fs::create_dir_all(&unpacked).unwrap();
    let status = Command::new("tar")
        .args([
            "-xf",
            archive.to_str().unwrap(),
            "-C",
            unpacked.to_str().unwrap(),
        ])
        .status();
    if matches!(status, Ok(status) if status.success()) {
        assert!(unpacked.join("app/server").is_file());
    }
}

#[test]
fn bundle_can_write_a_directory_and_an_archive_at_once() {
    let Some(binary) = subject() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let rootfs = tmp.path().join("rootfs");
    let archive = tmp.path().join("rootfs.tar");

    let output = elfpak(&[
        "bundle",
        binary.to_str().unwrap(),
        "-o",
        rootfs.to_str().unwrap(),
        "--tar",
        archive.to_str().unwrap(),
        "--install",
        "/app/server",
        "--no-config",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(rootfs.join("app/server").is_file());
    assert!(archive.is_file());
}

#[test]
fn bundle_requires_some_output() {
    let Some(binary) = subject() else { return };
    let output = elfpak(&["bundle", binary.to_str().unwrap(), "--no-config"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("--tar"), "{}", stderr(&output));
}

#[test]
fn strict_verify_rejects_an_unlisted_file() {
    let Some(binary) = subject() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let rootfs = tmp.path().join("rootfs");
    let manifest = tmp.path().join("elfpak-manifest.json");

    let output = elfpak(&[
        "bundle",
        binary.to_str().unwrap(),
        "-o",
        rootfs.to_str().unwrap(),
        "--install",
        "/app/server",
        "--no-config",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));

    std::fs::write(rootfs.join("unexpected"), b"smuggled").unwrap();
    let lenient = elfpak(&["verify", manifest.to_str().unwrap()]);
    assert!(
        lenient.status.success(),
        "the default mode ignores extra files"
    );

    let strict = elfpak(&["verify", manifest.to_str().unwrap(), "--strict"]);
    assert!(!strict.status.success());
    assert!(
        stderr(&strict).contains("not listed in the manifest"),
        "{}",
        stderr(&strict)
    );
}

#[test]
fn reusable_bundle_entry_point_forces_the_supplied_binary() {
    #[derive(Parser)]
    struct BundleCli {
        #[command(flatten)]
        bundle: elfpak::BundleArgs,
    }

    let Some(binary) = subject() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("elfpak.toml");
    let rootfs = tmp.path().join("rootfs");
    std::fs::write(&config, "[package]\nbinary = '/does/not/exist'\n").unwrap();

    let args = BundleCli::parse_from([
        "bundle",
        "--config",
        config.to_str().unwrap(),
        "--output",
        rootfs.to_str().unwrap(),
        "--dry-run",
    ])
    .bundle;
    let status = elfpak::run_bundle(args, binary, false, 0);

    assert_eq!(status, std::process::ExitCode::SUCCESS);
    assert!(!rootfs.exists(), "dry-run must not materialize the rootfs");
}
