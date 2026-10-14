//! Portable Markdown interchange. Parsing produces inert content; callers
//! validate and commit through the engine's prepared fragment boundary.
pub use editac_core::commands::editor::{FragmentBlock, FragmentRun};
pub mod export;
pub mod extensions;
pub mod import;
pub mod optional;
pub mod profile;
pub mod reconcile;

/// Stable v1 codec facade. Hooks are trusted host code, never document data.
pub mod v1 {
    pub use crate::export::{ExportResult, export};
    pub use crate::extensions::{
        Extension, export as export_with_extensions, import as import_with_extensions,
    };
    pub use crate::import::{ImportError, PreparedContent};
    pub use crate::optional::{Codecs, import_with_codecs};
    pub use crate::{FragmentBlock, FragmentRun};
    pub const EXTENSION_API_VERSION: u32 = editac_core::modules::EXTENSION_API_VERSION;
}
