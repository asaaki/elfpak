//! End-to-end tests for the Cargo adapter against temporary real projects.

use std::{
    path::Path,
    process::{Command, Output},
    time::{Duration, Instant},
};

fn cargo_elfpak(project: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cargo-elfpak"))
        .current_dir(project)
        .args(args)
        .output()
        .expect("cargo-elfpak runs")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn write_binary_project(project: &Path, name: &str, body: &str) {
    std::fs::create_dir_all(project.join("src")).unwrap();
    std::fs::write(
        project.join("Cargo.toml"),
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n"),
    )
    .unwrap();
    std::fs::write(project.join("src/main.rs"), body).unwrap();
}

fn dry_run_args(rootfs: &Path) -> [&str; 5] {
    [
        "bundle",
        "--output",
        rootfs.to_str().unwrap(),
        "--dry-run",
        "--no-config",
    ]
}

fn host_target() -> String {
    let output = Command::new("rustc").arg("-vV").output().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    stdout(&output)
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .expect("rustc reports its host target")
        .to_owned()
}

fn write_multi_binary_project(project: &Path, package: &str, binaries: &[&str]) {
    std::fs::create_dir_all(project.join("src/bin")).unwrap();
    std::fs::write(
        project.join("Cargo.toml"),
        format!("[package]\nname = \"{package}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n"),
    )
    .unwrap();
    for binary in binaries {
        std::fs::write(
            project.join("src/bin").join(format!("{binary}.rs")),
            format!("fn main() {{ println!(\"{binary}\"); }}\n"),
        )
        .unwrap();
    }
}

#[test]
fn multiple_binaries_all_selects_every_workspace_binary() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    let rootfs = tmp.path().join("rootfs");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(
        workspace.join("Cargo.toml"),
        "[workspace]\nresolver = \"3\"\nmembers = [\"api\", \"worker\"]\n",
    )
    .unwrap();
    write_binary_project(&workspace.join("api"), "api", "fn main() {}\n");
    write_binary_project(&workspace.join("worker"), "worker", "fn main() {}\n");

    let output = cargo_elfpak(
        &workspace,
        &[
            "bundle",
            "--all",
            "--install-dir",
            "/app",
            "--output",
            rootfs.to_str().unwrap(),
            "--dry-run",
            "--no-config",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("/debug/api"), "{text}");
    assert!(text.contains("/debug/worker"), "{text}");
    assert!(text.contains("-> /app/api"), "{text}");
    assert!(text.contains("-> /app/worker"), "{text}");
}

#[test]
fn multiple_binaries_all_bins_selects_one_packages_binaries() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("tools");
    let rootfs = tmp.path().join("rootfs");
    write_multi_binary_project(&project, "tools", &["first", "second"]);

    let output = cargo_elfpak(
        &project,
        &[
            "bundle",
            "-p",
            "tools",
            "--all-bins",
            "--install-dir",
            "/app",
            "--output",
            rootfs.to_str().unwrap(),
            "--dry-run",
            "--no-config",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("-> /app/first"), "{text}");
    assert!(text.contains("-> /app/second"), "{text}");
}

#[test]
fn multiple_binaries_bins_materializes_only_the_named_subset() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("tools");
    let rootfs = tmp.path().join("rootfs");
    write_multi_binary_project(&project, "tools", &["first", "second", "third"]);

    let output = cargo_elfpak(
        &project,
        &[
            "bundle",
            "-p",
            "tools",
            "--bins",
            "first,third",
            "--install-dir",
            "/app",
            "--output",
            rootfs.to_str().unwrap(),
            "--no-config",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(rootfs.join("app/first").is_file());
    assert!(!rootfs.join("app/second").exists());
    assert!(rootfs.join("app/third").is_file());
}

#[test]
fn multiple_binaries_selector_modes_conflict_before_building() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    write_binary_project(&project, "conflict-fixture", "this is not Rust\n");

    let output = cargo_elfpak(&project, &["bundle", "--all", "--bin", "conflict-fixture"]);
    assert!(!output.status.success());
    let error = stderr(&output);
    assert!(error.contains("cannot be used with"), "{error}");
    assert!(
        !error.contains("src/main.rs"),
        "Cargo must not run: {error}"
    );
}

#[test]
fn builds_missing_and_stale_binaries_and_reuses_fresh_ones() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    let rootfs = tmp.path().join("rootfs");
    write_binary_project(&project, "freshness-fixture", "fn main() {}\n");

    let args = dry_run_args(&rootfs);

    let first = cargo_elfpak(&project, &args);
    assert!(first.status.success(), "{}", stderr(&first));
    assert!(
        stdout(&first).contains("built Cargo binary:"),
        "{}",
        stdout(&first)
    );
    assert!(stdout(&first).contains("rootfs:"), "{}", stdout(&first));
    assert!(!rootfs.exists(), "dry-run must not materialize output");
    let executable_modified = std::fs::metadata(
        project
            .join("target/debug")
            .join(format!("freshness-fixture{}", std::env::consts::EXE_SUFFIX)),
    )
    .unwrap()
    .modified()
    .unwrap();

    let second = cargo_elfpak(&project, &args);
    assert!(second.status.success(), "{}", stderr(&second));
    assert!(
        stdout(&second).contains("fresh Cargo binary:"),
        "{}",
        stdout(&second)
    );

    // Cargo's freshness model includes mtimes. Wait until this source edit is
    // distinguishable on the current filesystem instead of assuming its timestamp precision.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        std::fs::write(
            project.join("src/main.rs"),
            "fn main() { println!(\"changed\"); }\n",
        )
        .unwrap();
        let source_modified = std::fs::metadata(project.join("src/main.rs"))
            .unwrap()
            .modified()
            .unwrap();
        if source_modified > executable_modified {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "source {source_modified:?} must become newer than executable {executable_modified:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    let third = cargo_elfpak(&project, &args);
    assert!(third.status.success(), "{}", stderr(&third));
    assert!(
        stdout(&third).contains("built Cargo binary:"),
        "stdout:\n{}\nstderr:\n{}",
        stdout(&third),
        stderr(&third)
    );
}

