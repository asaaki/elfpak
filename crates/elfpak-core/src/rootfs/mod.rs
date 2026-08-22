//! Root filesystem materialization backends.

pub mod archive;
pub mod copy;

pub use archive::{TarBuilder, TarReport};
pub use copy::{RootFsBuilder, RootFsReport};
pub(crate) use copy::{
    ensure_directory, guard_output, output_parent, path_exists, publish_directory,
};
