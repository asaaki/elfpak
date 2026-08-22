//! ELF analysis, dependency resolution, bundle planning, and rootfs output.

pub mod diagnostics;
pub mod elf;
pub mod error;
pub mod graph;
pub mod hash;
pub mod manifest;
pub mod paths;
pub mod plan;
pub mod policy;
pub mod resolver;
pub mod rootfs;
pub mod source;

pub use elf::{Architecture, ElfClass, ElfMetadata, Endianness, Machine, ObjectType};
pub use error::{Error, Result};
pub use graph::{DependencyGraph, DependencyReason, Digest, Node, NodeKind};
pub use manifest::{Manifest, VerifyReport};
pub use plan::{
    ApplicationPlan, BundlePlan, InclusionReason, PlannedFile, PlannedFileKind, Planner, Warning,
};
pub use policy::{CachePolicy, DependencyPolicy, Preset, RuntimeFeature, RuntimePolicy, UserSpec};
pub use resolver::{DynamicLinkerResolver, LdCache, LibraryRequest, Resolver};
pub use rootfs::{RootFsBuilder, RootFsReport, TarBuilder, TarReport};
pub use source::SourceRoot;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