#[test]
fn oci_archive_options_build_the_cargo_binary_before_dry_run() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    let archive = tmp.path().join("image.tar");
    write_binary_project(&project, "oci-fixture", "fn main() {}\n");

    let output = cargo_elfpak(
        &project,
        &[
            "bundle",
            "--oci-archive",
            archive.to_str().unwrap(),
            "--install",
            "/app/oci-fixture",
            "--image-tag",
            "ci-test",
            "--entrypoint",
            "/app/oci-fixture",
            "--dry-run",
            "--no-config",
        ],
    );

    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("built Cargo binary:"), "{text}");
    assert!(text.contains("oci archive:"), "{text}");
    assert!(!archive.exists());
}

#[test]
fn package_selector_disambiguates_a_virtual_workspace() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(
        workspace.join("Cargo.toml"),
        "[workspace]\nresolver = \"3\"\nmembers = [\"api\", \"worker\"]\n",
    )
    .unwrap();
    write_binary_project(&workspace.join("api"), "api", "fn main() {}\n");
    write_binary_project(&workspace.join("worker"), "worker", "fn main() {}\n");
    let rootfs = tmp.path().join("rootfs");

    let ambiguous = cargo_elfpak(&workspace, &dry_run_args(&rootfs));
    assert!(!ambiguous.status.success());
    let error = stderr(&ambiguous);
    assert!(error.contains("--package"), "{error}");
    assert!(error.contains("api, worker"), "{error}");

    let selected = cargo_elfpak(
        &workspace,
        &[
            "elfpak",
            "bundle",
            "-p",
            "worker",
            "--output",
            rootfs.to_str().unwrap(),
            "--dry-run",
            "--no-config",
        ],
    );
    assert!(selected.status.success(), "{}", stderr(&selected));
    let output = stdout(&selected);
    assert!(output.contains("worker"), "{output}");
    assert!(output.contains("rootfs:"), "{output}");
}

#[test]
fn bin_selector_is_required_when_no_default_can_be_inferred() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("tools");
    std::fs::create_dir_all(project.join("src/bin")).unwrap();
    std::fs::write(
        project.join("Cargo.toml"),
        "[package]\nname = \"tools\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    std::fs::write(project.join("src/bin/first.rs"), "fn main() {}\n").unwrap();
    std::fs::write(project.join("src/bin/second.rs"), "fn main() {}\n").unwrap();
    let rootfs = tmp.path().join("rootfs");

    let ambiguous = cargo_elfpak(&project, &dry_run_args(&rootfs));
    assert!(!ambiguous.status.success());
    let error = stderr(&ambiguous);
    assert!(error.contains("--bin"), "{error}");
    assert!(error.contains("first, second"), "{error}");

    let selected = cargo_elfpak(
        &project,
        &[
            "bundle",
            "--bin",
            "second",
            "--output",
            rootfs.to_str().unwrap(),
            "--dry-run",
            "--no-config",
        ],
    );
    assert!(selected.status.success(), "{}", stderr(&selected));
    assert!(
        stdout(&selected).contains("second"),
        "{}",
        stdout(&selected)
    );
}

