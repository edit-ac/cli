pub mod content;
pub mod evolution;
mod inert_content;
pub mod registry;
mod rich_table;

/// Public library API contract version, independent of stored module data versions.
pub const EXTENSION_API_VERSION: u32 = 1;

/// Stable v1 facade over the qualified portable implementations.
pub mod v1 {
    pub use super::evolution::{EvolutionError, ModuleEvolution, PreparedModuleMigration};
    pub use super::registry::{Definition, FrozenRegistry, ModuleKind, Registry, RegistryError};
    pub use crate::commands::editor::{
        EditorCommand, EditorCommit, EditorError, FragmentBlock, FragmentRun, PreparedEditorEdit,
    };
}
