//! Shared editor integration for the local-first client.
//!
//! The engine owns committed state. Markdown prepares inert content and source
//! edits for that engine; a source buffer is never a second writable replica.
//! Local command orchestration reuses these APIs without attaching a transport.

pub use editac_core as engine;
pub use editac_markdown as markdown;

/// Immutable upstream source consumed by both crates (bound by vendor/core/provenance.json).
pub const EDITOR_REVISION: &str = "c592e5f639fb104c5f1dfbb767647f99e1839e2e";

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

/// Explicit saved-credential port; no ambient credential discovery.
pub mod auth;

/// Authorized hosted bootstrap and native offline cache acquisition.
pub mod hosted;

pub mod status;
/// Native explicit synchronization and offline durability reporting.
pub mod sync;

pub mod client_commands;
pub mod host_adapter;

/// Hosted external-editor workflow over the existing native boundaries.
pub mod hosted_edit;

/// Terminal presenter acquisition over the qualified shared session owner.
pub mod tui;
