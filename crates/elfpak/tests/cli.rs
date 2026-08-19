//! End-to-end CLI tests against the host filesystem.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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

#[test]
fn config_file_supplies_defaults_and_cli_overrides_them() {
    let Some(binary) = subject() else { return };
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("elfpak.toml");
    let configured = tmp.path().join("configured");
    std::fs::write(
        &config,
        format!(
            "[package]\nbinary = \"{}\"\ninstall = \"/srv/app\"\noutput = \"{}\"\n\n[runtime]\npreset = \"minimal\"\n",
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
