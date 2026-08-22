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
