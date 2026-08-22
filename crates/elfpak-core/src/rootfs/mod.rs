//! Root filesystem materialization backends.

pub mod archive;
pub mod copy;

pub use archive::{TarBuilder, TarReport};
pub use copy::{RootFsBuilder, RootFsReport};
