//! Immutable output-plan data exposed to callers.

use crate::{
    elf::Architecture,
    graph::{DependencyGraph, Digest},
    policy::{DependencyPolicy, Preset, RuntimeFeature, RuntimePolicy},
};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PlannedFileKind {
    Directory,
    Symlink,
    Executable,
    Interpreter,
    SharedObject,
    CertificateBundle,
    RuntimeConfig,
    ApplicationData,
}

impl PlannedFileKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            PlannedFileKind::Directory => "directory",
            PlannedFileKind::Symlink => "symlink",
            PlannedFileKind::Executable => "executable",
            PlannedFileKind::Interpreter => "interpreter",
            PlannedFileKind::SharedObject => "shared-object",
            PlannedFileKind::CertificateBundle => "certificate-bundle",
            PlannedFileKind::RuntimeConfig => "runtime-config",
            PlannedFileKind::ApplicationData => "application-data",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InclusionReason {
    Application,
    Interpreter,
    NeededBy { binary: PathBuf, soname: String },
    RuntimePolicy { feature: RuntimeFeature },
    ExplicitInclude,
}

/// One entry of the output rootfs. Nothing is written that is not planned here.
#[derive(Debug, Clone)]
pub struct PlannedFile {
    /// Host path to copy from, if the content comes from the source root.
    pub(crate) source: Option<PathBuf>,
    /// Absolute path inside the generated rootfs.
    pub(crate) destination: PathBuf,
    pub(crate) kind: PlannedFileKind,
    pub(crate) reason: InclusionReason,
    pub(crate) mode: u32,
    pub(crate) size: u64,
    pub(crate) sha256: Option<Digest>,
    /// Verbatim link target for [`PlannedFileKind::Symlink`].
    pub(crate) link_target: Option<PathBuf>,
    /// Generated content (passwd, nsswitch.conf, ...).
    pub(crate) content: Option<Vec<u8>>,
}

impl PlannedFile {
    pub fn source(&self) -> Option<&Path> {
        self.source.as_deref()
    }

    pub fn destination(&self) -> &Path {
        &self.destination
    }

    pub fn kind(&self) -> PlannedFileKind {
        self.kind
    }

    pub fn reason(&self) -> &InclusionReason {
        &self.reason
    }

    pub fn mode(&self) -> u32 {
        self.mode
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn sha256(&self) -> Option<&Digest> {
        self.sha256.as_ref()
    }

    pub fn link_target(&self) -> Option<&Path> {
        self.link_target.as_deref()
    }

    pub fn content(&self) -> Option<&[u8]> {
        self.content.as_deref()
    }

    /// Invariants every entry holds, checked when it enters a plan and again
    /// before it is written.
    pub(crate) fn assert_well_formed(&self) {
        assert!(self.destination.is_absolute());
        assert!(self.mode <= 0o7777);

        match self.kind {
            PlannedFileKind::Directory => {
                assert!(self.source.is_none());
                assert!(self.link_target.is_none());
                assert!(self.content.is_none());
                assert!(self.sha256.is_none());
                assert_eq!(self.size, 0);
            }
            PlannedFileKind::Symlink => {
                assert!(self.source.is_none());
                assert!(self.link_target.is_some());
                assert!(self.content.is_none());
                assert!(self.sha256.is_none());
                assert_eq!(self.size, 0);
            }
            _ => {
                assert!(self.link_target.is_none());
                assert_ne!(self.source.is_some(), self.content.is_some());
                assert!(self.sha256.as_ref().is_some_and(Digest::is_well_formed));
                if let Some(content) = &self.content {
                    assert_eq!(self.size, content.len() as u64);
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct ApplicationPlan {
    pub(crate) executable: PlannedFile,
    pub(crate) graph: DependencyGraph,
    /// `PT_INTERP` as declared by this executable.
    pub(crate) interpreter: Option<PathBuf>,
    /// Where that interpreter lives after following symlinks.
    pub(crate) interpreter_resolved: Option<PathBuf>,
}

impl ApplicationPlan {
    pub fn executable(&self) -> &PlannedFile {
        &self.executable
    }

    pub fn graph(&self) -> &DependencyGraph {
        &self.graph
    }

    pub fn interpreter(&self) -> Option<&Path> {
        self.interpreter.as_deref()
    }

    pub fn interpreter_resolved(&self) -> Option<&Path> {
        self.interpreter_resolved.as_deref()
    }
}

#[derive(Debug, Clone)]
pub struct BundlePlan {
    pub(crate) applications: Vec<ApplicationPlan>,
    /// All entries, including the executable, sorted by destination.
    pub(crate) files: Vec<PlannedFile>,
    pub(crate) architecture: Architecture,
    /// Preset the policy was derived from, when one was named.
    pub(crate) preset: Option<Preset>,
    /// Runtime policy this plan was built with, recorded for the manifest.
    pub(crate) runtime_policy: RuntimePolicy,
    pub(crate) dependency_policy: DependencyPolicy,
    pub(crate) warnings: Vec<Warning>,
}

impl BundlePlan {
    /// The first application, retained for singular callers.
    pub fn executable(&self) -> &PlannedFile {
        self.applications
            .first()
            .expect("a bundle plan always contains an application")
            .executable()
    }

    pub fn applications(&self) -> &[ApplicationPlan] {
        &self.applications
    }

    pub fn executables(&self) -> impl Iterator<Item = &PlannedFile> {
        self.applications.iter().map(ApplicationPlan::executable)
    }

    pub fn files(&self) -> &[PlannedFile] {
        &self.files
    }

    /// The first dependency graph, retained for singular callers.
    pub fn graph(&self) -> &DependencyGraph {
        self.applications
            .first()
            .expect("a bundle plan always contains an application")
            .graph()
    }

    pub fn architecture(&self) -> Architecture {
        self.architecture
    }

    pub fn preset(&self) -> Option<Preset> {
        self.preset
    }

    pub fn runtime_policy(&self) -> &RuntimePolicy {
        &self.runtime_policy
    }

    pub fn dependency_policy(&self) -> &DependencyPolicy {
        &self.dependency_policy
    }

    /// The first application's interpreter, retained for singular callers.
    pub fn interpreter(&self) -> Option<&Path> {
        self.applications
            .first()
            .and_then(ApplicationPlan::interpreter)
    }

    /// The first application's resolved interpreter, retained for singular callers.
    pub fn interpreter_resolved(&self) -> Option<&Path> {
        self.applications
            .first()
            .and_then(ApplicationPlan::interpreter_resolved)
    }

    pub fn warnings(&self) -> &[Warning] {
        &self.warnings
    }

    pub fn total_size(&self) -> u64 {
        self.files.iter().map(|file| file.size).sum()
    }

    pub fn files_of_kind(&self, kind: PlannedFileKind) -> impl Iterator<Item = &PlannedFile> {
        self.files.iter().filter(move |file| file.kind == kind)
    }
}

#[derive(Debug, Clone)]
pub struct Warning {
    pub code: &'static str,
    pub message: String,
    pub details: Vec<String>,
}
