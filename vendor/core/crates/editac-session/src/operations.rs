//! Bounded endpoint-local sideband provenance, never a durable/authenticated ledger.
use crate::{ports::LocalCommit, revision::ObservedRevision};
use std::collections::VecDeque;

pub const MAX_LABEL_BYTES: usize = 128;
pub const MAX_OPERATION_RECORDS: usize = 256;
pub const MAX_OPERATION_BYTES: usize = 262_144;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperationOrigin {
    Human,
    OtherHuman,
    Agent,
    Import,
    Replay,
    Unattributed,
    Undo,
    Redo,
}

/// Construct only after the host has authenticated its current principal. This
/// constructor does not authenticate credentials; requests must never supply it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostActor(String);
impl HostActor {
    pub fn authenticated_by_host(subject: &str) -> Result<Self, OperationError> {
        label(subject)?;
        Ok(Self(subject.into()))
    }
    pub fn subject(&self) -> &str {
        &self.0
    }
}

/// Passed by the trusted host at apply, independently of prepared client input.
#[derive(Clone, Debug)]
pub struct HostOperation {
    pub actor: Option<HostActor>,
    pub origin: OperationOrigin,
    pub request_id: String,
}
impl HostOperation {
    pub(crate) fn validate(&self) -> Result<(), OperationError> {
        label(&self.request_id)
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OperationError {
    InvalidLabel,
    Exhausted,
}
fn label(value: &str) -> Result<(), OperationError> {
    if value.is_empty() || value.len() > MAX_LABEL_BYTES || value.chars().any(char::is_control) {
        Err(OperationError::InvalidLabel)
    } else {
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub enum OperationOutcome {
    LocalCommit(LocalCommit),
    /// Semantic mutation occurred but local acknowledgment failed. Recovery is required.
    RecoveryRequired,
}
#[derive(Clone, Debug)]
pub struct OperationRecord {
    /// Unique only within this service lifetime, scoped by the before/after document tokens.
    pub operation_id: u64,
    /// Correlation only: repeated request IDs do not grant idempotency or authority.
    pub request_id: String,
    pub actor: Option<HostActor>,
    pub origin: OperationOrigin,
    /// Untrusted engine routing tag, never promoted to an actor.
    pub engine_origin: String,
    pub before: ObservedRevision,
    pub after: ObservedRevision,
    pub outcome: OperationOutcome,
}
impl OperationRecord {
    fn bytes(&self) -> usize {
        // Fixed-size fields, tokens, outcome and owned string capacities; allocator
        // bookkeeping is additional but bounded by the record-count cap.
        let outcome_bytes = match &self.outcome {
            OperationOutcome::LocalCommit(c) => {
                let id = &c.update.identity;
                [
                    &id.document_id,
                    &id.epoch,
                    &id.schema,
                    &id.profile,
                    &id.offsets,
                ]
                .iter()
                .map(|s| s.capacity())
                .sum::<usize>()
            }
            OperationOutcome::RecoveryRequired => 0,
        };
        std::mem::size_of::<Self>()
            + self.request_id.capacity()
            + self.engine_origin.capacity()
            + self.actor.as_ref().map_or(0, |a| a.0.capacity())
            + outcome_bytes
    }
}
#[derive(Default)]
pub(crate) struct OperationLog {
    records: VecDeque<OperationRecord>,
    bytes: usize,
    next: u64,
}
impl OperationLog {
    pub fn records(&self) -> &VecDeque<OperationRecord> {
        &self.records
    }
    pub fn bytes(&self) -> usize {
        self.bytes
    }
    pub fn next(&self) -> Result<u64, OperationError> {
        self.next.checked_add(1).ok_or(OperationError::Exhausted)
    }
    pub fn push(&mut self, record: OperationRecord) {
        self.next = record.operation_id;
        self.bytes += record.bytes();
        self.records.push_back(record);
        while self.records.len() > MAX_OPERATION_RECORDS || self.bytes > MAX_OPERATION_BYTES {
            self.bytes -= self.records.pop_front().unwrap().bytes();
        }
    }
}
