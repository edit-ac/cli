//! Portable local-first companion to the semantic engine.
//! Locator/catalog resolution and prepared store/transport ports are available.
//! Local-first lifecycle and optional native persistence are available.
pub mod locator;
pub mod ports;

#[cfg(feature = "native-store")]
pub mod store;

pub mod session;

pub mod local_channel;

pub mod edit_service;
pub mod revision;

pub mod operations;

pub mod history_store;

pub mod review_apply;
