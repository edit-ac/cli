//! Platform-independent edit.ac engine primitives.
//!
//! The protocol factory creates a single UTF-16 Yrs replica. Document admission
//! and initialization use the frozen registry schema; semantic commands and
//! persistence ports are subsequent gates.

pub mod anchors;
pub mod clipboard;
pub mod commands;
pub mod document;
pub mod history;
pub mod import;
pub mod modules;
mod ownership;
pub mod patches;
// One faithful v4 replay implementation shared with the preserved qualification
// fixtures. Production never uses its fixed seed/envelope or fixture admission.
mod editor;
#[path = "../../../fixtures/schema-v4/mapping.rs"]
#[allow(dead_code)]
mod editor_mapping;
pub mod projection;
#[path = "../../../fixtures/schema-v5/mapping.rs"]
#[allow(dead_code)]
mod stable_editor_mapping;

/// Create a fresh, independently editing replica for the candidate Yjs 13 profile.
///
/// Yrs generates a new client identity; callers must not restore a persisted
/// writer identity into multiple live replicas. Root types and document/epoch
/// metadata remain subject to the foundation schema gate.
pub fn new_document() -> yrs::Doc {
    yrs::Doc::with_options(yrs::Options {
        offset_kind: yrs::OffsetKind::Utf16,
        ..yrs::Options::default()
    })
}

/// An offset measured in UTF-16 code units at a platform boundary.
///
/// This value alone does not identify a document, prove a valid text boundary,
/// or guarantee a grapheme boundary. It must not be retained as a stable anchor
/// across edits. The owning text operation will need to validate and convert it.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Utf16Offset(pub u32);

#[doc(hidden)]
pub mod interchange;
