//! Shared editor integration for the local-first client.
//!
//! The engine owns committed state. Markdown prepares inert content and source
//! edits for that engine; a source buffer is never a second writable replica.
//! Local command orchestration reuses these APIs without attaching a transport.

pub use editac_core as engine;
pub use editac_markdown as markdown;

/// Immutable upstream source consumed by both crates (bound by vendor/core/provenance.json).
pub const EDITOR_REVISION: &str = "1a57fa527b28862865ca64c1fec170103d456af0";

/// Offline local source acquisition, before any editor or mutation boundary.
pub mod target;

/// Recoverable external-editor sessions; successful changes still need preparation.
pub mod editor;

/// Exact-source preparation over the shared engine; does not publish files.
pub mod reconcile;

/// Guarded native publication; semantic commit alone never acknowledges a save.
pub mod files;

/// Native checkpoint/journal/outbox and independent source-draft recovery.
pub mod storage;

/// Bounded secondary interchange operations; no transport or remote receipt.
pub mod diagnostics;

/// Actual local editing and explicit recovery command orchestration.
pub mod local;