#[test]
fn cargo_build_options_are_forwarded_to_metadata_and_build() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    let rootfs = tmp.path().join("rootfs");
    let target_dir = tmp.path().join("custom-target");
    let target = host_target();
    write_binary_project(&project, "options-fixture", "fn main() {}\n");
    let manifest = project.join("Cargo.toml");
    std::fs::write(
        &manifest,
        "[package]\nname = \"options-fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[features]\nspecial = []\n",
    )
    .unwrap();
    let lock = Command::new("cargo")
        .args(["generate-lockfile", "--offline"])
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(lock.status.success(), "{}", stderr(&lock));

    let output = cargo_elfpak(
        tmp.path(),
        &[
            "bundle",
            "--manifest-path",
            manifest.to_str().unwrap(),
            "--profile",
            "dev",
            "--target-dir",
            target_dir.to_str().unwrap(),
            "--target",
            &target,
            "--features",
            "special",
            "--no-default-features",
            "--locked",
            "--offline",
            "--frozen",
            "--output",
            rootfs.to_str().unwrap(),
            "--dry-run",
            "--no-config",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.contains(&format!("custom-target/{target}/debug/options-fixture")),
        "{text}"
    );
}

#[test]
fn release_profile_is_forwarded() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    let rootfs = tmp.path().join("rootfs");
    let target_dir = tmp.path().join("custom-target");
    write_binary_project(&project, "release-fixture", "fn main() {}\n");

    let output = cargo_elfpak(
        &project,
        &[
            "bundle",
            "--release",
            "--target-dir",
            target_dir.to_str().unwrap(),
            "--output",
            rootfs.to_str().unwrap(),
            "--dry-run",
            "--no-config",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("custom-target/release/release-fixture"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn release_and_profile_conflict() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    write_binary_project(&project, "conflict-fixture", "fn main() {}\n");

    let output = cargo_elfpak(
        &project,
        &["bundle", "--release", "--profile", "dev", "--dry-run"],
    );
    assert!(!output.status.success());
    let error = stderr(&output);
    assert!(error.contains("cannot be used with"), "{error}");
}

#[test]
fn positional_binary_is_rejected_in_favor_of_bin_selection() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    let rootfs = tmp.path().join("rootfs");
    write_binary_project(&project, "selected-fixture", "fn main() {}\n");

    let output = cargo_elfpak(
        &project,
        &[
            "bundle",
            "/ignored/binary",
            "--output",
            rootfs.to_str().unwrap(),
            "--dry-run",
            "--no-config",
        ],
    );
    assert!(!output.status.success());
    let error = stderr(&output);
    assert!(error.contains("unexpected argument"), "{error}");
}

#[test]
fn quiet_suppresses_cargo_warnings_and_bundle_output() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    let rootfs = tmp.path().join("rootfs");
    write_binary_project(&project, "quiet-fixture", "fn main() { let unused = 1; }\n");

    let output = cargo_elfpak(
        &project,
        &[
            "bundle",
            "--quiet",
            "--output",
            rootfs.to_str().unwrap(),
            "--dry-run",
            "--no-config",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).is_empty(), "{}", stdout(&output));
    assert!(stderr(&output).is_empty(), "{}", stderr(&output));
}

#[test]
fn quiet_still_reports_cargo_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    let rootfs = tmp.path().join("rootfs");
    write_binary_project(&project, "error-fixture", "fn main( {\n");

    let output = cargo_elfpak(
        &project,
        &[
            "bundle",
            "--quiet",
            "--output",
            rootfs.to_str().unwrap(),
            "--dry-run",
            "--no-config",
        ],
    );
    assert!(!output.status.success());
    assert!(stdout(&output).is_empty(), "{}", stdout(&output));
    let error = stderr(&output);
    assert!(error.contains("src/main.rs"), "{error}");
    assert!(error.contains("Cargo build failed"), "{error}");
}
