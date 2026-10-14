//! Offline status. Native durability and application receipts are independent.
use crate::{
    hosted::HostedSession,
    storage::{JournalEntry, StorageError},
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ConnectionState {
    #[default]
    Offline,
    Connected,
    RetryExhausted,
    AuthorizationFailure,
    Revoked,
    IncompatibleSchemaEpoch,
    InvalidProtocol,
    LocalPersistenceFailure,
    ReceiptPersistenceFailure,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub locally_saved: bool,
    pub pending_upload: usize,
    pub remote_committed_updates: usize,
    pub remote_durability: &'static str,
    pub unresolved_draft: bool,
    pub conflict: bool,
    pub connection: ConnectionState,
}
pub fn uploadable(entry: &JournalEntry) -> bool {
    // Native received records are recovery-only and must never echo as local uploads.
    // Older conservatively journaled entries remain pending until exact receipts.
    entry.origin != "editac:remote:sync"
}
pub fn inspect(session: &HostedSession) -> Result<Status, StorageError> {
    let recovery = session.storage.recover()?;
    let pending = recovery.pending.iter().filter(|e| uploadable(e)).count();
    let committed = session.storage.committed_receipts()?;
    let locally_saved = session
        .storage
        .durability_barrier(&session.recovery.document, &crate::storage::NativeIo)?;
    let unresolved_draft = recovery.draft.is_some() || recovery.rejected_draft.is_some();
    let conflict = recovery.draft.as_ref().is_some_and(|draft| {
        session.source_baseline().is_ok_and(|base| {
            let current = &base.opening().source;
            (draft.baseline != *current && draft.source != *current)
                || base
                    .prepare(&session.recovery.document, &draft.source)
                    .is_err()
        })
    });
    Ok(Status {
        locally_saved,
        pending_upload: pending,
        remote_committed_updates: committed,
        remote_durability: if locally_saved && pending == 0 && committed > 0 {
            "remotely-committed"
        } else {
            "unknown"
        },
        unresolved_draft,
        conflict,
        connection: ConnectionState::Offline,
    })
}
