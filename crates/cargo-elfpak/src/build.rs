use crate::metadata::Selection;
use anyhow::{Context, Result, bail};
use cargo_metadata::Message;
use std::{
    ffi::OsString,
    io::{BufReader, Read},
    path::{Path, PathBuf},
    process::{ChildStderr, Command, Stdio},
    thread,
};

const CARGO_STDERR_MAX_BYTES: usize = 1024 * 1024;

#[derive(Debug)]
pub(crate) struct BuildRequest<'a> {
    pub(crate) selection: &'a Selection,
    pub(crate) current_dir: &'a Path,
    pub(crate) manifest_path: Option<&'a Path>,
    pub(crate) release: bool,
    pub(crate) profile: Option<&'a str>,
    pub(crate) target: Option<&'a str>,
    pub(crate) target_dir: Option<&'a Path>,
    pub(crate) features: &'a [String],
    pub(crate) all_features: bool,
    pub(crate) no_default_features: bool,
    pub(crate) locked: bool,
    pub(crate) offline: bool,
    pub(crate) frozen: bool,
    pub(crate) quiet: bool,
}

#[derive(Debug)]
pub(crate) struct BuildArtifact {
    pub(crate) executable: PathBuf,
    pub(crate) fresh: bool,
}

pub(crate) fn run(request: &BuildRequest<'_>) -> Result<BuildArtifact> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let mut command = Command::new(&cargo);
    command
        .current_dir(request.current_dir)
        .args([
            "build",
            "--message-format=json-render-diagnostics",
            "--package",
            &request.selection.package_name,
            "--bin",
            &request.selection.binary_name,
        ])
        .stdout(Stdio::piped());
    append_path_option(&mut command, "--manifest-path", request.manifest_path);
    append_flag(&mut command, "--release", request.release);
    append_value_option(&mut command, "--profile", request.profile);
    append_value_option(&mut command, "--target", request.target);
    append_path_option(&mut command, "--target-dir", request.target_dir);
    for features in request.features {
        command.args(["--features", features]);
    }
    append_flag(&mut command, "--all-features", request.all_features);
    append_flag(
        &mut command,
        "--no-default-features",
        request.no_default_features,
    );
    append_flag(&mut command, "--locked", request.locked);
    append_flag(&mut command, "--offline", request.offline);
    append_flag(&mut command, "--frozen", request.frozen);
    append_flag(&mut command, "--quiet", request.quiet);
    if request.quiet {
        command.stderr(Stdio::piped());
    }

    let mut child = command
        .spawn()
        .with_context(|| format!("could not run `{}`", cargo.to_string_lossy()))?;
    let stdout = child.stdout.take().context("could not read Cargo output")?;
    let stderr_reader = child.stderr.take().map(read_stderr);
    let mut artifacts = Vec::new();

    for message in Message::parse_stream(BufReader::new(stdout)) {
        match message.context("could not parse Cargo build output")? {
            Message::CompilerArtifact(artifact)
                if artifact.package_id == request.selection.package_id
                    && artifact.target.name == request.selection.binary_name
                    && artifact.target.is_bin() =>
            {
                if let Some(executable) = artifact.executable {
                    artifacts.push(BuildArtifact {
                        executable: executable.into_std_path_buf(),
                        fresh: artifact.fresh,
                    });
                }
            }
            _ => {}
        }
    }

    let status = child.wait().context("could not wait for Cargo build")?;
    let stderr = match stderr_reader {
        Some(reader) => reader
            .join()
            .map_err(|_| anyhow::anyhow!("Cargo stderr reader panicked"))??,
        None => CapturedStderr::default(),
    };
    if !status.success() {
        eprint!("{}", String::from_utf8_lossy(&stderr.bytes));
        if stderr.truncated {
            eprintln!("Cargo stderr was truncated after {CARGO_STDERR_MAX_BYTES} bytes");
        }
        bail!("Cargo build failed with status {status}");
    }
    match artifacts.len() {
        1 => Ok(artifacts.remove(0)),
        0 => bail!(
            "Cargo produced no executable artifact for package `{}` binary `{}`",
            request.selection.package_name,
            request.selection.binary_name
        ),
        count => bail!(
            "Cargo produced {count} executable artifacts for package `{}` binary `{}`",
            request.selection.package_name,
            request.selection.binary_name
        ),
    }
}

#[derive(Default)]
struct CapturedStderr {
    bytes: Vec<u8>,
    truncated: bool,
}

fn read_stderr(stderr: ChildStderr) -> thread::JoinHandle<std::io::Result<CapturedStderr>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let limit = u64::try_from(CARGO_STDERR_MAX_BYTES).expect("stderr limit fits u64");
        let mut limited = stderr.take(limit + 1);
        limited.read_to_end(&mut bytes)?;
        let truncated = bytes.len() > CARGO_STDERR_MAX_BYTES;
        if truncated {
            bytes.truncate(CARGO_STDERR_MAX_BYTES);
        }
        std::io::copy(&mut limited.into_inner(), &mut std::io::sink())?;
        Ok(CapturedStderr { bytes, truncated })
    })
}

fn append_flag(command: &mut Command, flag: &str, enabled: bool) {
    if enabled {
        command.arg(flag);
    }
}

fn append_value_option(command: &mut Command, flag: &str, value: Option<&str>) {
    if let Some(value) = value {
        command.args([flag, value]);
    }
}

fn append_path_option(command: &mut Command, flag: &str, value: Option<&Path>) {
    if let Some(value) = value {
        command.arg(flag).arg(value);
    }
}
