//! `elfpak-core`: analyze a Linux ELF application, resolve its runtime closure
//! the way the glibc loader would, and plan a minimal rootfs for it.
//!
//! Guarantees held by this crate:
//!
//! * the target binary is never executed
//! * `ldd`, `ldconfig` and shell commands are never invoked
//! * no network access
//! * the source root is only ever read
//! * every output file carries a recorded reason

pub mod config;
pub mod elf;
pub mod error;
pub mod graph;
pub mod hash;
pub mod manifest;
pub mod paths;
pub mod plan;
pub mod resolver;
pub mod rootfs;
pub mod source;

pub use config::Config;
pub use elf::{Architecture, ElfClass, ElfMetadata, Endianness, Machine, ObjectType};
pub use error::{Error, Result};
pub use graph::{DependencyGraph, DependencyReason, Digest, Node, NodeKind};
pub use manifest::{Manifest, VerifyReport};
pub use plan::{BundlePlan, InclusionReason, PlannedFile, PlannedFileKind, Planner, Warning};
pub use resolver::{DynamicLinkerResolver, LdCache, LibraryRequest, Resolver};
pub use rootfs::{
    DependencyPolicy, Preset, RootFsBuilder, RootFsReport, RuntimeFeature, RuntimePolicy, UserSpec,
};
pub use source::SourceRoot;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
