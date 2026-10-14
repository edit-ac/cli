//! Shared editor integration for the local-first client.
//!
//! The engine owns committed state. Markdown prepares inert content and source
//! edits for that engine; a source buffer is never a second writable replica.
//! Including these APIs does not enable document commands in the binary.

pub use editac_core as engine;
pub use editac_markdown as markdown;

/// Immutable upstream source consumed by both crates (also pinned in Cargo.lock).
pub const EDITOR_REVISION: &str = "5b3e96ec259f1cdd8df2971b2192c9cacf225a65";

/// Offline local source acquisition, before any editor or mutation boundary.
pub mod target;

/// Recoverable external-editor sessions; successful changes still need preparation.
pub mod editor;

/// Exact-source preparation over the shared engine; does not publish files.
pub mod reconcile;
