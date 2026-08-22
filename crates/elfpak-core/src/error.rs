//! Structured errors for `elfpak`.
//!
//! Every variant carries a stable diagnostic code so that the CLI can render
//! `error[E2001]`-style messages and scripts can match on them.

use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io error on `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("`{path}` is not a valid ELF object: {message}")]
    Elf { path: PathBuf, message: String },

    #[error("`{path}` is not an ELF file")]
    NotElf { path: PathBuf },

    #[error(
        "`{path}` targets an unsupported architecture: {architecture} (e_machine = {machine:#x})"
    )]
    UnsupportedArchitecture {
        path: PathBuf,
        architecture: String,
        machine: u16,
    },

    #[error("unable to resolve shared library `{soname}`")]
    UnresolvedLibrary {
        soname: String,
        required_by: PathBuf,
        searched: Vec<PathBuf>,
    },

    #[error("library `{soname}` is not allowed by dependency policy")]
    DisallowedLibrary {
        soname: String,
        required_by: PathBuf,
    },

    #[error("resolved library `{soname}` has an incompatible architecture")]
    IncompatibleArchitecture {
        soname: String,
        expected: String,
        found: PathBuf,
        found_architecture: String,
    },

    #[error("runtime policy feature `{feature}` could not be satisfied")]
    MissingRuntimeFile {
        feature: &'static str,
        searched: Vec<PathBuf>,
    },

    #[error("{resource} exceeds the supported limit of {limit}")]
    LimitExceeded {
        resource: &'static str,
        limit: usize,
    },

    #[error(
        "source file `{path}` changed after it was added to the bundle plan (expected {expected_size} bytes with sha256 {expected_digest}, found {actual_size} bytes with sha256 {actual_digest})"
    )]
    SourceChanged {
        path: PathBuf,
        expected_digest: String,
        expected_size: u64,
        actual_digest: String,
        actual_size: u64,
    },

    #[error("path `{path}` escapes the {kind} root")]
    PathEscape { path: PathBuf, kind: &'static str },

    #[error("`{path}` does not exist inside the source root")]
    MissingSourcePath { path: PathBuf },

    #[error("too many levels of symbolic links while resolving `{path}`")]
    SymlinkLoop { path: PathBuf },

    #[error("invalid configuration: {message}")]
    Config { message: String },

    #[error("invalid manifest `{path}`: {message}")]
    Manifest { path: PathBuf, message: String },

    #[error("verification failed: {failures} problem(s) across {checked} manifest entries")]
    VerifyFailed { checked: u32, failures: u32 },
}

impl Error {
    /// Stable diagnostic code, rendered as `error[E1001]` by the CLI.
    ///
    /// The codes live in [`crate::diagnostics`], next to the warning codes, so
    /// that the whole namespace is visible in one place and checked there.
    pub fn code(&self) -> &'static str {
        use crate::diagnostics::error as code;
        match self {
            Error::Io { .. } => code::IO,
            Error::Elf { .. } => code::ELF,
            Error::NotElf { .. } => code::NOT_ELF,
            Error::UnsupportedArchitecture { .. } => code::UNSUPPORTED_ARCHITECTURE,
            Error::LimitExceeded { .. } => code::LIMIT_EXCEEDED,
            Error::SourceChanged { .. } => code::SOURCE_CHANGED,
            Error::UnresolvedLibrary { .. } => code::UNRESOLVED_LIBRARY,
            Error::DisallowedLibrary { .. } => code::DISALLOWED_LIBRARY,
            Error::IncompatibleArchitecture { .. } => code::INCOMPATIBLE_ARCHITECTURE,
            Error::MissingRuntimeFile { .. } => code::MISSING_RUNTIME_FILE,
            Error::PathEscape { .. } => code::PATH_ESCAPE,
            Error::MissingSourcePath { .. } => code::MISSING_SOURCE_PATH,
            Error::SymlinkLoop { .. } => code::SYMLINK_LOOP,
            Error::Config { .. } => code::CONFIG,
            Error::Manifest { .. } => code::MANIFEST,
            Error::VerifyFailed { .. } => code::VERIFY_FAILED,
        }
    }

    /// Extra context printed underneath the headline message, if there is any.
    pub fn details(&self) -> Vec<String> {
        match self {
            Error::UnresolvedLibrary {
                required_by,
                searched,
                ..
            } => {
                let mut out = vec![format!("required by:\n  {}", required_by.display())];
                if !searched.is_empty() {
                    let list = searched
                        .iter()
                        .map(|p| format!("  {}", p.display()))
                        .collect::<Vec<_>>()
                        .join("\n");
                    out.push(format!("searched:\n{list}"));
                }
                out
            }
            Error::DisallowedLibrary {
                soname,
                required_by,
            } => vec![
                format!("required by:\n  {}", required_by.display()),
                format!("add:\n  --allow-library {soname}"),
            ],
            Error::IncompatibleArchitecture {
                soname,
                expected,
                found,
                found_architecture,
            } => vec![
                format!("requested:\n  {soname} for {expected}"),
                format!("found:\n  {} ({found_architecture})", found.display()),
            ],
            Error::UnsupportedArchitecture { .. } => {
                vec!["supported:\n  x86_64\n  aarch64".to_string()]
            }
            Error::MissingRuntimeFile { searched, .. } if !searched.is_empty() => {
                let list = searched
                    .iter()
                    .map(|p| format!("  {}", p.display()))
                    .collect::<Vec<_>>()
                    .join("\n");
                vec![format!("searched:\n{list}")]
            }
            _ => Vec::new(),
        }
    }
}

pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Error {
    Error::Io {
        path: path.into(),
        source,
    }
}
