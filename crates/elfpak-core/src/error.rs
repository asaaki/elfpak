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

    #[error("`{path}` targets an unsupported architecture (e_machine = {machine:#x})")]
    UnsupportedArchitecture { path: PathBuf, machine: u16 },

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
    VerifyFailed { checked: usize, failures: usize },
}

impl Error {
    /// Stable diagnostic code, rendered as `error[E1001]` by the CLI.
    pub fn code(&self) -> &'static str {
        match self {
            Error::Io { .. } => "E1000",
            Error::Elf { .. } => "E1001",
            Error::NotElf { .. } => "E1002",
            Error::UnsupportedArchitecture { .. } => "E1003",
            Error::UnresolvedLibrary { .. } => "E2001",
            Error::DisallowedLibrary { .. } => "E2002",
            Error::IncompatibleArchitecture { .. } => "E2003",
            Error::MissingRuntimeFile { .. } => "E2004",
            Error::PathEscape { .. } => "E3001",
            Error::MissingSourcePath { .. } => "E3002",
            Error::SymlinkLoop { .. } => "E3003",
            Error::Config { .. } => "E4001",
            Error::Manifest { .. } => "E4002",
            Error::VerifyFailed { .. } => "E5001",
        }
    }

    /// Extra context printed underneath the headline message.
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
