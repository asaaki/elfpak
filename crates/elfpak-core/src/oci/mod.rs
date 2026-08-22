//! OCI image layout and archive output.

mod archive;
mod layout;
mod model;

pub use archive::OciArchiveBuilder;
pub use layout::{OciLayoutBuilder, OciReport};
pub use model::{OciImageConfig, ResolvedImageConfig};
